use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use agent::{ThreadStore, ThreadStoreEvent};
use anyhow::Context as _;
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::{
    kvp::KeyValueStore,
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use futures::{FutureExt, channel::oneshot, future::Shared};
use gpui::{AppContext as _, Entity, EntityId, Global, Subscription, Task};
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use ui::{App, Context, SharedString};
use util::ResultExt as _;
use workspace::PathList;

use crate::{
    TerminalId,
    thread_metadata_store::{ThreadMetadataStore, WorktreePaths},
};

/// The terminal threads of remote hosts as last seen, so they can be listed
/// before the hosts are connected again (e.g. right after launch).
const REMOTE_TERMINALS_CACHE_KEY: &str = "remote-terminal-thread-cache";
/// The writes to remote hosts that the hosts haven't confirmed yet, so they
/// survive Zed quitting before the hosts are reachable again.
const REMOTE_TERMINALS_OUTBOX_KEY: &str = "remote-terminal-thread-outbox";
const PERSIST_REMOTE_STATE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedRemoteTerminals {
    connection: RemoteConnectionOptions,
    terminals: Vec<proto::AgentTerminalThread>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedRemoteWrite {
    connection: RemoteConnectionOptions,
    upsert: Option<proto::AgentTerminalThread>,
    delete: Option<String>,
}

pub fn init(cx: &mut App) {
    TerminalThreadMetadataStore::init_global(cx);
}

struct GlobalTerminalThreadMetadataStore(Entity<TerminalThreadMetadataStore>);
impl Global for GlobalTerminalThreadMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTerminalMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTerminalMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTerminalMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TERMINAL_THREAD_METADATA_DB_{}", test_name)
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadMetadata {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub custom_title: Option<SharedString>,
    pub created_at: DateTime<Utc>,
    pub worktree_paths: WorktreePaths,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub working_directory: Option<PathBuf>,
    /// Archived terminals have no live shell and are listed in Thread History
    /// instead of the sidebar. They still reference their worktree, so it is
    /// kept on disk for when the terminal is restored.
    pub archived: bool,
}

impl TerminalThreadMetadata {
    pub fn folder_paths(&self) -> &PathList {
        self.worktree_paths.folder_path_list()
    }

    pub fn main_worktree_paths(&self) -> &PathList {
        self.worktree_paths.main_worktree_path_list()
    }

    pub fn display_title(&self) -> SharedString {
        compose_terminal_thread_title(
            self.title.as_ref(),
            self.custom_title.as_ref().map(|title| title.as_ref()),
        )
    }

    pub fn editable_title(&self) -> SharedString {
        self.custom_title.clone().unwrap_or_else(|| {
            SharedString::from(terminal_title_without_prefix(self.title.as_ref()).to_string())
        })
    }
}

pub(crate) fn compose_terminal_thread_title(
    terminal_title: &str,
    custom_title: Option<&str>,
) -> SharedString {
    let Some(custom_title) = custom_title.filter(|title| !title.trim().is_empty()) else {
        return SharedString::from(terminal_title.to_string());
    };

    if let Some(prefix) = terminal_title_prefix(terminal_title) {
        SharedString::from(format!("{prefix}{custom_title}"))
    } else {
        SharedString::from(custom_title.to_string())
    }
}

pub(crate) fn terminal_title_without_prefix(title: &str) -> &str {
    terminal_title_prefix(title)
        .map(|prefix| &title[prefix.len()..])
        .unwrap_or(title)
}

pub(crate) fn normalize_terminal_custom_title(
    terminal_title: &str,
    edited_title: SharedString,
) -> Option<SharedString> {
    if edited_title.trim().is_empty()
        || edited_title == terminal_title_without_prefix(terminal_title)
    {
        None
    } else {
        Some(edited_title)
    }
}

pub fn terminal_title_prefix(title: &str) -> Option<&str> {
    let mut prefix_byte_len = 0;
    let mut saw_prefix_character = false;
    let mut saw_whitespace_after_prefix = false;

    let mut chars = title.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_alphanumeric() {
            return None;
        }

        if character.is_whitespace() {
            if !saw_prefix_character {
                return None;
            }

            prefix_byte_len += character.len_utf8();
            saw_whitespace_after_prefix = true;

            while let Some(character) = chars.peek() {
                if !character.is_whitespace() {
                    break;
                }

                prefix_byte_len += character.len_utf8();
                chars.next();
            }

            break;
        }

        saw_prefix_character = true;
        prefix_byte_len += character.len_utf8();
    }

    if saw_whitespace_after_prefix {
        Some(&title[..prefix_byte_len])
    } else {
        None
    }
}

pub struct TerminalThreadMetadataStore {
    terminals: HashMap<TerminalId, TerminalThreadMetadata>,
    terminals_by_paths: HashMap<PathList, HashSet<TerminalId>>,
    terminals_by_main_paths: HashMap<PathList, HashSet<TerminalId>>,
    reload_task: Option<Shared<Task<()>>>,
    pending_terminal_ops_tx: async_channel::Sender<DbRequest>,
    _db_operations_task: Task<()>,
    /// Terminals whose metadata is stored on a remote host (so that the Zed
    /// running there sees them too), with the connection to that host.
    remote_terminals: HashMap<TerminalId, RemoteConnectionOptions>,
    /// Writes to remote hosts, applied in order by `_remote_operations_task`.
    remote_operations: VecDeque<(RemoteConnectionOptions, RemoteDbOperation)>,
    /// Writes to remote hosts that weren't connected, retried on reconnect.
    stalled_remote_operations: Vec<(RemoteConnectionOptions, RemoteDbOperation)>,
    remote_operations_tx: async_channel::Sender<()>,
    _remote_operations_task: Task<()>,
    /// Incremented on every in-memory change to a terminal. Reloads use it to
    /// tell which terminals changed after the reloaded rows were requested, so
    /// that they don't revert changes that hadn't been written yet.
    mutation_count: u64,
    last_mutation: HashMap<TerminalId, u64>,
    _thread_store_subscription: Option<Subscription>,
    /// Writes to remote hosts that the host hasn't confirmed yet, with the
    /// mutation count they were made at.
    pending_remote_writes: HashMap<TerminalId, (RemoteConnectionOptions, RemoteWrite, u64)>,
    _persist_remote_state: Option<Task<()>>,
    _quit_subscription: Subscription,
}

#[derive(Debug, PartialEq)]
enum DbOperation {
    Upsert(TerminalThreadMetadata),
    Delete(TerminalId),
}

enum DbRequest {
    Operation(DbOperation),
    /// Lists all rows once the operations requested before it were applied.
    List(oneshot::Sender<anyhow::Result<Vec<TerminalThreadMetadata>>>),
}

enum RemoteDbOperation {
    /// A write, made when the store's mutation count was `mutation`.
    Write { write: RemoteWrite, mutation: u64 },
    /// Re-fetches the host's terminals. `mutation_count` is the store's
    /// mutation count when the list was requested.
    List { mutation_count: u64 },
}

#[derive(Clone)]
enum RemoteWrite {
    Upsert(TerminalThreadMetadata),
    Delete(TerminalId),
}

impl RemoteWrite {
    fn terminal_id(&self) -> TerminalId {
        match self {
            RemoteWrite::Upsert(metadata) => metadata.terminal_id,
            RemoteWrite::Delete(terminal_id) => *terminal_id,
        }
    }
}

impl RemoteDbOperation {
    fn terminal_id(&self) -> Option<TerminalId> {
        match self {
            RemoteDbOperation::Write { write, .. } => Some(write.terminal_id()),
            RemoteDbOperation::List { .. } => None,
        }
    }
}

impl DbOperation {
    fn id(&self) -> TerminalId {
        match self {
            DbOperation::Upsert(metadata) => metadata.terminal_id,
            DbOperation::Delete(terminal_id) => *terminal_id,
        }
    }
}

impl TerminalThreadMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTerminalThreadMetadataStore>() {
            return;
        }

        let db = TerminalThreadMetadataDb::global(cx);
        let terminal_store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTerminalMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(&db_name));
        let terminal_store = cx.new(|cx| Self::new(TerminalThreadMetadataDb(db), cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalThreadMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTerminalThreadMetadataStore>().0.clone()
    }

    pub fn entry(&self, terminal_id: TerminalId) -> Option<&TerminalThreadMetadata> {
        self.terminals.get(&terminal_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TerminalThreadMetadata> + '_ {
        self.terminals.values()
    }

    pub fn archived_entries(&self) -> impl Iterator<Item = &TerminalThreadMetadata> + '_ {
        self.terminals.values().filter(|terminal| terminal.archived)
    }

    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared())
    }

    /// Unarchived terminals whose folder paths match `path_list`.
    pub fn entries_for_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                !terminal.archived
                    && same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
            })
    }

    /// Unarchived terminals whose main worktree paths match `path_list`.
    pub fn entries_for_main_worktree_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_main_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                !terminal.archived
                    && same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
            })
    }

    /// Whether any terminal other than `terminal_id` uses `path`. Archived
    /// terminals count too, so their worktrees survive until they're restored
    /// or deleted.
    pub fn path_is_referenced_by_terminal(
        &self,
        terminal_id: Option<TerminalId>,
        path: &Path,
        remote_connection: Option<&RemoteConnectionOptions>,
    ) -> bool {
        self.entries().any(|terminal| {
            Some(terminal.terminal_id) != terminal_id
                && same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
                && terminal
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|folder_path| folder_path.as_path() == path)
        })
    }

    pub fn save(&mut self, metadata: TerminalThreadMetadata, cx: &mut Context<Self>) {
        self.save_internal(metadata, cx);
        cx.notify();
    }

    pub fn rename_terminal(
        &mut self,
        terminal_id: TerminalId,
        title: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(mut metadata) = self.entry(terminal_id).cloned() else {
            return;
        };
        let custom_title = normalize_terminal_custom_title(metadata.title.as_ref(), title);
        if metadata.custom_title == custom_title {
            return;
        }

        metadata.custom_title = custom_title;
        self.save_internal(metadata, cx);
        cx.notify();
    }

    pub fn archive(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        self.update_archived(terminal_id, true, cx);
    }

    pub fn unarchive(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        self.update_archived(terminal_id, false, cx);
    }

    fn update_archived(&mut self, terminal_id: TerminalId, archived: bool, cx: &mut Context<Self>) {
        let Some(mut metadata) = self.entry(terminal_id).cloned() else {
            return;
        };
        if metadata.archived == archived {
            return;
        }

        metadata.archived = archived;
        self.save_internal(metadata, cx);
        cx.notify();
    }

    pub fn change_worktree_paths(
        &mut self,
        current_folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        mutate: impl Fn(&mut WorktreePaths),
        cx: &mut Context<Self>,
    ) {
        let terminal_ids: Vec<_> = self
            .terminals_by_paths
            .get(current_folder_paths)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.terminals.get(id).is_some_and(|terminal| {
                    same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
                })
            })
            .copied()
            .collect();

        if terminal_ids.is_empty() {
            return;
        }

        for terminal_id in terminal_ids {
            if let Some(mut terminal) = self.terminals.get(&terminal_id).cloned() {
                mutate(&mut terminal.worktree_paths);
                self.save_internal(terminal, cx);
            }
        }

        cx.notify();
    }

    fn save_internal(&mut self, metadata: TerminalThreadMetadata, cx: &App) {
        // Route the write before caching, which changes whether the terminal
        // counts as already persisted.
        let is_remote = self.enqueue_write(DbOperation::Upsert(metadata.clone()), cx);

        if let Some(existing) = self.terminals.get(&metadata.terminal_id) {
            if existing.folder_paths() != metadata.folder_paths()
                && let Some(ids) = self.terminals_by_paths.get_mut(existing.folder_paths())
            {
                ids.remove(&metadata.terminal_id);
            }

            if existing.main_worktree_paths() != metadata.main_worktree_paths()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(existing.main_worktree_paths())
            {
                ids.remove(&metadata.terminal_id);
            }
        }

        self.cache_terminal_metadata(metadata);
        if is_remote {
            self.schedule_persist_remote_state(cx);
        }
    }

    fn cache_terminal_metadata(&mut self, metadata: TerminalThreadMetadata) {
        self.terminals
            .insert(metadata.terminal_id, metadata.clone());

        self.terminals_by_paths
            .entry(metadata.folder_paths().clone())
            .or_default()
            .insert(metadata.terminal_id);

        if !metadata.main_worktree_paths().is_empty() {
            self.terminals_by_main_paths
                .entry(metadata.main_worktree_paths().clone())
                .or_default()
                .insert(metadata.terminal_id);
        }
    }

    pub fn delete(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.remove(&terminal_id) {
            if let Some(ids) = self.terminals_by_paths.get_mut(terminal.folder_paths()) {
                ids.remove(&terminal_id);
            }
            if !terminal.main_worktree_paths().is_empty()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(terminal.main_worktree_paths())
            {
                ids.remove(&terminal_id);
            }
        }
        if self.enqueue_write(DbOperation::Delete(terminal_id), cx) {
            self.schedule_persist_remote_state(cx);
        }
        cx.notify();
    }

    fn new(db: TerminalThreadMetadataDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded::<DbRequest>();
        let _db_operations_task = cx.background_spawn({
            async move {
                while let Ok(first_request) = rx.recv().await {
                    let mut requests = vec![first_request];
                    while let Ok(request) = rx.try_recv() {
                        requests.push(request);
                    }
                    let mut updates = Vec::new();
                    let mut list_requests = Vec::new();
                    for request in requests {
                        match request {
                            DbRequest::Operation(operation) => updates.push(operation),
                            DbRequest::List(tx) => list_requests.push(tx),
                        }
                    }
                    let updates = Self::dedup_db_operations(updates);
                    let wrote = !updates.is_empty();
                    for operation in updates {
                        match operation {
                            DbOperation::Upsert(metadata) => {
                                db.save(metadata).await.log_err();
                            }
                            DbOperation::Delete(terminal_id) => {
                                db.delete(terminal_id).await.log_err();
                            }
                        }
                    }
                    if wrote && cfg!(not(any(test, feature = "test-support"))) {
                        paths::touch_agent_threads_changed_marker().log_err();
                    }
                    for tx in list_requests {
                        tx.send(db.list()).ok();
                    }
                }
            }
        });

        let (remote_operations_tx, remote_operations_rx) = async_channel::unbounded();
        let _remote_operations_task = cx.spawn(async move |this, cx| {
            Self::run_remote_operations(this, remote_operations_rx, cx).await
        });

        let _thread_store_subscription = ThreadStore::try_global(cx)
            .map(|thread_store| cx.subscribe(&thread_store, Self::handle_thread_store_event));

        let mut this = Self {
            terminals: HashMap::default(),
            terminals_by_paths: HashMap::default(),
            terminals_by_main_paths: HashMap::default(),
            reload_task: None,
            pending_terminal_ops_tx: tx,
            _db_operations_task,
            remote_terminals: HashMap::default(),
            remote_operations: VecDeque::new(),
            stalled_remote_operations: Vec::new(),
            remote_operations_tx,
            _remote_operations_task,
            mutation_count: 0,
            last_mutation: HashMap::default(),
            _thread_store_subscription,
            pending_remote_writes: HashMap::default(),
            _persist_remote_state: None,
            _quit_subscription: cx.on_app_quit(Self::flush_remote_state_on_quit),
        };
        this.load_remote_state(cx);
        if let Some(thread_store) = ThreadStore::try_global(cx) {
            let host_ids = thread_store
                .read(cx)
                .remote_storages()
                .map(|storage| storage.host_id())
                .collect::<Vec<_>>();
            for host_id in host_ids {
                this.reload_remote_host(&thread_store, host_id, cx);
            }
        }
        this.reload(cx);
        this
    }

    fn changed_since(&self, terminal_id: TerminalId, mutation_count: u64) -> bool {
        self.last_mutation
            .get(&terminal_id)
            .is_some_and(|last_mutation| *last_mutation > mutation_count)
    }

    fn rebuild_cache(&mut self, terminals: HashMap<TerminalId, TerminalThreadMetadata>) {
        self.terminals.clear();
        self.terminals_by_paths.clear();
        self.terminals_by_main_paths.clear();
        for terminal in terminals.into_values() {
            self.cache_terminal_metadata(terminal);
        }
    }

    /// Replaces the cached local terminals with rows read from the local
    /// database when `mutation_count` was current.
    fn apply_local_rows(&mut self, rows: Vec<TerminalThreadMetadata>, mutation_count: u64) {
        let mut terminals = HashMap::default();
        for (terminal_id, terminal) in std::mem::take(&mut self.terminals) {
            if self.remote_terminals.contains_key(&terminal_id)
                || self.changed_since(terminal_id, mutation_count)
            {
                terminals.insert(terminal_id, terminal);
            }
        }
        for row in rows {
            let terminal_id = row.terminal_id;
            if self.remote_terminals.contains_key(&terminal_id)
                || self.changed_since(terminal_id, mutation_count)
            {
                continue;
            }
            terminals.insert(terminal_id, row);
        }
        self.rebuild_cache(terminals);
    }

    /// Replaces the cached terminals of a remote host with rows read from the
    /// host when `mutation_count` was current.
    fn apply_remote_rows(
        &mut self,
        connection: &RemoteConnectionOptions,
        rows: Vec<TerminalThreadMetadata>,
        mutation_count: u64,
    ) {
        let on_host = |terminal_connection: &RemoteConnectionOptions| {
            same_remote_connection_identity(Some(terminal_connection), Some(connection))
        };
        let mut terminals = HashMap::default();
        for (terminal_id, terminal) in std::mem::take(&mut self.terminals) {
            let is_on_host = self
                .remote_terminals
                .get(&terminal_id)
                .is_some_and(|terminal_connection| on_host(terminal_connection));
            if !is_on_host || self.changed_since(terminal_id, mutation_count) {
                terminals.insert(terminal_id, terminal);
            }
        }
        self.remote_terminals
            .retain(|terminal_id, terminal_connection| {
                !on_host(terminal_connection) || terminals.contains_key(terminal_id)
            });
        for mut row in rows {
            let terminal_id = row.terminal_id;
            if self.changed_since(terminal_id, mutation_count) {
                continue;
            }
            // A terminal kept in the local database takes precedence.
            if terminals.contains_key(&terminal_id)
                && !self.remote_terminals.contains_key(&terminal_id)
            {
                continue;
            }
            row.remote_connection = Some(connection.clone());
            self.remote_terminals
                .insert(terminal_id, connection.clone());
            terminals.insert(terminal_id, row);
        }
        self.rebuild_cache(terminals);
    }

    /// Returns the remote host the metadata should be written to, if any.
    ///
    /// Terminals keep living where they were first persisted. New terminals of
    /// remote projects are stored on the host, as long as it's connected.
    fn remote_connection_for_write(
        &mut self,
        metadata: &TerminalThreadMetadata,
        cx: &App,
    ) -> Option<RemoteConnectionOptions> {
        let terminal_id = metadata.terminal_id;
        if let Some(connection) = self.remote_terminals.get(&terminal_id) {
            return Some(connection.clone());
        }
        if self.terminals.contains_key(&terminal_id) {
            return None;
        }
        let connection = metadata.remote_connection.as_ref()?;
        ThreadMetadataStore::remote_storage(connection, cx)?;
        self.remote_terminals
            .insert(terminal_id, connection.clone());
        Some(connection.clone())
    }

    /// Returns whether the write goes to a remote host.
    fn enqueue_write(&mut self, operation: DbOperation, cx: &App) -> bool {
        let terminal_id = operation.id();
        self.mutation_count += 1;
        self.last_mutation.insert(terminal_id, self.mutation_count);

        let remote_connection = match &operation {
            DbOperation::Upsert(metadata) => self.remote_connection_for_write(metadata, cx),
            DbOperation::Delete(terminal_id) => self.remote_terminals.remove(terminal_id),
        };
        match remote_connection {
            Some(connection) => {
                let write = match operation {
                    DbOperation::Upsert(metadata) => RemoteWrite::Upsert(metadata),
                    DbOperation::Delete(terminal_id) => RemoteWrite::Delete(terminal_id),
                };
                self.queue_remote_write(connection, write);
                true
            }
            None => {
                self.pending_terminal_ops_tx
                    .try_send(DbRequest::Operation(operation))
                    .log_err();
                false
            }
        }
    }

    /// Takes the next remote operation whose host is connected.
    fn next_remote_operation(
        &mut self,
        cx: &App,
    ) -> Option<(
        RemoteConnectionOptions,
        RemoteDbOperation,
        agent::RemoteThreadStorage,
    )> {
        while let Some((connection, operation)) = self.remote_operations.pop_front() {
            // A later write to the same terminal supersedes this one.
            if let Some(terminal_id) = operation.terminal_id()
                && self
                    .remote_operations
                    .iter()
                    .any(|(_, later)| later.terminal_id() == Some(terminal_id))
            {
                continue;
            }
            match ThreadMetadataStore::remote_storage(&connection, cx) {
                Some(storage) => return Some((connection, operation, storage)),
                None => {
                    if operation.terminal_id().is_some() {
                        self.stalled_remote_operations.push((connection, operation));
                    }
                }
            }
        }
        None
    }

    async fn run_remote_operations(
        this: gpui::WeakEntity<Self>,
        wake: async_channel::Receiver<()>,
        cx: &mut gpui::AsyncApp,
    ) {
        while wake.recv().await.is_ok() {
            loop {
                let Ok(Some((connection, operation, storage))) =
                    this.update(cx, |this, cx| this.next_remote_operation(cx))
                else {
                    break;
                };
                let client = storage.client().clone();
                let result = match &operation {
                    RemoteDbOperation::Write { write, mutation } => {
                        let result = match write {
                            RemoteWrite::Upsert(metadata) => client
                                .request(proto::SaveAgentTerminalThread {
                                    terminal: Some(terminal_to_proto(metadata)),
                                })
                                .await
                                .map(|_| ()),
                            RemoteWrite::Delete(terminal_id) => client
                                .request(proto::DeleteAgentTerminalThread {
                                    terminal_id: terminal_id.to_key_string(),
                                })
                                .await
                                .map(|_| ()),
                        };
                        if result.is_ok() {
                            let (terminal_id, mutation) = (write.terminal_id(), *mutation);
                            this.update(cx, |this, cx| {
                                this.acknowledge_remote_write(terminal_id, mutation, cx)
                            })
                            .ok();
                        }
                        result
                    }
                    RemoteDbOperation::List { mutation_count } => {
                        let mutation_count = *mutation_count;
                        let Some(response) = client
                            .request(proto::ListAgentTerminalThreads {})
                            .await
                            .context("failed to list terminal threads of remote host")
                            .log_err()
                        else {
                            continue;
                        };
                        let rows = response
                            .terminals
                            .into_iter()
                            .filter_map(|row| terminal_from_proto(row).log_err())
                            .collect();
                        this.update(cx, |this, cx| {
                            this.apply_remote_rows(&connection, rows, mutation_count);
                            this.schedule_persist_remote_state(cx);
                            cx.notify();
                        })
                        .ok();
                        continue;
                    }
                };
                if let Err(error) = result {
                    log::error!("failed to write terminal thread to remote host: {error:#}");
                    // Retry once the host reconnects.
                    this.update(cx, |this, _| {
                        this.stalled_remote_operations.push((connection, operation));
                    })
                    .ok();
                }
            }
        }
    }

    fn handle_thread_store_event(
        &mut self,
        thread_store: Entity<ThreadStore>,
        event: &ThreadStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ThreadStoreEvent::RemoteHostChanged(host_id) => {
                self.reload_remote_host(&thread_store, *host_id, cx);
            }
            ThreadStoreEvent::RemoteHostRemoved(_) => {}
            ThreadStoreEvent::LocalThreadsChangedExternally => self.reload(cx),
        }
    }

    fn reload_remote_host(
        &mut self,
        thread_store: &Entity<ThreadStore>,
        host_id: EntityId,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = thread_store
            .read(cx)
            .remote_storage(host_id)
            .map(|storage| storage.connection_options().clone())
        else {
            return;
        };
        // Retry writes made while the host wasn't connected.
        let (stalled, still_stalled) = std::mem::take(&mut self.stalled_remote_operations)
            .into_iter()
            .partition::<Vec<_>, _>(|(stalled_connection, _)| {
                same_remote_connection_identity(Some(stalled_connection), Some(&connection))
            });
        self.stalled_remote_operations = still_stalled;
        self.remote_operations.extend(stalled);
        self.remote_operations.push_back((
            connection,
            RemoteDbOperation::List {
                mutation_count: self.mutation_count,
            },
        ));
        self.remote_operations_tx.try_send(()).log_err();
    }

    /// Queues a write to a remote host, made at the current mutation count,
    /// and keeps it in the outbox until the host confirms it.
    fn queue_remote_write(&mut self, connection: RemoteConnectionOptions, write: RemoteWrite) {
        let mutation = self.mutation_count;
        self.pending_remote_writes.insert(
            write.terminal_id(),
            (connection.clone(), write.clone(), mutation),
        );
        self.remote_operations
            .push_back((connection, RemoteDbOperation::Write { write, mutation }));
        self.remote_operations_tx.try_send(()).log_err();
    }

    fn acknowledge_remote_write(&mut self, terminal_id: TerminalId, mutation: u64, cx: &App) {
        if self
            .pending_remote_writes
            .get(&terminal_id)
            .is_some_and(|(_, _, pending_mutation)| *pending_mutation == mutation)
        {
            self.pending_remote_writes.remove(&terminal_id);
            self.schedule_persist_remote_state(cx);
        }
    }

    fn load_remote_state(&mut self, cx: &App) {
        // The key-value store is shared by all tests in a process.
        if cfg!(any(test, feature = "test-support")) {
            return;
        }
        let kvp = KeyValueStore::global(cx);
        if let Some(cache) = kvp.read_kvp(REMOTE_TERMINALS_CACHE_KEY).log_err().flatten() {
            self.apply_remote_terminals_cache(&cache).log_err();
        }
        if let Some(outbox) = kvp
            .read_kvp(REMOTE_TERMINALS_OUTBOX_KEY)
            .log_err()
            .flatten()
        {
            self.apply_remote_terminals_outbox(&outbox).log_err();
        }
    }

    /// Applies writes the hosts hadn't confirmed when Zed last quit, and
    /// queues them to be sent once the hosts are connected.
    fn apply_remote_terminals_outbox(&mut self, outbox: &str) -> anyhow::Result<()> {
        for persisted in serde_json::from_str::<Vec<PersistedRemoteWrite>>(outbox)? {
            let connection = persisted.connection;
            let write = match (persisted.upsert, persisted.delete) {
                (Some(row), _) => {
                    let Some(mut metadata) = terminal_from_proto(row).log_err() else {
                        continue;
                    };
                    metadata.remote_connection = Some(connection.clone());
                    RemoteWrite::Upsert(metadata)
                }
                (None, Some(terminal_id)) => {
                    let Some(terminal_id) = TerminalId::from_key_string(&terminal_id).log_err()
                    else {
                        continue;
                    };
                    RemoteWrite::Delete(terminal_id)
                }
                (None, None) => continue,
            };
            let terminal_id = write.terminal_id();
            self.uncache_terminal(terminal_id);
            match &write {
                RemoteWrite::Upsert(metadata) => {
                    self.remote_terminals
                        .insert(terminal_id, connection.clone());
                    self.cache_terminal_metadata(metadata.clone());
                }
                RemoteWrite::Delete(_) => {
                    self.remote_terminals.remove(&terminal_id);
                }
            }
            self.mutation_count += 1;
            self.last_mutation.insert(terminal_id, self.mutation_count);
            self.queue_remote_write(connection, write);
        }
        Ok(())
    }

    fn uncache_terminal(&mut self, terminal_id: TerminalId) {
        let Some(terminal) = self.terminals.remove(&terminal_id) else {
            return;
        };
        if let Some(ids) = self.terminals_by_paths.get_mut(terminal.folder_paths()) {
            ids.remove(&terminal_id);
        }
        if let Some(ids) = self
            .terminals_by_main_paths
            .get_mut(terminal.main_worktree_paths())
        {
            ids.remove(&terminal_id);
        }
    }

    fn remote_terminals_outbox(&self) -> anyhow::Result<String> {
        let writes = self
            .pending_remote_writes
            .values()
            .map(|(connection, write, _)| PersistedRemoteWrite {
                connection: connection.clone(),
                upsert: match write {
                    RemoteWrite::Upsert(metadata) => Some(terminal_to_proto(metadata)),
                    RemoteWrite::Delete(_) => None,
                },
                delete: match write {
                    RemoteWrite::Upsert(_) => None,
                    RemoteWrite::Delete(terminal_id) => Some(terminal_id.to_key_string()),
                },
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_string(&writes)?)
    }

    fn apply_remote_terminals_cache(&mut self, cache: &str) -> anyhow::Result<()> {
        for host in serde_json::from_str::<Vec<CachedRemoteTerminals>>(cache)? {
            let rows = host
                .terminals
                .into_iter()
                .filter_map(|row| terminal_from_proto(row).log_err())
                .collect();
            self.apply_remote_rows(&host.connection, rows, self.mutation_count);
        }
        Ok(())
    }

    /// Saves the terminals of remote hosts as currently known, and the writes
    /// they haven't confirmed, shortly after they stop changing.
    fn schedule_persist_remote_state(&mut self, cx: &App) {
        let persist = self.persist_remote_state(cx);
        let executor = cx.background_executor().clone();
        self._persist_remote_state = Some(cx.background_spawn(async move {
            executor.timer(PERSIST_REMOTE_STATE_DELAY).await;
            persist.await;
        }));
    }

    fn persist_remote_state(&self, cx: &App) -> impl Future<Output = ()> + use<> {
        let cache = self.remote_terminals_cache().log_err();
        let outbox = self.remote_terminals_outbox().log_err();
        let kvp = KeyValueStore::global(cx);
        // The key-value store is shared by all tests in a process.
        let skip = cfg!(any(test, feature = "test-support"));
        async move {
            if skip {
                return;
            }
            if let Some(cache) = cache {
                kvp.write_kvp(REMOTE_TERMINALS_CACHE_KEY.to_string(), cache)
                    .await
                    .log_err();
            }
            if let Some(outbox) = outbox {
                kvp.write_kvp(REMOTE_TERMINALS_OUTBOX_KEY.to_string(), outbox)
                    .await
                    .log_err();
            }
        }
    }

    /// Writes any state still waiting for its debounce before Zed quits.
    fn flush_remote_state_on_quit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let pending = self
            ._persist_remote_state
            .take()
            .is_some_and(|task| !task.is_ready());
        let persist = pending.then(|| self.persist_remote_state(cx));
        async move {
            if let Some(persist) = persist {
                persist.await;
            }
        }
    }

    fn remote_terminals_cache(&self) -> anyhow::Result<String> {
        let mut hosts: Vec<CachedRemoteTerminals> = Vec::new();
        for (terminal_id, connection) in &self.remote_terminals {
            let Some(terminal) = self.terminals.get(terminal_id) else {
                continue;
            };
            let host_ix = match hosts.iter().position(|host| {
                same_remote_connection_identity(Some(&host.connection), Some(connection))
            }) {
                Some(ix) => ix,
                None => {
                    hosts.push(CachedRemoteTerminals {
                        connection: connection.clone(),
                        terminals: Vec::new(),
                    });
                    hosts.len() - 1
                }
            };
            hosts[host_ix].terminals.push(terminal_to_proto(terminal));
        }
        Ok(serde_json::to_string(&hosts)?)
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            if ops.contains_key(&operation.id()) {
                continue;
            }
            ops.insert(operation.id(), operation);
        }
        ops.into_values().collect()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = oneshot::channel();
        self.pending_terminal_ops_tx
            .try_send(DbRequest::List(tx))
            .log_err();
        let mutation_count = self.mutation_count;
        self.reload_task = Some(
            cx.spawn(async move |this, cx| {
                let Some(rows) = rx
                    .await
                    .context("terminal thread metadata database worker stopped")
                    .and_then(|rows| rows)
                    .context("Failed to fetch terminal thread metadata")
                    .log_err()
                else {
                    return;
                };

                this.update(cx, |this, cx| {
                    this.apply_local_rows(rows, mutation_count);
                    cx.notify();
                })
                .ok();
            })
            .shared(),
        );
    }
}

