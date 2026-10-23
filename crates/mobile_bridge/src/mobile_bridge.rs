//! Lets the mobile web app control Zed. Zed spawns the `mobile_server`
//! executable (which serves the PWA, authenticates devices and sends push
//! notifications) and answers the API requests it relays from the PWA. See
//! `api.rs` for the API and the `mobile_protocol` crate for the stdio
//! envelope.

mod api;
mod buffers;
mod status_bar;
mod terminals;
mod threads;
mod workspace_items;

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _, io::BufReader,
};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, Hsla, Task, WeakEntity,
    WindowHandle, actions,
};
use mobile_protocol::{ClientId, HostMessage, ServerMessage};
use rand::Rng as _;
use serde::Serialize;
use settings::{RegisterSetting, Settings, SettingsStore};
use std::path::PathBuf;
use theme::ActiveTheme as _;
use util::ResultExt as _;
use workspace::{MultiWorkspace, Toast, Workspace, notifications::NotificationId};

pub use status_bar::MobileNotificationsIndicator;

use crate::api::{
    Appearance, IncomingRequest, OutgoingEvent, OutgoingResponse, Request, ResponseOutcome,
    SyntaxStyle, Theme, WorkspaceId, events,
};

actions!(
    mobile_server,
    [
        /// Shows a code for pairing a phone with the mobile web app.
        PairDevice,
        /// Revokes access for every paired phone.
        UnpairAllDevices,
    ]
);

const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIRING_CODE_TTL_SECONDS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, RegisterSetting)]
pub struct MobileServerSettings {
    pub enabled: bool,
    pub address: String,
    pub port: u16,
    pub always_notify: bool,
}

impl Settings for MobileServerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let mobile_server = content.mobile_server.clone().unwrap_or_default();
        Self {
            enabled: mobile_server.enabled.unwrap_or(false),
            address: mobile_server
                .address
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: mobile_server.port.unwrap_or(7878),
            always_notify: mobile_server.always_notify.unwrap_or(false),
        }
    }
}

struct GlobalMobileBridge(Entity<MobileBridge>);

impl Global for GlobalMobileBridge {}

pub fn init(cx: &mut App) {
    let bridge = cx.new(MobileBridge::new);
    cx.set_global(GlobalMobileBridge(bridge));

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &PairDevice, _window, cx| {
            MobileBridge::start_pairing(workspace, cx);
        });
        workspace.register_action(|workspace, _: &UnpairAllDevices, _window, cx| {
            MobileBridge::unpair_all_devices(workspace, cx);
        });
        if let Some(window) = window {
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    MobileBridge::mark_desktop_interaction(cx);
                }
            })
            .detach();
        }
    })
    .detach();
    cx.observe_keystrokes(|_, _, cx| MobileBridge::mark_desktop_interaction(cx))
        .detach();
}

pub struct MobileBridge {
    running_settings: Option<MobileServerSettings>,
    server: Option<RunningServer>,
    clients: HashMap<ClientId, Client>,
    /// The workspace that requested pairing, so the outcome can be shown there.
    pairing_workspace: Option<WeakEntity<Workspace>>,
    /// Whether the phone, rather than the desktop, was the last thing used
    /// to interact with Zed. Pushes are only sent in that case unless
    /// `always_notify` is on.
    last_interaction_from_mobile: bool,
    pub(crate) threads: threads::ThreadsState,
    pub(crate) buffers: buffers::BuffersState,
    pub(crate) terminals: terminals::TerminalsState,
    pub(crate) workspace_items: workspace_items::WorkspaceItemsState,
    _settings_subscription: gpui::Subscription,
    _theme_subscription: gpui::Subscription,
}

struct RunningServer {
    outgoing: futures::channel::mpsc::UnboundedSender<HostMessage>,
    listening_address: Option<String>,
    _io_task: Task<()>,
}

pub(crate) struct Client {
    pub device_name: String,
}

