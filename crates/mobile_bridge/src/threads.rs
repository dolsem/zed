use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use acp_thread::AcpThreadEvent;
use agent_ui::mobile::{self as agent_mobile, MobileConversation};
use agent_ui::{AgentNotificationInfo, AgentNotificationObserver, AgentPanel, ThreadId};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EntityId, Subscription, Task, WindowHandle};
use mobile_protocol::rest::{self, thread_events};
use mobile_protocol::{ClientId, RequestId};
use settings::Settings as _;
use sidebar::mobile::{MobileThreadId, find_or_open_workspace, threads_snapshot};
use util::path_list::PathList;
use workspace::{MultiWorkspace, ProjectGroupKey, Workspace};

use crate::api::{
    self, AgentOption, ControlValue, CreateThreadParams, CreatedThread, NewThreadOptions,
    NewThreadProject, ProjectThreads, ThreadEntriesRemovedEvent, ThreadEntryEvent, ThreadKind,
    ThreadSnapshot, ThreadSummary, ThreadsList, WorktreeChoice, WorktreeOption, events,
};
use crate::{MobileBridge, Subscribers, WebUiSettings, multi_workspace_windows, to_json};

const THREADS_LIST_DEBOUNCE: Duration = Duration::from_millis(250);
const THREAD_FLUSH_INTERVAL: Duration = Duration::from_millis(80);
const THREAD_LOAD_TIMEOUT: Duration = Duration::from_secs(90);
const THREAD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct ThreadsState {
    list_subscribers: Subscribers,
    last_list: Option<ThreadsList>,
    list_refresh_task: Option<Task<()>>,
    observed_sidebars: HashMap<EntityId, Subscription>,
    threads: HashMap<String, ThreadSubscription>,
    _global_subscriptions: Vec<Subscription>,
}

struct ThreadSubscription {
    /// Web UI clients with the thread open.
    subscribers: Subscribers,
    /// HTTP API event streams, and whether they want every run item.
    streams: HashMap<RequestId, bool>,
    conversation: MobileConversation,
    thread_view_id: Option<EntityId>,
    entry_count: usize,
    last_state: Option<api::ThreadState>,
    /// The runs as last sent to `streams`, without items.
    last_runs: Vec<rest::Run>,
    /// Set when entries were removed, so streams need a fresh snapshot.
    streams_need_snapshot: bool,
    dirty_entries: BTreeSet<usize>,
    state_dirty: bool,
    flush_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ThreadSubscription {
    fn is_unused(&self) -> bool {
        self.subscribers.is_empty() && self.streams.is_empty()
    }
}

impl ThreadsState {
    pub fn init(&mut self, cx: &mut Context<MobileBridge>) {
        if let Some(store) = agent_ui::thread_metadata_store::ThreadMetadataStore::try_global(cx) {
            self._global_subscriptions
                .push(cx.observe(&store, |this, _, cx| this.schedule_list_refresh(cx)));
        }
        if let Some(store) =
            agent_ui::terminal_thread_metadata_store::TerminalThreadMetadataStore::try_global(cx)
        {
            self._global_subscriptions
                .push(cx.observe(&store, |this, _, cx| this.schedule_list_refresh(cx)));
        }

        // Sidebars rebuild whenever live thread state changes (status, titles,
        // diff stats), which the metadata stores alone don't reflect.
        let bridge = cx.weak_entity();
        cx.observe_new::<sidebar::Sidebar>(move |_, _window, cx| {
            let sidebar = cx.entity();
            bridge
                .update(cx, |bridge, cx| bridge.observe_sidebar(sidebar, cx))
                .ok();
        })
        .detach();
        for window in multi_workspace_windows(cx) {
            if let Some(sidebar) = window
                .read(cx)
                .ok()
                .and_then(|multi_workspace| multi_workspace.sidebar())
                .and_then(|sidebar| sidebar.to_any().downcast::<sidebar::Sidebar>().ok())
            {
                let subscription =
                    cx.observe(&sidebar, |this, _, cx| this.schedule_list_refresh(cx));
                self.observed_sidebars
                    .insert(sidebar.entity_id(), subscription);
            }
        }

        let bridge = cx.weak_entity();
        cx.set_global(AgentNotificationObserver(Rc::new(
            move |info: &AgentNotificationInfo, cx: &mut App| {
                bridge
                    .update(cx, |bridge, cx| bridge.forward_agent_notification(info, cx))
                    .ok();
            },
        )));
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.list_subscribers.remove(&client_id);
        if self.list_subscribers.is_empty() {
            self.last_list = None;
        }
        self.threads.retain(|_, subscription| {
            subscription.subscribers.remove(&client_id);
            !subscription.is_unused()
        });
    }

    pub fn unsubscribe(&mut self, client_id: ClientId, thread_id: &str) {
        if let Some(subscription) = self.threads.get_mut(thread_id) {
            subscription.subscribers.remove(&client_id);
            if subscription.is_unused() {
                self.threads.remove(thread_id);
            }
        }
    }