/// Converts metadata to the form it is stored in on a remote host. Mirrors
/// [`TerminalThreadMetadataDb::save`]; the host stores no remote connection,
/// since the terminal is local to it.
fn terminal_to_proto(metadata: &TerminalThreadMetadata) -> proto::AgentTerminalThread {
    let serialize_paths = |paths: &PathList| {
        if paths.is_empty() {
            (None, None)
        } else {
            let serialized = paths.serialize();
            (Some(serialized.paths), Some(serialized.order))
        }
    };
    let (folder_paths, folder_paths_order) = serialize_paths(metadata.folder_paths());
    let (main_worktree_paths, main_worktree_paths_order) =
        serialize_paths(metadata.main_worktree_paths());
    proto::AgentTerminalThread {
        terminal_id: metadata.terminal_id.to_key_string(),
        title: metadata.title.to_string(),
        custom_title: metadata.custom_title.as_ref().map(ToString::to_string),
        created_at: metadata.created_at.to_rfc3339(),
        working_directory: metadata
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        folder_paths,
        folder_paths_order,
        main_worktree_paths,
        main_worktree_paths_order,
        archived: metadata.archived,
    }
}

/// Converts metadata stored on a remote host. Mirrors the [`Column`]
/// implementation of [`TerminalThreadMetadata`].
fn terminal_from_proto(row: proto::AgentTerminalThread) -> anyhow::Result<TerminalThreadMetadata> {
    let deserialize_paths = |paths: Option<String>, order: Option<String>| {
        paths
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: order.unwrap_or_default(),
                })
            })
            .unwrap_or_default()
    };
    let folder_paths = deserialize_paths(row.folder_paths, row.folder_paths_order);
    let main_worktree_paths =
        deserialize_paths(row.main_worktree_paths, row.main_worktree_paths_order);
    let worktree_paths = WorktreePaths::from_path_lists(main_worktree_paths, folder_paths)
        .unwrap_or_else(|_| WorktreePaths::default());

    Ok(TerminalThreadMetadata {
        terminal_id: TerminalId::from_key_string(&row.terminal_id)?,
        title: SharedString::from(row.title),
        custom_title: row
            .custom_title
            .filter(|title| !title.trim().is_empty())
            .map(SharedString::from),
        created_at: DateTime::parse_from_rfc3339(&row.created_at)?.with_timezone(&Utc),
        worktree_paths,
        remote_connection: None,
        working_directory: row.working_directory.map(PathBuf::from),
        archived: row.archived,
    })
}