impl MobileBridge {
    fn new(cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_server_with_settings(cx);
        });
        let theme_subscription = cx.observe_global::<theme::GlobalTheme>(|this, cx| {
            if !this.clients.is_empty() {
                this.broadcast_event(events::THEME, &build_theme(cx));
            }
        });
        let mut this = Self {
            running_settings: None,
            server: None,
            clients: HashMap::default(),
            pairing_workspace: None,
            last_interaction_from_mobile: false,
            threads: threads::ThreadsState::default(),
            buffers: buffers::BuffersState::default(),
            terminals: terminals::TerminalsState::default(),
            workspace_items: workspace_items::WorkspaceItemsState::default(),
            _settings_subscription: settings_subscription,
            _theme_subscription: theme_subscription,
        };
        this.threads.init(cx);
        this.sync_server_with_settings(cx);
        this
    }

    fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalMobileBridge>()
            .map(|global| global.0.clone())
    }

    fn sync_server_with_settings(&mut self, cx: &mut Context<Self>) {
        let settings = MobileServerSettings::get_global(cx).clone();
        let desired = settings.enabled.then_some(settings);
        if desired == self.running_settings {
            return;
        }
        self.stop_server(cx);
        self.running_settings = desired.clone();
        if let Some(settings) = desired {
            match self.start_server(&settings, cx) {
                Ok(server) => self.server = Some(server),
                Err(error) => {
                    log::error!("failed to start the mobile server: {error:#}");
                }
            }
        }
    }

    fn start_server(
        &mut self,
        settings: &MobileServerSettings,
        cx: &mut Context<Self>,
    ) -> Result<RunningServer> {
        let binary_path = server_binary_path()?;
        let state_dir = paths::data_dir().join("mobile_server");
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;

        let mut child = util::command::new_command(&binary_path)
            .arg(mobile_protocol::ADDRESS_FLAG)
            .arg(&settings.address)
            .arg(mobile_protocol::PORT_FLAG)
            .arg(settings.port.to_string())
            .arg(mobile_protocol::STATE_DIR_FLAG)
            .arg(&state_dir)
            .stdin(util::command::Stdio::piped())
            .stdout(util::command::Stdio::piped())
            .stderr(util::command::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", binary_path.display()))?;
        log::info!(
            "started the mobile server ({}) on {}:{}",
            binary_path.display(),
            settings.address,
            settings.port
        );

        let mut stdin = child.stdin.take().context("missing stdin")?;
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let (outgoing_tx, mut outgoing_rx) = futures::channel::mpsc::unbounded::<HostMessage>();
        let (incoming_tx, mut incoming_rx) = futures::channel::mpsc::unbounded::<ServerMessage>();

        let writer = cx.background_spawn(async move {
            while let Some(message) = outgoing_rx.next().await {
                let line = mobile_protocol::encode(&message)?;
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
            anyhow::Ok(())
        });
        let reader = cx.background_spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next().await {
                let line = line?;
                match mobile_protocol::decode::<ServerMessage>(&line) {
                    Ok(message) => {
                        if incoming_tx.unbounded_send(message).is_err() {
                            break;
                        }
                    }
                    Err(error) => log::error!("invalid message from the mobile server: {error}"),
                }
            }
            anyhow::Ok(())
        });
        let logger = cx.background_spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                log::info!("mobile_server: {line}");
            }
        });

        let io_task = cx.spawn(async move |this, cx| {
            let process = cx.background_spawn(async move {
                let (writer_result, reader_result) = futures::join!(writer, reader);
                writer_result.log_err();
                reader_result.log_err();
                logger.await;
                child.status().await
            });
            while let Some(message) = incoming_rx.next().await {
                if this
                    .update(cx, |this, cx| this.handle_server_message(message, cx))
                    .is_err()
                {
                    return;
                }
            }
            match process.await {
                Ok(status) => log::info!("the mobile server exited with {status}"),
                Err(error) => log::error!("failed to wait for the mobile server: {error}"),
            }
            this.update(cx, |this, cx| this.handle_server_exited(cx))
                .log_err();
        });

        Ok(RunningServer {
            outgoing: outgoing_tx,
            listening_address: None,
            _io_task: io_task,
        })
    }

    fn stop_server(&mut self, cx: &mut Context<Self>) {
        // Dropping the server closes its stdin, which makes it exit; `kill_on_drop` covers the rest.
        if self.server.take().is_some() {
            self.disconnect_all_clients(cx);
        }
    }

    fn handle_server_exited(&mut self, cx: &mut Context<Self>) {
        self.server = None;
        self.disconnect_all_clients(cx);
    }

    fn disconnect_all_clients(&mut self, cx: &mut Context<Self>) {
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.handle_client_disconnected(client_id, cx);
        }
    }

    fn send_to_server(&self, message: HostMessage) -> Result<()> {
        let server = self
            .server
            .as_ref()
            .context("the mobile server isn't running")?;
        server
            .outgoing
            .unbounded_send(message)
            .map_err(|_| anyhow!("the mobile server exited"))
    }

    fn handle_server_message(&mut self, message: ServerMessage, cx: &mut Context<Self>) {
        match message {
            ServerMessage::Listening { address } => {
                log::info!("the mobile server is listening on {address}");
                if let Some(server) = self.server.as_mut() {
                    server.listening_address = Some(address);
                }
            }
            ServerMessage::Failed { message } => {
                log::error!("the mobile server failed to start: {message}");
            }
            ServerMessage::ClientConnected {
                client_id,
                device_name,
            } => {
                log::info!("mobile client {client_id} connected from {device_name}");
                self.clients.insert(client_id, Client { device_name });
                let hello = api::Hello {
                    client_id: client_id.to_string(),
                    zed_version: release_channel::AppVersion::global(cx).to_string(),
                    theme: build_theme(cx),
                };
                self.send_event(client_id, events::HELLO, &hello);
            }
            ServerMessage::ClientDisconnected { client_id } => {
                self.handle_client_disconnected(client_id, cx);
            }
            ServerMessage::Received { client_id, message } => {
                self.handle_client_message(client_id, &message, cx);
            }
            ServerMessage::DevicePaired { device_name } => {
                self.show_pairing_outcome(format!("Paired {device_name} with Zed."), cx);
            }
            ServerMessage::PairingFailed { reason } => {
                self.show_pairing_outcome(format!("Pairing failed: {reason}"), cx);
            }
        }
    }

    fn handle_client_disconnected(&mut self, client_id: ClientId, cx: &mut Context<Self>) {
        if let Some(client) = self.clients.remove(&client_id) {
            log::info!(
                "mobile client {client_id} ({}) disconnected",
                client.device_name
            );
        }
        self.threads.remove_client(client_id);
        self.buffers.remove_client(client_id, cx);
        self.terminals.remove_client(client_id);
        self.workspace_items.remove_client(client_id);
    }

    fn mark_desktop_interaction(cx: &mut App) {
        // Runs on every keystroke, so skip the update (and its observers)
        // when nothing changes.
        if let Some(bridge) = Self::global(cx)
            && bridge.read(cx).last_interaction_from_mobile
        {
            bridge.update(cx, |bridge, _| bridge.last_interaction_from_mobile = false);
        }
    }

    fn handle_client_message(&mut self, client_id: ClientId, message: &str, cx: &mut Context<Self>) {
        if !self.clients.contains_key(&client_id) {
            return;
        }
        self.last_interaction_from_mobile = true;
        let request = match serde_json::from_str::<IncomingRequest>(message) {
            Ok(request) => request,
            Err(error) => {
                #[derive(serde::Deserialize)]
                struct RequestId {
                    id: u64,
                }
                match serde_json::from_str::<RequestId>(message) {
                    Ok(RequestId { id }) => {
                        self.send_response(client_id, id, Err(anyhow!("invalid request: {error}")));
                    }
                    Err(_) => log::error!("invalid message from mobile client: {error}"),
                }
                return;
            }
        };
        let request_id = request.id;
        let mut task = self.handle_request(client_id, request.request, cx);
        // Answer synchronous requests right away, so the response can't be
        // overtaken by events it triggers (e.g. `buffer.updates` for a buffer
        // the client only learns about from this response).
        let waker = futures::task::noop_waker();
        if let std::task::Poll::Ready(result) =
            task.poll_unpin(&mut std::task::Context::from_waker(&waker))
        {
            self.send_response(client_id, request_id, result);
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, _| {
                this.send_response(client_id, request_id, result);
            })
            .ok();
        })
        .detach();
    }

    fn handle_request(
        &mut self,
        client_id: ClientId,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        match request {
            Request::ListThreads {} => self.list_threads(client_id, cx),
            Request::NewThreadOptions {} => self.new_thread_options(cx),
            Request::CreateThread(params) => self.create_thread(params, cx),
            Request::OpenThread { thread_id } => self.open_thread(client_id, thread_id, cx),
            Request::CloseThread { thread_id } => {
                self.threads.unsubscribe(client_id, &thread_id);
                Task::ready(Ok(serde_json::Value::Null))
            }
            Request::SendMessage { thread_id, text } => self.send_message(&thread_id, text, cx),
            Request::CancelGeneration { thread_id } => self.cancel_generation(&thread_id, cx),
            Request::AuthorizeToolCall {
                thread_id,
                tool_call_id,
                option_id,
            } => self.authorize_tool_call(&thread_id, tool_call_id, option_id, cx),
            Request::SetThreadControl {
                thread_id,
                control_id,
                value,
            } => self.set_thread_control(&thread_id, &control_id, value, cx),
            Request::RemoveQueuedMessage { thread_id, index } => {
                self.remove_queued_message(&thread_id, index, cx)
            }
            Request::WorkspaceItems { workspace_id } => {
                self.workspace_items(client_id, workspace_id, cx)
            }
            Request::CloseItem {
                workspace_id,
                item_id,
            } => self.close_item(workspace_id, item_id, cx),
            Request::NewTerminal { workspace_id } => self.new_terminal(workspace_id, cx),
            Request::ListDirectory {
                workspace_id,
                worktree_id,
                path,
            } => self.list_directory(workspace_id, worktree_id, &path, cx),
            Request::OpenFile {
                workspace_id,
                worktree_id,
                path,
            } => self.open_file(workspace_id, worktree_id, &path, cx),
            Request::GitLog {
                workspace_id,
                offset,
                limit,
            } => self.git_log(workspace_id, offset, limit, cx),
            Request::OpenUncommittedChanges { workspace_id } => {
                self.open_uncommitted_changes(workspace_id, cx)
            }
            Request::OpenCommit { workspace_id, sha } => self.open_commit(workspace_id, sha, cx),
            Request::StagePaths {
                workspace_id,
                repository_id,
                paths,
            } => self.set_paths_staged(workspace_id, repository_id, paths, true, cx),
            Request::UnstagePaths {
                workspace_id,
                repository_id,
                paths,
            } => self.set_paths_staged(workspace_id, repository_id, paths, false, cx),
            Request::OpenItem {
                workspace_id,
                item_id,
            } => self.open_item(client_id, workspace_id, item_id, cx),
            Request::CloseItemView {
                workspace_id,
                item_id,
            } => {
                self.close_item_view(client_id, workspace_id, item_id, cx);
                Task::ready(Ok(serde_json::Value::Null))
            }
            Request::PushBufferUpdates {
                buffer,
                version,
                updates,
            } => self.push_buffer_updates(client_id, buffer, version, updates, cx),
            Request::BufferHighlights { buffer, from, to } => {
                self.buffer_highlights(buffer, from, to, cx)
            }
            Request::Completions {
                buffer,
                version,
                offset,
            } => self.completions(client_id, buffer, version, offset, cx),
            Request::ApplyCompletion {
                buffer,
                completion_id,
            } => self.apply_completion(client_id, buffer, completion_id, cx),
            Request::Hover {
                buffer,
                version,
                offset,
            } => self.hover(buffer, version, offset, cx),
            Request::GoToDefinition {
                workspace_id,
                buffer,
                version,
                offset,
            } => self.go_to_definition(workspace_id, buffer, version, offset, cx),
            Request::FormatBuffer { buffer } => self.format_buffer(buffer, cx),
            Request::SaveBuffer { buffer } => self.save_buffer(buffer, cx),
            Request::OpenTerminal {
                workspace_id,
                item_id,
            } => self.open_terminal(client_id, workspace_id, item_id, cx),
            Request::CloseTerminal {
                workspace_id,
                item_id,
            } => {
                self.terminals
                    .unsubscribe(client_id, workspace_id, item_id);
                Task::ready(Ok(serde_json::Value::Null))
            }
            Request::TerminalInput {
                workspace_id,
                item_id,
                data,
            } => self.terminal_input(workspace_id, item_id, data, cx),
            Request::TerminalScroll {
                workspace_id,
                item_id,
                lines,
            } => self.terminal_scroll(workspace_id, item_id, lines, cx),
        }
    }

    fn send_response(&self, client_id: ClientId, request_id: u64, result: Result<serde_json::Value>) {
        let outcome = match result {
            Ok(value) => ResponseOutcome::Result(value),
            Err(error) => ResponseOutcome::Error(format!("{error:#}")),
        };
        let response = OutgoingResponse {
            id: request_id,
            outcome,
        };
        match serde_json::to_string(&response) {
            Ok(message) => self.send_message_to_client(Some(client_id), message),
            Err(error) => log::error!("failed to serialize a mobile response: {error}"),
        }
    }

    pub(crate) fn send_event<T: Serialize>(&self, client_id: ClientId, event: &str, data: &T) {
        self.send_event_to(Some(client_id), event, data);
    }

    pub(crate) fn broadcast_event<T: Serialize>(&self, event: &str, data: &T) {
        self.send_event_to(None, event, data);
    }

    fn send_event_to<T: Serialize>(&self, client_id: Option<ClientId>, event: &str, data: &T) {
        match serde_json::to_string(&OutgoingEvent { event, data }) {
            Ok(message) => self.send_message_to_client(client_id, message),
            Err(error) => log::error!("failed to serialize mobile event {event}: {error}"),
        }
    }

    fn send_message_to_client(&self, client_id: Option<ClientId>, message: String) {
        if self.server.is_none() {
            return;
        }
        self.send_to_server(HostMessage::Send { client_id, message })
            .log_err();
    }

    pub(crate) fn push_notification(
        &self,
        title: String,
        body: String,
        tag: Option<String>,
        url: Option<String>,
    ) {
        if self.server.is_none() {
            return;
        }
        self.send_to_server(HostMessage::Notify {
            title,
            body,
            tag,
            url,
        })
        .log_err();
    }

    fn start_pairing(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        struct PairingToast;

        let Some(bridge) = Self::global(cx) else {
            return;
        };
        let code = generate_pairing_code();
        let workspace_handle = cx.entity().downgrade();
        let result = bridge.update(cx, |bridge, _| {
            bridge.pairing_workspace = Some(workspace_handle);
            let listening_address = bridge
                .server
                .as_ref()
                .and_then(|server| server.listening_address.clone());
            bridge
                .send_to_server(HostMessage::StartPairing {
                    code: code.replace('-', ""),
                    expires_in_seconds: PAIRING_CODE_TTL_SECONDS,
                })
                .map(|()| listening_address)
        });
        let message = match result {
            Ok(listening_address) => {
                let address = listening_address
                    .map(|address| format!(" (listening on {address})"))
                    .unwrap_or_default();
                format!(
                    "Enter pairing code {code} in the Zed mobile app{address}. \
                     It expires in {} minutes.",
                    PAIRING_CODE_TTL_SECONDS / 60
                )
            }
            Err(_) => {
                "The mobile server isn't running. Set `mobile_server.enabled` to true in your settings."
                    .to_string()
            }
        };
        workspace.show_toast(
            Toast::new(NotificationId::unique::<PairingToast>(), message),
            cx,
        );
    }

    fn unpair_all_devices(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        struct UnpairToast;

        let Some(bridge) = Self::global(cx) else {
            return;
        };
        let message = match bridge.read(cx).send_to_server(HostMessage::UnpairAllDevices) {
            Ok(()) => "Unpaired all devices from the Zed mobile app.".to_string(),
            Err(error) => format!("Failed to unpair devices: {error}"),
        };
        workspace.show_toast(
            Toast::new(NotificationId::unique::<UnpairToast>(), message),
            cx,
        );
    }

    fn show_pairing_outcome(&mut self, message: String, cx: &mut Context<Self>) {
        struct PairingOutcomeToast;

        let Some(workspace) = self
            .pairing_workspace
            .take()
            .and_then(|workspace| workspace.upgrade())
        else {
            log::info!("{message}");
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<PairingOutcomeToast>(), message),
                cx,
            );
        });
    }
}