    pub fn remove_stream(&mut self, request_id: RequestId, thread_id: &str) {
        if let Some(subscription) = self.threads.get_mut(thread_id) {
            subscription.streams.remove(&request_id);
            if subscription.is_unused() {
                self.threads.remove(thread_id);
            }
        }
    }
}

impl MobileBridge {
    fn observe_sidebar(&mut self, sidebar: Entity<sidebar::Sidebar>, cx: &mut Context<Self>) {
        let sidebar_id = sidebar.entity_id();
        let subscription = cx.observe(&sidebar, |this, _, cx| this.schedule_list_refresh(cx));
        self.threads
            .observed_sidebars
            .insert(sidebar_id, subscription);
        cx.observe_release(&sidebar, move |this, _, _| {
            this.threads.observed_sidebars.remove(&sidebar_id);
        })
        .detach();
    }

    fn schedule_list_refresh(&mut self, cx: &mut Context<Self>) {
        if self.threads.list_subscribers.is_empty() || self.threads.list_refresh_task.is_some() {
            return;
        }
        self.threads.list_refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(THREADS_LIST_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.threads.list_refresh_task = None;
                let list = build_threads_list(cx);
                if this.threads.last_list.as_ref() != Some(&list) {
                    for client_id in &this.threads.list_subscribers {
                        this.send_event(*client_id, events::THREADS_CHANGED, &list);
                    }
                    this.threads.last_list = Some(list);
                }
            })
            .ok();
        }));
    }

    fn forward_agent_notification(&self, info: &AgentNotificationInfo, cx: &App) {
        // Same rule as Zed's own popups: nothing while the thread is on screen.
        if info.visible_on_desktop {
            return;
        }
        if !WebUiSettings::get_global(cx).always_notify && !self.last_interaction_from_mobile {
            return;
        }
        let thread_id = info.thread_id.to_key_string();
        // Someone looking at the thread on their phone doesn't need a push.
        if self
            .threads
            .threads
            .get(&thread_id)
            .is_some_and(|subscription| !subscription.subscribers.is_empty())
        {
            return;
        }
        self.push_notification(
            info.title.to_string(),
            info.caption.to_string(),
            Some(thread_id.clone()),
            Some(format!("#/thread/{thread_id}")),
        );
    }

    pub(crate) fn list_threads(
        &mut self,
        client_id: ClientId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        self.threads.list_subscribers.insert(client_id);
        let list = build_threads_list(cx);
        self.threads.last_list = Some(list.clone());
        Task::ready(to_json(list))
    }

    pub(crate) fn new_thread_options(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let options = build_new_thread_options(cx);
        cx.background_spawn(async move { to_json(options.await?) })
    }

    /// Loads the thread in Zed if needed, without changing what the user
    /// sees, and waits until its agent is ready (or failed to start).
    pub(crate) fn load_conversation(
        &mut self,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<MobileConversation>> {
        let parsed_thread_id = match ThreadId::from_key_string(thread_id) {
            Ok(thread_id) => thread_id,
            Err(error) => return Task::ready(Err(error.context("invalid thread id"))),
        };
        let load = if agent_mobile::find_conversation(parsed_thread_id, cx).is_none() {
            Some(sidebar::mobile::load_thread_in_background(
                parsed_thread_id,
                cx,
            ))
        } else {
            None
        };
        cx.spawn(async move |_, cx| {
            if let Some(load) = load {
                load.await?;
            }
            wait_for_conversation(parsed_thread_id, cx).await
        })
    }
}

/// Waits until the thread's conversation is open and its session loaded.
pub(crate) async fn wait_for_conversation(
    thread_id: ThreadId,
    cx: &mut gpui::AsyncApp,
) -> Result<MobileConversation> {
    let deadline = cx.background_executor().now() + THREAD_LOAD_TIMEOUT;
    loop {
        let conversation = cx.update(|cx| {
            let conversation = agent_mobile::find_conversation(thread_id, cx)?;
            if conversation.thread(cx).is_some() {
                return Some(Ok(conversation));
            }
            agent_mobile::conversation_error(&conversation, cx).map(|error| Err(anyhow!(error)))
        });
        if let Some(conversation) = conversation {
            return conversation;
        }
        if cx.background_executor().now() >= deadline {
            return Err(anyhow!("timed out loading the thread"));
        }
        cx.background_executor()
            .timer(THREAD_LOAD_POLL_INTERVAL)
            .await;
    }
}