struct TerminalThreadMetadataDb(ThreadSafeConnection);

impl Domain for TerminalThreadMetadataDb {
    const NAME: &str = stringify!(TerminalThreadMetadataDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS sidebar_terminal_threads(
                terminal_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                custom_title TEXT,
                created_at TEXT NOT NULL,
                working_directory TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                main_worktree_paths TEXT,
                main_worktree_paths_order TEXT,
                remote_connection TEXT
            ) STRICT;
        ),
        sql!(ALTER TABLE sidebar_terminal_threads ADD COLUMN archived INTEGER DEFAULT 0),
    ];
}

db::static_connection!(TerminalThreadMetadataDb, []);

impl TerminalThreadMetadataDb {
    pub fn list(&self) -> anyhow::Result<Vec<TerminalThreadMetadata>> {
        self.select::<TerminalThreadMetadata>(
            "SELECT terminal_id, title, custom_title, created_at, \
            working_directory, folder_paths, folder_paths_order, main_worktree_paths, \
            main_worktree_paths_order, remote_connection, archived \
            FROM sidebar_terminal_threads \
            ORDER BY created_at DESC",
        )?()
    }

    pub async fn save(&self, row: TerminalThreadMetadata) -> anyhow::Result<()> {
        let terminal_id = row.terminal_id.to_key_string();
        let title = row.title.to_string();
        let custom_title = row.custom_title.as_ref().map(ToString::to_string);
        let created_at = row.created_at.to_rfc3339();
        let working_directory = row
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let serialized = row.folder_paths().serialize();
        let (folder_paths, folder_paths_order) = if row.folder_paths().is_empty() {
            (None, None)
        } else {
            (Some(serialized.paths), Some(serialized.order))
        };
        let main_serialized = row.main_worktree_paths().serialize();
        let (main_worktree_paths, main_worktree_paths_order) =
            if row.main_worktree_paths().is_empty() {
                (None, None)
            } else {
                (Some(main_serialized.paths), Some(main_serialized.order))
            };
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize terminal thread remote connection")?;
        let archived = row.archived;

        self.write(move |conn| {
            let sql = "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, created_at, working_directory, folder_paths, folder_paths_order, main_worktree_paths, main_worktree_paths_order, remote_connection, archived) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           title = excluded.title, \
                           custom_title = excluded.custom_title, \
                           created_at = excluded.created_at, \
                           working_directory = excluded.working_directory, \
                           folder_paths = excluded.folder_paths, \
                           folder_paths_order = excluded.folder_paths_order, \
                           main_worktree_paths = excluded.main_worktree_paths, \
                           main_worktree_paths_order = excluded.main_worktree_paths_order, \
                           remote_connection = excluded.remote_connection, \
                           archived = excluded.archived";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&custom_title, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&working_directory, i)?;
            i = stmt.bind(&folder_paths, i)?;
            i = stmt.bind(&folder_paths_order, i)?;
            i = stmt.bind(&main_worktree_paths, i)?;
            i = stmt.bind(&main_worktree_paths_order, i)?;
            i = stmt.bind(&remote_connection, i)?;
            stmt.bind(&archived, i)?;
            stmt.exec()
        })
        .await
    }

    pub async fn delete(&self, terminal_id: TerminalId) -> anyhow::Result<()> {
        let terminal_id = terminal_id.to_key_string();
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "DELETE FROM sidebar_terminal_threads WHERE terminal_id = ?",
            )?;
            stmt.bind(&terminal_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TerminalThreadMetadata {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (archived, next): (bool, i32) = Column::column(statement, next)?;

        let folder_paths = folder_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: folder_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let main_worktree_paths = main_worktree_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: main_worktree_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize terminal thread remote connection")?;

        let worktree_paths = WorktreePaths::from_path_lists(main_worktree_paths, folder_paths)
            .unwrap_or_else(|_| WorktreePaths::default());

        Ok((
            TerminalThreadMetadata {
                terminal_id: TerminalId::from_key_string(&terminal_id)?,
                title: SharedString::from(title),
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                worktree_paths,
                remote_connection,
                working_directory: working_directory.map(PathBuf::from),
                archived,
            },
            next,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    fn metadata(title: &str, worktree_paths: WorktreePaths) -> TerminalThreadMetadata {
        let now = Utc::now();
        TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: SharedString::from(title.to_string()),
            custom_title: None,
            created_at: now,
            worktree_paths,
            remote_connection: None,
            working_directory: None,
            archived: false,
        }
    }

    #[test]
    fn test_terminal_title_prefix_preserves_non_alphanumeric_prefixes() {
        assert_eq!(terminal_title_prefix("✳ Thinking"), Some("✳ "));
        assert_eq!(terminal_title_prefix(">>>   Thinking"), Some(">>>   "));
        assert_eq!(terminal_title_prefix("⠋ Running"), Some("⠋ "));
        assert_eq!(terminal_title_prefix("* Claude"), Some("* "));
        assert_eq!(terminal_title_prefix("✳Thinking"), None);
        assert_eq!(terminal_title_prefix("Thinking"), None);
        assert_eq!(terminal_title_prefix(" Thinking"), None);
        assert_eq!(terminal_title_prefix("✳"), None);
        assert_eq!(terminal_title_prefix("v1 Running"), None);
    }

    #[test]
    fn test_terminal_thread_display_title_combines_raw_and_custom_titles() {
        let mut metadata = metadata(
            "⠋ Thinking",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.custom_title = Some("Fix bug".into());
        assert_eq!(metadata.display_title().as_ref(), "⠋ Fix bug");

        metadata.title = "Thinking".into();
        assert_eq!(metadata.display_title().as_ref(), "Fix bug");
    }

    #[gpui::test]
    async fn test_rename_terminal_updates_stored_custom_title(cx: &mut TestAppContext) {
        init_test(cx);

        let metadata = metadata(
            "⠋ Dev Server",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
                store.rename_terminal(terminal_id, "Renamed Terminal".into(), cx);
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let metadata = store
                .read(cx)
                .entry(terminal_id)
                .expect("renamed terminal metadata should exist");
            assert_eq!(metadata.custom_title.as_deref(), Some("Renamed Terminal"));
            assert_eq!(metadata.display_title().as_ref(), "⠋ Renamed Terminal");
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.rename_terminal(terminal_id, "Dev Server".into(), cx);
            });
            let store = TerminalThreadMetadataStore::global(cx);
            let metadata = store
                .read(cx)
                .entry(terminal_id)
                .expect("renamed terminal metadata should exist");
            assert_eq!(metadata.custom_title, None);
            assert_eq!(metadata.display_title().as_ref(), "⠋ Dev Server");
        });
    }

    #[gpui::test]
    async fn test_archive_hides_terminal_but_keeps_worktree_referenced(cx: &mut TestAppContext) {
        init_test(cx);

        let folder_paths = PathList::new(&[Path::new("/repo-feature")]);
        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_folder_paths(&folder_paths),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
                store.archive(terminal_id, cx);
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(store.entry(terminal_id).unwrap().archived);
            assert!(store.entries_for_path(&folder_paths, None).next().is_none());
            assert_eq!(
                store
                    .archived_entries()
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
            assert!(store.path_is_referenced_by_terminal(None, Path::new("/repo-feature"), None));
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.unarchive(terminal_id, cx);
            });
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(!store.entry(terminal_id).unwrap().archived);
            assert_eq!(
                store
                    .entries_for_path(&folder_paths, None)
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
        });
    }

    #[gpui::test]
    async fn test_archived_flag_persists_across_reload(cx: &mut TestAppContext) {
        init_test(cx);

        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
                store.archive(terminal_id, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.reload(cx));
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            assert!(store.read(cx).entry(terminal_id).unwrap().archived);
        });
    }

    #[gpui::test]
    async fn test_terminals_listed_by_remote_host_are_found_by_path(cx: &mut TestAppContext) {
        init_test(cx);

        let paths = PathList::new(&[Path::new("/Users/me/Projects/app")]);
        let remote = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 1 });
        let row = terminal_from_proto(terminal_to_proto(&metadata(
            "✳ Host Terminal",
            WorktreePaths::from_folder_paths(&paths),
        )))
        .unwrap();
        let terminal_id = row.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.apply_remote_rows(&remote, vec![row], store.mutation_count);
                cx.notify();
            })
        });
        cx.run_until_parked();

        let found = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                let store = TerminalThreadMetadataStore::global(cx);
                let store = store.read(cx);
                (
                    store
                        .entries_for_path(&paths, Some(&remote))
                        .map(|terminal| terminal.terminal_id)
                        .collect::<Vec<_>>(),
                    store
                        .entries_for_path(&paths, None)
                        .map(|terminal| terminal.terminal_id)
                        .collect::<Vec<_>>(),
                )
            })
        };
        assert_eq!(found(cx), (vec![terminal_id], vec![]));

        // Reloading the local database keeps the host's terminals.
        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.reload(cx))
        });
        cx.run_until_parked();
        assert_eq!(found(cx), (vec![terminal_id], vec![]));
    }

    #[gpui::test]
    async fn test_terminal_writes_to_disconnected_host_survive_relaunch(cx: &mut TestAppContext) {
        init_test(cx);

        let paths = PathList::new(&[Path::new("/Users/me/Projects/app")]);
        let remote = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 9 });
        let host_terminal = metadata("✳ Host Terminal", WorktreePaths::from_folder_paths(&paths));
        let terminal_id = host_terminal.terminal_id;

        let (cache, outbox) = cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.apply_remote_rows(&remote, vec![host_terminal], store.mutation_count);
                store.rename_terminal(terminal_id, "Renamed".into(), cx);
                (
                    store.remote_terminals_cache().unwrap(),
                    store.remote_terminals_outbox().unwrap(),
                )
            })
        });
        cx.run_until_parked();

        let restored = cx.update(|cx| {
            cx.new(|cx| {
                let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(
                    "test_terminal_writes_to_disconnected_host_survive_relaunch",
                ));
                let mut store = TerminalThreadMetadataStore::new(TerminalThreadMetadataDb(db), cx);
                store.apply_remote_terminals_cache(&cache).unwrap();
                store.apply_remote_terminals_outbox(&outbox).unwrap();
                store
            })
        });
        cx.run_until_parked();
        restored.read_with(cx, |store, _| {
            assert_eq!(
                store.entry(terminal_id).unwrap().custom_title.as_deref(),
                Some("Renamed")
            );
            assert!(store.pending_remote_writes.contains_key(&terminal_id));
            assert!(matches!(
                store.stalled_remote_operations.as_slice(),
                [(_, RemoteDbOperation::Write { write: RemoteWrite::Upsert(metadata), .. })]
                    if metadata.terminal_id == terminal_id
            ));
        });
    }

    #[gpui::test]
    async fn test_remote_terminals_cache_lists_terminals_before_host_connects(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let paths = PathList::new(&[Path::new("/Users/me/Projects/app")]);
        let remote = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 7 });
        let host_terminal = metadata("✳ Host Terminal", WorktreePaths::from_folder_paths(&paths));
        let terminal_id = host_terminal.terminal_id;

        let cache = cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, _| {
                store.apply_remote_rows(&remote, vec![host_terminal], store.mutation_count);
                store.remote_terminals_cache().unwrap()
            })
        });

        let restored = cx.update(|cx| {
            cx.new(|cx| {
                let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(
                    "test_remote_terminals_cache_lists_terminals_before_host_connects",
                ));
                let mut store = TerminalThreadMetadataStore::new(TerminalThreadMetadataDb(db), cx);
                store.apply_remote_terminals_cache(&cache).unwrap();
                store
            })
        });
        cx.run_until_parked();
        restored.read_with(cx, |store, _| {
            let terminal = store
                .entry(terminal_id)
                .expect("cached terminal should be listed");
            assert_eq!(terminal.title.as_ref(), "✳ Host Terminal");
            assert!(same_remote_connection_identity(
                terminal.remote_connection.as_ref(),
                Some(&remote)
            ));
            assert!(store.remote_terminals.contains_key(&terminal_id));
        });
    }

    #[gpui::test]
    async fn test_change_worktree_paths_reindexes_terminal_metadata(cx: &mut TestAppContext) {
        init_test(cx);

        let old_main_paths = PathList::new(&[Path::new("/repo")]);
        let old_folder_paths = PathList::new(&[Path::new("/repo-feature")]);
        let new_main_path = Path::new("/repo");
        let new_folder_path = Path::new("/repo-feature-renamed");
        let new_folder_paths = PathList::new(&[new_folder_path]);
        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_path_lists(old_main_paths.clone(), old_folder_paths.clone())
                .unwrap(),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.change_worktree_paths(
                    &old_folder_paths,
                    None,
                    |paths| {
                        paths.add_path(new_main_path, new_folder_path);
                        paths.remove_folder_path(Path::new("/repo-feature"));
                    },
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(
                store
                    .entries_for_path(&old_folder_paths, None)
                    .next()
                    .is_none()
            );
            assert_eq!(
                store
                    .entries_for_path(&new_folder_paths, None)
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
            assert_eq!(
                store
                    .entry(terminal_id)
                    .unwrap()
                    .main_worktree_paths()
                    .paths(),
                old_main_paths.paths()
            );
        });
    }
}