fn generate_pairing_code() -> String {
    let mut rng = rand::rng();
    let characters: String = (0..8)
        .map(|_| {
            let index = rng.random_range(0..PAIRING_CODE_ALPHABET.len());
            char::from(PAIRING_CODE_ALPHABET[index])
        })
        .collect();
    format!("{}-{}", &characters[..4], &characters[4..])
}

fn server_binary_path() -> Result<PathBuf> {
    let executable_name = format!("mobile_server{}", std::env::consts::EXE_SUFFIX);
    let current_exe = std::env::current_exe().context("locating the Zed executable")?;
    let directory = current_exe
        .parent()
        .context("the Zed executable has no parent directory")?;
    let candidates = [
        directory.join(&executable_name),
        directory.join("tools").join(&executable_name),
        directory.join("..").join("libexec").join(&executable_name),
    ];
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .with_context(|| {
            format!(
                "couldn't find {executable_name} next to {}",
                current_exe.display()
            )
        })
}

/// Every window's multi-workspace, frontmost first.
pub(crate) fn multi_workspace_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    let mut windows = cx
        .window_stack()
        .unwrap_or_else(|| cx.windows())
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect::<Vec<_>>();
    for window in cx.windows() {
        if let Some(window) = window.downcast::<MultiWorkspace>()
            && !windows.contains(&window)
        {
            windows.push(window);
        }
    }
    windows
}