pub(crate) fn build_new_thread_options(cx: &mut App) -> Task<Result<NewThreadOptions>> {
    let mut projects = Vec::new();
    let mut branch_tasks = Vec::new();
    for window in multi_workspace_windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for group in sidebar::mobile::project_groups(multi_workspace, cx) {
            if group.key.path_list().paths().is_empty() {
                continue;
            }
            let mut worktrees = Vec::new();
            let mut repository = None;
            for workspace in &group.workspaces {
                let project = workspace.read(cx).project().read(cx);
                for candidate in project.repositories(cx).values() {
                    let snapshot = candidate.read(cx).snapshot();
                    let is_root =
                        group.key.path_list().paths().iter().any(|path| {
                            path.as_path() == snapshot.work_directory_abs_path.as_ref()
                        });
                    if !is_root {
                        continue;
                    }
                    repository.get_or_insert_with(|| candidate.clone());
                    push_worktree_option(
                        &mut worktrees,
                        snapshot.work_directory_abs_path.to_path_buf(),
                        snapshot
                            .branch
                            .as_ref()
                            .map(|branch| branch.name().to_string()),
                        true,
                    );
                    for linked_worktree in snapshot.linked_worktrees() {
                        push_worktree_option(
                            &mut worktrees,
                            linked_worktree.path.clone(),
                            linked_worktree.branch_name().map(str::to_string),
                            false,
                        );
                    }
                }
            }
            let current_branch = worktrees
                .iter()
                .find(|worktree| worktree.is_main)
                .and_then(|worktree| worktree.branch.clone());
            if let Some(repository) = &repository {
                branch_tasks.push((
                    projects.len(),
                    repository.update(cx, |repository, _| repository.branches()),
                ));
            }
            projects.push(NewThreadProject {
                project_id: project_id(&group.key),
                name: String::new(),
                remote_host: group.key.host().map(|host| host.display_name()),
                worktrees,
                // Projects that aren't open have no repository state yet; a
                // worktree can be created once they're opened in the background.
                can_create_worktree: repository.is_some()
                    || (group.workspaces.is_empty()
                        && group.key.host().is_none()
                        && group
                            .key
                            .path_list()
                            .paths()
                            .iter()
                            .any(|path| path.join(".git").exists())),
                branches: current_branch.into_iter().collect(),
            });
        }
    }
    let names: HashMap<String, String> = threads_snapshot(cx)
        .into_iter()
        .map(|project| (project_id(&project.key), project.name.to_string()))
        .collect();
    for project in &mut projects {
        if let Some(name) = names.get(&project.project_id) {
            project.name = name.clone();
        }
    }

    let mut agents = vec![AgentOption {
        id: agent::ZED_AGENT_ID.to_string(),
        name: "Zed Agent".to_string(),
        is_external: false,
    }];
    let mut default_agent_id = agent::ZED_AGENT_ID.to_string();
    if let Ok((_, workspace)) = crate::active_workspace(cx) {
        let project = workspace.read(cx).project().read(cx);
        let store = project.agent_server_store().read(cx);
        for agent_id in store.external_agents() {
            agents.push(AgentOption {
                id: agent_id.0.to_string(),
                name: store
                    .agent_display_name(agent_id)
                    .unwrap_or_else(|| agent_id.0.clone())
                    .to_string(),
                is_external: true,
            });
        }
        if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            default_agent_id = panel.read(cx).selected_agent(cx).id().0.to_string();
        }
    }

    cx.background_spawn(async move {
        for (index, branches) in branch_tasks {
            let Ok(Ok(result)) = branches.await else {
                continue;
            };
            if let Some(project) = projects.get_mut(index) {
                for branch in result.branches {
                    if branch.is_remote() {
                        continue;
                    }
                    let name = branch.name().to_string();
                    if !project.branches.contains(&name) {
                        project.branches.push(name);
                    }
                }
            }
        }
        Ok(NewThreadOptions {
            projects,
            agents,
            default_agent_id,
        })
    })
}

impl MobileBridge {
    pub(crate) fn create_thread(
        &mut self,
        params: CreateThreadParams,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let Some((window, key, workspaces)) = find_project_group(&params.project_id, cx) else {
            return Task::ready(Err(anyhow!("the project is no longer open")));
        };
        let agent_id = params.agent_id;
        cx.spawn(async move |_, cx| {
            let workspace = match params.worktree {
                WorktreeChoice::Existing { path } => {
                    let folder_paths = PathList::new(&[std::path::PathBuf::from(&path)]);
                    // Projects without git have no worktree options, so the
                    // app sends an empty path for "the project itself".
                    let folder_paths = if key.path_list().paths().len() > 1 || path.is_empty() {
                        key.path_list().clone()
                    } else {
                        folder_paths
                    };
                    cx.update(|cx| find_or_open_workspace(window, key.clone(), folder_paths, cx))
                        .await?
                }
                WorktreeChoice::New { name, branch } => {
                    let source = match workspaces.first() {
                        Some(workspace) => workspace.clone(),
                        None => {
                            cx.update(|cx| {
                                find_or_open_workspace(
                                    window,
                                    key.clone(),
                                    key.path_list().clone(),
                                    cx,
                                )
                            })
                            .await?
                        }
                    };
                    let action = zed_actions::CreateWorktree {
                        worktree_name: name.filter(|name| !name.trim().is_empty()),
                        branch_target: match branch {
                            Some(name) => {
                                zed_actions::NewWorktreeBranchTarget::ExistingBranch { name }
                            }
                            None => zed_actions::NewWorktreeBranchTarget::CurrentBranch,
                        },
                    };
                    let creation = window.update(cx, |_, window, cx| {
                        source.update(cx, |workspace, cx| {
                            git_ui_core::worktree_service::create_worktree_workspace(
                                workspace, &action, window, None, cx,
                            )
                        })
                    })?;
                    creation
                        .await
                        .context("failed to create the worktree")?
                        .workspace
                }
            };
            let thread_id = wait_for_agent_panel(&workspace, cx).await.and_then(|()| {
                cx.update(|cx| {
                    agent_mobile::create_thread(&workspace, window.into(), &agent_id, cx)
                })
            })?;
            to_json(CreatedThread {
                thread_id: thread_id.to_key_string(),
            })
        })
    }

    pub(crate) fn open_thread(
        &mut self,
        client_id: ClientId,
        thread_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let conversation = self.load_conversation(&thread_id, cx);
        cx.spawn(async move |this, cx| {
            let conversation = conversation.await?;
            this.update(cx, |this, cx| {
                let snapshot = this.subscribe_thread(client_id, &thread_id, conversation, cx)?;
                to_json(snapshot)
            })?
        })
    }

    /// Makes sure the thread is watched through `conversation`, keeping
    /// whoever is already subscribed.
    fn ensure_thread_subscription(
        &mut self,
        thread_id: &str,
        conversation: &MobileConversation,
        cx: &mut Context<Self>,
    ) -> Result<&mut ThreadSubscription> {
        let needs_new_subscription = self.threads.threads.get(thread_id).is_none_or(|existing| {
            existing.conversation.conversation != conversation.conversation
                || existing.thread_view_id
                    != conversation
                        .thread_view(cx)
                        .map(|thread_view| thread_view.entity_id())
        });
        if needs_new_subscription {
            let (subscribers, streams) = self
                .threads
                .threads
                .remove(thread_id)
                .map(|existing| (existing.subscribers, existing.streams))
                .unwrap_or_default();
            let mut subscription =
                self.watch_thread(thread_id, conversation.clone(), subscribers, cx)?;
            subscription.streams_need_snapshot = !streams.is_empty();
            subscription.streams = streams;
            let needs_flush = subscription.streams_need_snapshot;
            self.threads
                .threads
                .insert(thread_id.to_string(), subscription);
            if needs_flush {
                self.schedule_thread_flush(thread_id, cx);
            }
        }
        self.threads
            .threads
            .get_mut(thread_id)
            .context("the thread subscription disappeared")
    }

    fn subscribe_thread(
        &mut self,
        client_id: ClientId,
        thread_id: &str,
        conversation: MobileConversation,
        cx: &mut Context<Self>,
    ) -> Result<ThreadSnapshot> {
        self.ensure_thread_subscription(thread_id, &conversation, cx)?
            .subscribers
            .insert(client_id);

        let state = agent_mobile::thread_state(&conversation, thread_id, cx)
            .context("the thread is still loading")?;
        let entries = agent_mobile::thread_entries(&conversation, cx);
        if let Some(subscription) = self.threads.threads.get_mut(thread_id) {
            subscription.entry_count = entries.len();
            subscription.last_state = Some(state.clone());
        }
        Ok(ThreadSnapshot {
            thread_id: thread_id.to_string(),
            workspace_id: conversation.workspace.entity_id().as_u64(),
            entries,
            state,
        })
    }

    fn watch_thread(
        &mut self,
        thread_id: &str,
        conversation: MobileConversation,
        subscribers: Subscribers,
        cx: &mut Context<Self>,
    ) -> Result<ThreadSubscription> {
        let thread = conversation
            .thread(cx)
            .context("the thread is still loading")?;
        let mut subscriptions = Vec::new();

        let event_thread_id = thread_id.to_string();
        subscriptions.push(cx.subscribe(&thread, move |this, _, event, cx| {
            this.handle_thread_event(&event_thread_id, event, cx);
        }));

        let view_thread_id = thread_id.to_string();
        let bridge = cx.weak_entity();
        if let Some(subscription) =
            agent_mobile::observe_thread_view(&conversation, cx, move |cx| {
                bridge
                    .update(cx, |this, cx| {
                        this.mark_thread_state_dirty(&view_thread_id, cx)
                    })
                    .ok();
            })
        {
            subscriptions.push(subscription);
        }

        // The conversation swaps in a new thread view when its session reloads
        // (e.g. after the agent restarts), so re-attach when that happens.
        let reload_thread_id = thread_id.to_string();
        subscriptions.push(cx.observe(&conversation.conversation, move |this, _, cx| {
            this.handle_conversation_changed(&reload_thread_id, cx);
        }));

        let closed_thread_id = thread_id.to_string();
        subscriptions.push(
            cx.observe_release(&conversation.conversation, move |this, _, cx| {
                this.handle_thread_closed(&closed_thread_id, cx);
            }),
        );

        Ok(ThreadSubscription {
            subscribers,
            streams: HashMap::default(),
            thread_view_id: conversation
                .thread_view(cx)
                .map(|thread_view| thread_view.entity_id()),
            conversation,
            entry_count: 0,
            last_state: None,
            last_runs: Vec::new(),
            streams_need_snapshot: false,
            dirty_entries: BTreeSet::new(),
            state_dirty: false,
            flush_task: None,
            _subscriptions: subscriptions,
        })
    }