pub(crate) fn find_workspace(
    workspace_id: WorkspaceId,
    cx: &App,
) -> Result<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let entity_id = EntityId::from(workspace_id);
    for window in multi_workspace_windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        if let Some(workspace) = multi_workspace
            .workspaces()
            .find(|workspace| workspace.entity_id() == entity_id)
        {
            return Ok((window, workspace.clone()));
        }
    }
    Err(anyhow!("the workspace is no longer open"))
}

pub(crate) fn active_workspace(cx: &App) -> Result<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    multi_workspace_windows(cx)
        .into_iter()
        .find_map(|window| {
            let workspace = window.read(cx).ok()?.workspace().clone();
            Some((window, workspace))
        })
        .context("no Zed window is open")
}

pub(crate) fn to_json<T: Serialize>(value: T) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(value)?)
}

pub(crate) fn css_color(color: Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b),
        channel(rgba.a)
    )
}

fn build_theme(cx: &App) -> Theme {
    let theme = cx.theme();
    let colors = theme.colors();
    let status = theme.status();
    let local_player = theme.players().local();
    let mut color_map = std::collections::BTreeMap::new();
    let mut insert = |name: &str, color: Hsla| {
        color_map.insert(name.to_string(), css_color(color));
    };
    insert("background", colors.background);
    insert("surface", colors.surface_background);
    insert("elevated_surface", colors.elevated_surface_background);
    insert("border", colors.border);
    insert("border_variant", colors.border_variant);
    insert("text", colors.text);
    insert("text_muted", colors.text_muted);
    insert("text_placeholder", colors.text_placeholder);
    insert("text_accent", colors.text_accent);
    insert("accent", colors.text_accent);
    insert("element_background", colors.element_background);
    insert("element_hover", colors.element_hover);
    insert("element_selected", colors.element_selected);
    insert("error", status.error);
    insert("warning", status.warning);
    insert("success", status.success);
    insert("info", status.info);
    insert("editor_background", colors.editor_background);
    insert("editor_foreground", colors.editor_foreground);
    insert("editor_line_number", colors.editor_line_number);
    insert("editor_active_line_number", colors.editor_active_line_number);
    insert("editor_active_line", colors.editor_active_line_background);
    insert("editor_selection", local_player.selection);
    insert("editor_cursor", local_player.cursor);
    insert("diff_added", status.created);
    insert("diff_deleted", status.deleted);
    insert("diff_modified", status.modified);
    insert("diff_added_background", status.created_background);
    insert("diff_deleted_background", status.deleted_background);
    insert("terminal_background", colors.terminal_background);
    insert("terminal_foreground", colors.terminal_foreground);
    insert("terminal_cursor", local_player.cursor);

    let syntax_theme = theme.syntax();
    let syntax = (0..)
        .map_while(|index: usize| syntax_theme.get(index))
        .map(|style| SyntaxStyle {
            color: style.color.map(css_color),
            background: style.background_color.map(css_color),
            italic: style.font_style == Some(gpui::FontStyle::Italic),
            bold: style
                .font_weight
                .is_some_and(|weight| weight >= gpui::FontWeight::SEMIBOLD),
            underline: style.underline.is_some(),
            strikethrough: style.strikethrough.is_some(),
        })
        .collect();

    let terminal_ansi = (0..16)
        .map(|index| css_color(terminal::get_color_at_index(index, theme)))
        .collect();

    let theme_settings = theme_settings::ThemeSettings::get_global(cx);
    Theme {
        appearance: match theme.appearance() {
            theme::Appearance::Light => Appearance::Light,
            theme::Appearance::Dark => Appearance::Dark,
        },
        colors: color_map,
        syntax,
        terminal_ansi,
        buffer_font_family: theme_settings.buffer_font.family.to_string(),
        ui_font_family: theme_settings.ui_font.family.to_string(),
    }
}

/// Clients subscribed to some resource, used by the feature modules.
pub(crate) type Subscribers = HashSet<ClientId>;