    /// Starts streaming the thread's events to an HTTP API client, beginning
    /// with a snapshot.
    pub(crate) fn subscribe_thread_stream(
        &mut self,
        request_id: RequestId,
        thread_id: &str,
        conversation: MobileConversation,
        include_items: bool,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let details = thread_details(&conversation, thread_id, include_items, cx)
            .context("the thread is still loading")?;
        let subscription = self.ensure_thread_subscription(thread_id, &conversation, cx)?;
        // Other streams were sent the runs as of the last flush, which they
        // must keep being compared against.
        if subscription.streams.is_empty() {
            subscription.last_runs = details.runs.iter().map(without_items).collect();
        }
        subscription.streams.insert(request_id, include_items);
        self.rest.streams.insert(
            request_id,
            crate::rest::StreamTarget::Thread(thread_id.to_string()),
        );
        self.start_stream(request_id);
        self.send_stream_event(request_id, thread_events::SNAPSHOT, &details);
        Ok(())
    }

    fn handle_thread_closed(&mut self, thread_id: &str, _cx: &mut Context<Self>) {
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        let streams = std::mem::take(&mut subscription.streams);
        if subscription.is_unused() {
            self.threads.threads.remove(thread_id);
        }
        for request_id in streams.into_keys() {
            self.end_stream_with_error(request_id, "The thread was closed in Zed.");
        }
    }

    fn flush_thread_streams(
        &mut self,
        thread_id: &str,
        dirty_entries: &BTreeSet<usize>,
        state: Option<&api::ThreadState>,
        cx: &mut Context<Self>,
    ) {
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        if subscription.streams.is_empty() {
            return;
        }
        let conversation = subscription.conversation.clone();
        let streams = subscription.streams.clone();

        if std::mem::take(&mut subscription.streams_need_snapshot) {
            let Some(summary) = thread_details(&conversation, thread_id, false, cx) else {
                return;
            };
            let full = streams
                .values()
                .any(|include_items| *include_items)
                .then(|| thread_details(&conversation, thread_id, true, cx))
                .flatten();
            if let Some(subscription) = self.threads.threads.get_mut(thread_id) {
                subscription.last_runs = summary.runs.clone();
            }
            for (request_id, include_items) in streams {
                let details = match &full {
                    Some(full) if include_items => full,
                    _ => &summary,
                };
                self.send_stream_event(request_id, thread_events::SNAPSHOT, details);
            }
            return;
        }

        let ranges = agent_mobile::run_ranges(&conversation, cx);
        let Some(last_index) = ranges.len().checked_sub(1) else {
            if let Some(state) = state {
                for request_id in streams.keys() {
                    self.send_stream_event(*request_id, thread_events::STATE, state);
                }
            }
            return;
        };
        let run_index_of = |entry_index: usize| {
            ranges
                .iter()
                .position(|range| range.entries.contains(&entry_index))
        };
        // A run's status also changes when the next one starts or the agent
        // stops, which doesn't touch its entries.
        let mut touched_runs = dirty_entries
            .iter()
            .filter_map(|index| run_index_of(*index))
            .collect::<BTreeSet<_>>();
        touched_runs.insert(last_index);
        if let Some(previous_index) = last_index.checked_sub(1) {
            touched_runs.insert(previous_index);
        }

        let mut item_events = Vec::new();
        if streams.values().any(|include_items| *include_items) {
            for entry_index in dirty_entries {
                let Some(run_index) = run_index_of(*entry_index) else {
                    continue;
                };
                let range = &ranges[run_index];
                if let Some(item) = agent_mobile::thread_run_item(&conversation, *entry_index, cx) {
                    item_events.push(rest::RunItemEvent {
                        run_id: range.run_id.clone(),
                        index: entry_index - range.entries.start,
                        item,
                    });
                }
            }
        }

        let mut run_events = Vec::new();
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        subscription.last_runs.truncate(ranges.len());
        for run_index in touched_runs {
            let Some(run) = agent_mobile::build_run(
                &conversation,
                &ranges[run_index],
                run_index == last_index,
                false,
                cx,
            ) else {
                continue;
            };
            match subscription.last_runs.get_mut(run_index) {
                Some(last_run) if *last_run == run => continue,
                Some(last_run) => *last_run = run.clone(),
                None => subscription.last_runs.push(run.clone()),
            }
            run_events.push(run);
        }

        for (request_id, include_items) in streams {
            if include_items {
                for event in &item_events {
                    self.send_stream_event(request_id, thread_events::ITEM, event);
                }
            }
            for run in &run_events {
                self.send_stream_event(request_id, thread_events::RUN, run);
            }
            if let Some(state) = state {
                self.send_stream_event(request_id, thread_events::STATE, state);
            }
        }
    }

    fn handle_conversation_changed(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(subscription) = self.threads.threads.get(thread_id) else {
            return;
        };
        let current_view_id = subscription
            .conversation
            .thread_view(cx)
            .map(|thread_view| thread_view.entity_id());
        if current_view_id == subscription.thread_view_id || current_view_id.is_none() {
            return;
        }
        let Some(subscription) = self.threads.threads.remove(thread_id) else {
            return;
        };
        let old_entry_count = subscription.entry_count;
        let conversation = subscription.conversation;
        let subscribers = subscription.subscribers;
        let streams = subscription.streams;
        match self.watch_thread(thread_id, conversation.clone(), subscribers.clone(), cx) {
            Ok(mut new_subscription) => {
                new_subscription.streams_need_snapshot = !streams.is_empty();
                new_subscription.streams = streams;
                let entries = agent_mobile::thread_entries(&conversation, cx);
                for client_id in &subscribers {
                    self.send_event(
                        *client_id,
                        events::THREAD_ENTRIES_REMOVED,
                        &ThreadEntriesRemovedEvent {
                            thread_id: thread_id.to_string(),
                            start: 0,
                            end: old_entry_count,
                        },
                    );
                    for (index, entry) in entries.iter().enumerate() {
                        self.send_event(
                            *client_id,
                            events::THREAD_ENTRY,
                            &ThreadEntryEvent {
                                thread_id: thread_id.to_string(),
                                index,
                                entry: entry.clone(),
                            },
                        );
                    }
                }
                new_subscription.entry_count = entries.len();
                new_subscription.state_dirty = true;
                self.threads
                    .threads
                    .insert(thread_id.to_string(), new_subscription);
                self.schedule_thread_flush(thread_id, cx);
            }
            Err(error) => {
                log::error!("failed to re-attach to the thread: {error:#}");
                for request_id in streams.into_keys() {
                    self.end_stream_with_error(
                        request_id,
                        &format!("Lost track of the thread: {error:#}"),
                    );
                }
            }
        }
    }

    fn handle_thread_event(
        &mut self,
        thread_id: &str,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        match event {
            AcpThreadEvent::NewEntry => {
                let entry_count = subscription
                    .conversation
                    .thread(cx)
                    .map(|thread| thread.read(cx).entries().len())
                    .unwrap_or(0);
                if let Some(index) = entry_count.checked_sub(1) {
                    subscription.dirty_entries.insert(index);
                }
                subscription.state_dirty = true;
            }
            AcpThreadEvent::EntryUpdated(index) => {
                subscription.dirty_entries.insert(*index);
            }
            AcpThreadEvent::EntriesRemoved(range) => {
                // Entries are addressed by index, so pending updates must be
                // sent before the indices shift.
                self.flush_thread(thread_id, cx);
                let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
                    return;
                };
                subscription.entry_count = subscription
                    .entry_count
                    .saturating_sub(range.end.saturating_sub(range.start));
                let event = ThreadEntriesRemovedEvent {
                    thread_id: thread_id.to_string(),
                    start: range.start,
                    end: range.end,
                };
                let subscribers = subscription.subscribers.clone();
                if !subscription.streams.is_empty() {
                    subscription.streams_need_snapshot = true;
                    self.schedule_thread_flush(thread_id, cx);
                }
                for client_id in subscribers {
                    self.send_event(client_id, events::THREAD_ENTRIES_REMOVED, &event);
                }
                return;
            }
            AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_) => {
                subscription.state_dirty = true;
                // The tool call's permission buttons live in its entry.
                if let Some(thread) = subscription.conversation.thread(cx) {
                    let entry_count = thread.read(cx).entries().len();
                    subscription
                        .dirty_entries
                        .extend((0..entry_count).rev().take(8));
                }
            }
            _ => {
                subscription.state_dirty = true;
            }
        }
        self.schedule_thread_flush(thread_id, cx);
    }

    pub(crate) fn mark_thread_state_dirty(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        if let Some(subscription) = self.threads.threads.get_mut(thread_id) {
            subscription.state_dirty = true;
            self.schedule_thread_flush(thread_id, cx);
        }
    }

    fn schedule_thread_flush(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        if subscription.flush_task.is_some() {
            return;
        }
        let thread_id = thread_id.to_string();
        subscription.flush_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(THREAD_FLUSH_INTERVAL).await;
            this.update(cx, |this, cx| this.flush_thread(&thread_id, cx))
                .ok();
        }));
    }

    fn flush_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        subscription.flush_task = None;
        let dirty_entries = std::mem::take(&mut subscription.dirty_entries);
        let state_dirty = std::mem::take(&mut subscription.state_dirty);
        let conversation = subscription.conversation.clone();
        let subscribers = subscription.subscribers.clone();

        let mut entry_events = Vec::new();
        for index in dirty_entries.iter().copied() {
            if subscribers.is_empty() {
                break;
            }
            if let Some(entry) = agent_mobile::thread_entry(&conversation, index, cx) {
                entry_events.push(ThreadEntryEvent {
                    thread_id: thread_id.to_string(),
                    index,
                    entry,
                });
            }
        }
        let entry_count = conversation
            .thread(cx)
            .map(|thread| thread.read(cx).entries().len())
            .unwrap_or(0);
        let new_state = state_dirty
            .then(|| agent_mobile::thread_state(&conversation, thread_id, cx))
            .flatten();

        let Some(subscription) = self.threads.threads.get_mut(thread_id) else {
            return;
        };
        subscription.entry_count = entry_count;
        let state_to_send =
            new_state.filter(|state| subscription.last_state.as_ref() != Some(state));
        if let Some(state) = &state_to_send {
            subscription.last_state = Some(state.clone());
        }
        for client_id in &subscribers {
            for event in &entry_events {
                self.send_event(*client_id, events::THREAD_ENTRY, event);
            }
            if let Some(state) = &state_to_send {
                self.send_event(*client_id, events::THREAD_STATE, state);
            }
        }
        self.flush_thread_streams(thread_id, &dirty_entries, state_to_send.as_ref(), cx);
    }

    pub(crate) fn conversation_for(&self, thread_id: &str, cx: &App) -> Result<MobileConversation> {
        if let Some(subscription) = self.threads.threads.get(thread_id) {
            return Ok(subscription.conversation.clone());
        }
        let thread_id = ThreadId::from_key_string(thread_id).context("invalid thread id")?;
        agent_mobile::find_conversation(thread_id, cx).context("the thread isn't open")
    }

    pub(crate) fn send_message(
        &mut self,
        thread_id: &str,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = self
            .conversation_for(thread_id, cx)
            .and_then(|conversation| agent_mobile::send_message(&conversation, text, cx));
        Task::ready(result.map(|()| serde_json::Value::Null))
    }

    pub(crate) fn cancel_generation(
        &mut self,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = self
            .conversation_for(thread_id, cx)
            .and_then(|conversation| agent_mobile::cancel_generation(&conversation, cx));
        Task::ready(result.map(|()| serde_json::Value::Null))
    }

    pub(crate) fn authorize_tool_call(
        &mut self,
        thread_id: &str,
        tool_call_id: String,
        option_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = self
            .conversation_for(thread_id, cx)
            .and_then(|conversation| {
                agent_mobile::authorize_tool_call(&conversation, &tool_call_id, &option_id, cx)
            });
        Task::ready(result.map(|()| serde_json::Value::Null))
    }

    pub(crate) fn set_thread_control(
        &mut self,
        thread_id: &str,
        control_id: &str,
        value: ControlValue,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let conversation = match self.conversation_for(thread_id, cx) {
            Ok(conversation) => conversation,
            Err(error) => return Task::ready(Err(error)),
        };
        let task = agent_mobile::set_control(&conversation, control_id, value, cx);
        let thread_id = thread_id.to_string();
        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |this, cx| this.mark_thread_state_dirty(&thread_id, cx))?;
            Ok(serde_json::Value::Null)
        })
    }

    pub(crate) fn remove_queued_message(
        &mut self,
        thread_id: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = self
            .conversation_for(thread_id, cx)
            .and_then(|conversation| agent_mobile::remove_queued_message(&conversation, index, cx));
        Task::ready(result.map(|()| serde_json::Value::Null))
    }
}

pub(crate) fn thread_details(
    conversation: &MobileConversation,
    thread_id: &str,
    include_items: bool,
    cx: &App,
) -> Option<rest::ThreadDetails> {
    Some(rest::ThreadDetails {
        state: agent_mobile::thread_state(conversation, thread_id, cx)?,
        workspace_paths: workspace_paths(&conversation.workspace, cx),
        runs: agent_mobile::thread_runs(conversation, include_items, cx),
    })
}

pub(crate) fn workspace_paths(workspace: &Entity<Workspace>, cx: &App) -> Vec<String> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
        .collect()
}

fn without_items(run: &rest::Run) -> rest::Run {
    rest::Run {
        items: None,
        ..run.clone()
    }
}

fn push_worktree_option(
    worktrees: &mut Vec<WorktreeOption>,
    path: std::path::PathBuf,
    branch: Option<String>,
    is_main: bool,
) {
    let path_string = path.to_string_lossy().into_owned();
    if worktrees
        .iter()
        .any(|worktree| worktree.path == path_string)
    {
        return;
    }
    let file_name = |path: &std::path::Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let mut name = file_name(&path).unwrap_or_else(|| path_string.clone());
    // Zed creates worktrees at `<directory>/<worktree name>/<repository
    // name>`, which would all be named like the main worktree.
    if !is_main
        && worktrees.iter().any(|worktree| worktree.name == name)
        && let Some(parent_name) = path.parent().and_then(file_name)
    {
        name = parent_name;
    }
    worktrees.push(WorktreeOption {
        path: path_string,
        name,
        branch,
        is_main,
    });
}

/// A stable identifier for a project group, used by the app to refer to it.
fn project_id(key: &ProjectGroupKey) -> String {
    let host = key
        .host()
        .map(|host| format!("{}:{}", host.connection_type(), host.host()))
        .unwrap_or_else(|| "local".to_string());
    let paths = key
        .path_list()
        .paths()
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join(";");
    format!("{host}|{paths}")
}

pub(crate) fn find_project_group(
    project_id_to_find: &str,
    cx: &App,
) -> Option<(
    WindowHandle<MultiWorkspace>,
    ProjectGroupKey,
    Vec<Entity<Workspace>>,
)> {
    multi_workspace_windows(cx).into_iter().find_map(|window| {
        let multi_workspace = window.read(cx).ok()?;
        sidebar::mobile::project_groups(multi_workspace, cx)
            .into_iter()
            .find(|group| project_id(&group.key) == project_id_to_find)
            .map(|group| (window, group.key, group.workspaces))
    })
}

pub(crate) async fn wait_for_agent_panel(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = cx.background_executor().now() + THREAD_LOAD_TIMEOUT;
    loop {
        let has_panel = cx.update(|cx| workspace.read(cx).panel::<AgentPanel>(cx).is_some());
        if has_panel {
            return Ok(());
        }
        if cx.background_executor().now() >= deadline {
            return Err(anyhow!("the workspace didn't finish loading"));
        }
        cx.background_executor()
            .timer(THREAD_LOAD_POLL_INTERVAL)
            .await;
    }
}

pub(crate) fn build_threads_list(cx: &App) -> ThreadsList {
    let projects = threads_snapshot(cx)
        .into_iter()
        .map(|project| ProjectThreads {
            project_id: project_id(&project.key),
            name: project.name.to_string(),
            remote_host: project.key.host().map(|host| host.display_name()),
            is_active: project.is_active,
            threads: project
                .threads
                .into_iter()
                .map(|thread| {
                    let (thread_id, kind) = match thread.id {
                        MobileThreadId::Agent(thread_id) => {
                            (thread_id.to_key_string(), ThreadKind::Agent)
                        }
                        MobileThreadId::Terminal(terminal_id) => {
                            (terminal_id.to_key_string(), ThreadKind::Terminal)
                        }
                    };
                    ThreadSummary {
                        thread_id,
                        kind,
                        title: thread.title.to_string(),
                        status: match thread.status {
                            ui::AgentThreadStatus::Running => api::ThreadStatus::Running,
                            ui::AgentThreadStatus::WaitingForConfirmation => {
                                api::ThreadStatus::WaitingForConfirmation
                            }
                            ui::AgentThreadStatus::Error => api::ThreadStatus::Error,
                            ui::AgentThreadStatus::Completed => api::ThreadStatus::Idle,
                        },
                        agent_name: thread.agent_id.map(|agent_id| agent_name(&agent_id, cx)),
                        worktrees: thread
                            .worktrees
                            .into_iter()
                            .map(|worktree| worktree.to_string())
                            .collect(),
                        lines_added: thread.lines_added,
                        lines_removed: thread.lines_removed,
                        has_notification: thread.has_notification,
                        is_active: thread.is_active,
                        updated_at: Some(thread.updated_at.to_rfc3339()),
                    }
                })
                .collect(),
        })
        .collect();
    ThreadsList { projects }
}

fn agent_name(agent_id: &project::AgentId, cx: &App) -> String {
    if agent_id.as_ref() == agent::ZED_AGENT_ID.as_ref() {
        return "Zed Agent".to_string();
    }
    crate::active_workspace(cx)
        .ok()
        .and_then(|(_, workspace)| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .agent_server_store()
                .read(cx)
                .agent_display_name(agent_id)
        })
        .map(|name| name.to_string())
        .unwrap_or_else(|| agent_id.0.to_string())
}
