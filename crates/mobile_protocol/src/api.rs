//! The API spoken over the WebSocket between the mobile PWA and Zed. The
//! TypeScript mirror lives in `crates/mobile_server/web/src/protocol.ts` and
//! must be kept in sync with this file.
//!
//! Framing (one JSON object per WebSocket text frame):
//! - client → Zed: `{"id": number, "method": string, "params": object}`
//! - Zed → client: `{"id": number, "result": any}` or `{"id": number, "error": string}`
//! - Zed → client: `{"event": string, "data": any}` for unsolicited updates
//!
//! All text positions are UTF-16 code unit offsets (what JavaScript strings and
//! CodeMirror use). Document changes use CodeMirror's `ChangeSet.toJSON()`
//! format: an array where a number `n` retains `n` code units, `[n]` deletes
//! `n` code units, and `[n, line0, line1, ...]` replaces `n` code units with
//! the given lines joined by `\n`.

use serde::{Deserialize, Serialize};

pub type WorkspaceId = u64;
pub type ItemId = u64;
pub type BufferHandle = u64;
pub type RepositoryId = u64;
pub type WorktreeId = u64;

#[derive(Debug, Deserialize)]
pub struct IncomingRequest {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    // Threads list screen
    #[serde(rename = "threads.list")]
    ListThreads {},
    #[serde(rename = "threads.new_thread_options")]
    NewThreadOptions {},
    #[serde(rename = "threads.create")]
    CreateThread(CreateThreadParams),

    // Thread screen
    /// Shows the thread in Zed (loading it if needed), returns a snapshot and
    /// subscribes the client to `thread.*` events until `thread.close`.
    #[serde(rename = "thread.open")]
    OpenThread { thread_id: String },
    #[serde(rename = "thread.close")]
    CloseThread { thread_id: String },
    /// Sends a message, or queues it if the agent is generating (like Zed does).
    #[serde(rename = "thread.send")]
    SendMessage { thread_id: String, text: String },
    #[serde(rename = "thread.cancel")]
    CancelGeneration { thread_id: String },
    #[serde(rename = "thread.authorize")]
    AuthorizeToolCall {
        thread_id: String,
        tool_call_id: String,
        option_id: String,
    },
    #[serde(rename = "thread.set_control")]
    SetThreadControl {
        thread_id: String,
        control_id: String,
        value: ControlValue,
    },
    #[serde(rename = "thread.remove_queued")]
    RemoveQueuedMessage { thread_id: String, index: usize },

    // Tabs screen
    /// Returns the editor tabs and terminals of a workspace. When
    /// `workspace_id` is omitted, uses the active workspace of the frontmost
    /// window. Subscribes the client to `workspace.items_changed`.
    #[serde(rename = "workspace.items")]
    WorkspaceItems { workspace_id: Option<WorkspaceId> },
    #[serde(rename = "workspace.close_item")]
    CloseItem {
        workspace_id: WorkspaceId,
        item_id: ItemId,
    },
    #[serde(rename = "workspace.new_terminal")]
    NewTerminal { workspace_id: WorkspaceId },
    /// Lists a directory of the workspace's worktrees. With `path` empty and
    /// `worktree_id` omitted, lists the visible worktree roots.
    #[serde(rename = "workspace.list_directory")]
    ListDirectory {
        workspace_id: WorkspaceId,
        worktree_id: Option<WorktreeId>,
        path: String,
    },
    /// Opens a file as a tab in Zed. Returns `OpenedItem`.
    #[serde(rename = "workspace.open_file")]
    OpenFile {
        workspace_id: WorkspaceId,
        worktree_id: WorktreeId,
        path: String,
    },

    // Git
    /// Commits of the workspace's active repository, newest first. The PWA
    /// prepends its own "Uncommitted changes" entry.
    #[serde(rename = "git.log")]
    GitLog {
        workspace_id: WorkspaceId,
        offset: usize,
        limit: usize,
    },
    /// Opens Zed's uncommitted changes multibuffer as a tab. Returns `OpenedItem`.
    #[serde(rename = "git.open_uncommitted")]
    OpenUncommittedChanges { workspace_id: WorkspaceId },
    /// Opens a commit as a tab. Returns `OpenedItem`.
    #[serde(rename = "git.open_commit")]
    OpenCommit {
        workspace_id: WorkspaceId,
        sha: String,
    },
    #[serde(rename = "git.stage")]
    StagePaths {
        workspace_id: WorkspaceId,
        repository_id: RepositoryId,
        paths: Vec<String>,
    },
    #[serde(rename = "git.unstage")]
    UnstagePaths {
        workspace_id: WorkspaceId,
        repository_id: RepositoryId,
        paths: Vec<String>,
    },

    // Full-screen item views
    /// Returns the content of an editor-like tab and subscribes to the
    /// buffers it contains. Returns `ItemContent`.
    #[serde(rename = "item.open")]
    OpenItem {
        workspace_id: WorkspaceId,
        item_id: ItemId,
    },
    /// Releases the buffer subscriptions taken by `item.open`.
    #[serde(rename = "item.close")]
    CloseItemView {
        workspace_id: WorkspaceId,
        item_id: ItemId,
    },

    // Buffers (see `BufferState`)
    /// Applies local edits made on top of `version`. Returns `PushResult`.
    /// Rejected when `version` is behind; the client then waits for the
    /// missing `buffer.updates`, rebases and pushes again.
    #[serde(rename = "buffer.push")]
    PushBufferUpdates {
        buffer: BufferHandle,
        version: u64,
        updates: Vec<ClientUpdate>,
    },
    /// Returns `Highlights` for the given UTF-16 range of the buffer at the
    /// current version.
    #[serde(rename = "buffer.highlights")]
    BufferHighlights {
        buffer: BufferHandle,
        from: usize,
        to: usize,
    },
    /// Returns `Vec<Completion>`. `offset` refers to `version`.
    #[serde(rename = "buffer.completions")]
    Completions {
        buffer: BufferHandle,
        version: u64,
        offset: usize,
    },
    /// Applies a completion returned by the last `buffer.completions` call,
    /// including additional edits like auto-imports. The edits arrive as
    /// `buffer.updates`.
    #[serde(rename = "buffer.apply_completion")]
    ApplyCompletion {
        buffer: BufferHandle,
        completion_id: u64,
    },
    /// Returns `Option<Hover>`.
    #[serde(rename = "buffer.hover")]
    Hover {
        buffer: BufferHandle,
        version: u64,
        offset: usize,
    },
    /// Navigates to the definition in Zed. Returns `Option<OpenedItem>` for the
    /// tab that shows it, plus the target offset.
    #[serde(rename = "buffer.definition")]
    GoToDefinition {
        workspace_id: WorkspaceId,
        buffer: BufferHandle,
        version: u64,
        offset: usize,
    },
    #[serde(rename = "buffer.format")]
    FormatBuffer { buffer: BufferHandle },
    #[serde(rename = "buffer.save")]
    SaveBuffer { buffer: BufferHandle },

    // Terminals
    /// Subscribes to `terminal.screen` events for a terminal item and returns
    /// the current `TerminalScreen`.
    #[serde(rename = "terminal.open")]
    OpenTerminal {
        workspace_id: WorkspaceId,
        item_id: ItemId,
    },
    #[serde(rename = "terminal.close")]
    CloseTerminal {
        workspace_id: WorkspaceId,
        item_id: ItemId,
    },
    /// Writes raw input (already encoded as the bytes a terminal expects,
    /// e.g. "\r" for Enter, "\x1b[A" for Up) to the terminal.
    #[serde(rename = "terminal.input")]
    TerminalInput {
        workspace_id: WorkspaceId,
        item_id: ItemId,
        data: String,
    },
    /// Scrolls the terminal viewport; positive is up (into history).
    #[serde(rename = "terminal.scroll")]
    TerminalScroll {
        workspace_id: WorkspaceId,
        item_id: ItemId,
        lines: i32,
    },
}

#[derive(Debug, Serialize)]
pub struct OutgoingResponse<T: Serialize> {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: ResponseOutcome<T>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome<T: Serialize> {
    Result(T),
    Error(String),
}

#[derive(Debug, Serialize)]
pub struct OutgoingEvent<'a, T: Serialize> {
    pub event: &'a str,
    pub data: T,
}

/// Event names, each listed with the type of its `data`.
pub mod events {
    /// `Hello`, sent once right after the WebSocket is authenticated.
    pub const HELLO: &str = "hello";
    /// `Theme`, sent when Zed's theme changes.
    pub const THEME: &str = "theme";
    /// `ThreadsList`, sent whenever the threads list changes.
    pub const THREADS_CHANGED: &str = "threads.changed";
    /// `ThreadEntryEvent`
    pub const THREAD_ENTRY: &str = "thread.entry";
    /// `ThreadEntriesRemovedEvent`
    pub const THREAD_ENTRIES_REMOVED: &str = "thread.entries_removed";
    /// `ThreadState`
    pub const THREAD_STATE: &str = "thread.state";
    /// `WorkspaceItemsChangedEvent`; the client should re-fetch `workspace.items`.
    pub const WORKSPACE_ITEMS_CHANGED: &str = "workspace.items_changed";
    /// `BufferUpdatesEvent`
    pub const BUFFER_UPDATES: &str = "buffer.updates";
    /// `BufferMetadataEvent`
    pub const BUFFER_METADATA: &str = "buffer.metadata";
    /// `BufferDiagnosticsEvent`
    pub const BUFFER_DIAGNOSTICS: &str = "buffer.diagnostics";
    /// `BufferReparsedEvent`; the client should re-request highlights.
    pub const BUFFER_REPARSED: &str = "buffer.reparsed";
    /// `DiffBaseChangedEvent`
    pub const DIFF_BASE_CHANGED: &str = "diff.base_changed";
    /// `DiffFilesChangedEvent`; the client should re-open the item.
    pub const DIFF_FILES_CHANGED: &str = "diff.files_changed";
    /// `TerminalScreen`
    pub const TERMINAL_SCREEN: &str = "terminal.screen";
    /// `TerminalClosedEvent`
    pub const TERMINAL_CLOSED: &str = "terminal.closed";
    /// `ToastEvent`, for errors from background operations.
    pub const TOAST: &str = "toast";
}

#[derive(Debug, Clone, Serialize)]
pub struct Hello {
    /// Identifies this connection in `ServerUpdate::client_id`, so the client
    /// can recognize its own updates echoed back.
    pub client_id: String,
    pub zed_version: String,
    pub theme: Theme,
}

#[derive(Debug, Clone, Serialize)]
pub struct Theme {
    pub appearance: Appearance,
    /// CSS colors (`#rrggbbaa`) keyed by role, e.g. "background",
    /// "surface", "elevated_surface", "border", "text", "text_muted",
    /// "text_accent", "accent", "error", "warning", "success", "info",
    /// "editor_background", "editor_foreground", "editor_line_number",
    /// "editor_active_line", "editor_selection", "editor_cursor",
    /// "diff_added", "diff_deleted", "diff_modified", "diff_added_background",
    /// "diff_deleted_background", "terminal_background",
    /// "terminal_foreground", "terminal_cursor".
    pub colors: std::collections::BTreeMap<String, String>,
    /// Indexed by `HighlightToken::2`.
    pub syntax: Vec<SyntaxStyle>,
    /// The 16 ANSI colors followed by bright variants, used for terminals.
    pub terminal_ansi: Vec<String>,
    pub buffer_font_family: String,
    pub ui_font_family: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyntaxStyle {
    pub color: Option<String>,
    pub background: Option<String>,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToastEvent {
    pub level: ToastLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastLevel {
    Info,
    Error,
}

// ---------------------------------------------------------------------------
// Threads list

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadsList {
    pub projects: Vec<ProjectThreads>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProjectThreads {
    /// Stable key for the project group, used in `CreateThreadParams`.
    pub project_id: String,
    pub name: String,
    /// The SSH/WSL/dev-container host for remote projects.
    pub remote_host: Option<String>,
    pub is_active: bool,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub kind: ThreadKind,
    pub title: String,
    pub status: ThreadStatus,
    pub agent_name: Option<String>,
    /// Labels for the worktrees the thread runs in, e.g. a linked worktree's
    /// branch name. Empty when it runs in the project's main worktree.
    pub worktrees: Vec<String>,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub has_notification: bool,
    pub is_active: bool,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Agent,
    /// Terminal threads are listed but not yet controllable from mobile.
    Terminal,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Running,
    WaitingForConfirmation,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewThreadOptions {
    pub projects: Vec<NewThreadProject>,
    pub agents: Vec<AgentOption>,
    /// The agent Zed would pick for a new thread.
    pub default_agent_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewThreadProject {
    pub project_id: String,
    pub name: String,
    pub remote_host: Option<String>,
    /// Existing git worktrees (main first). Empty for non-git projects.
    pub worktrees: Vec<WorktreeOption>,
    /// Whether a new git worktree can be created for this project.
    pub can_create_worktree: bool,
    /// Branches a new worktree can be based on, current branch first.
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeOption {
    pub path: String,
    pub name: String,
    pub branch: Option<String>,
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOption {
    /// Zed's native agent has the id `"Zed Agent"`; other ids are external agents.
    pub id: String,
    pub name: String,
    pub is_external: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateThreadParams {
    pub project_id: String,
    pub worktree: WorktreeChoice,
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeChoice {
    /// One of `NewThreadProject::worktrees`, by path.
    Existing { path: String },
    /// Creates a new git worktree. `branch` is the branch to base it on
    /// (default: current branch); `name` is optional.
    New {
        name: Option<String>,
        branch: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedThread {
    pub thread_id: String,
}

// ---------------------------------------------------------------------------
// Thread view

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    /// The workspace that hosts the thread; the tabs screen shows its items.
    pub workspace_id: WorkspaceId,
    pub entries: Vec<ThreadEntry>,
    pub state: ThreadState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadState {
    pub thread_id: String,
    pub title: String,
    pub agent_name: String,
    pub status: ThreadStatus,
    pub controls: Vec<ThreadControl>,
    pub plan: Vec<PlanEntry>,
    pub queued_messages: Vec<String>,
    pub token_usage: Option<TokenUsage>,
    /// The last error (e.g. a failed request), shown as a callout.
    pub error: Option<String>,
    /// Whether the thread has unanswered prompts (elicitations) that can only
    /// be answered from Zed's desktop UI.
    pub needs_desktop_input: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TokenUsage {
    pub used: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// A selector or toggle in the thread's composer toolbar (model, mode,
/// profile, thinking, effort, fast mode, agent-provided config options).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadControl {
    /// e.g. "model", "mode", "profile", "thinking", "effort", "fast",
    /// or "config:<agent config id>".
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub kind: ThreadControlKind,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadControlKind {
    Select {
        value: Option<String>,
        options: Vec<ControlOption>,
    },
    Toggle {
        value: bool,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ControlOption {
    pub value: String,
    pub label: String,
    /// Groups options in the picker (e.g. a model's provider).
    pub group: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ControlValue {
    Toggle(bool),
    Select(String),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadEntry {
    User {
        /// Markdown, with mentions rendered as links.
        text: String,
    },
    Assistant {
        chunks: Vec<AssistantChunk>,
    },
    ToolCall(ToolCall),
    /// The conversation was compacted to save context.
    Compaction {
        summary: Option<String>,
    },
    /// A prompt from the agent that must be answered from Zed's desktop UI.
    Elicitation {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantChunk {
    Message { markdown: String },
    Thought { markdown: String },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    /// Markdown title, e.g. "Edit `src/main.rs`".
    pub title: String,
    /// ACP tool kind: read, edit, delete, move, search, execute, think,
    /// fetch, switch_mode, other.
    pub tool_kind: String,
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    /// Present while the tool call waits for the user's permission.
    pub permission_options: Option<Vec<PermissionOption>>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    WaitingForConfirmation,
    InProgress,
    Completed,
    Failed,
    Rejected,
    Canceled,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallContent {
    Markdown {
        markdown: String,
    },
    Diff {
        path: String,
        /// `None` for newly created files.
        old_text: Option<String>,
        new_text: String,
    },
    Terminal {
        command: Option<String>,
        output: String,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// allow_once, allow_always, reject_once, reject_always
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadEntryEvent {
    pub thread_id: String,
    pub index: usize,
    pub entry: ThreadEntry,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadEntriesRemovedEvent {
    pub thread_id: String,
    pub start: usize,
    pub end: usize,
}

// ---------------------------------------------------------------------------
// Tabs screen

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WorkspaceItems {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub editors: Vec<EditorTab>,
    pub terminals: Vec<TerminalTab>,
    pub active_item_id: Option<ItemId>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EditorTab {
    pub item_id: ItemId,
    pub kind: EditorTabKind,
    pub title: String,
    /// Project-relative path for file editors.
    pub path: Option<String>,
    pub is_dirty: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditorTabKind {
    /// A single file.
    File,
    /// Uncommitted (or branch) changes multibuffer.
    Diff,
    /// A commit's changes.
    Commit,
    /// Something mobile can't display (shown disabled).
    Other,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerminalTab {
    pub item_id: ItemId,
    pub title: String,
    pub has_bell: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceItemsChangedEvent {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryListing {
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntry {
    pub worktree_id: WorktreeId,
    /// Path relative to the worktree root (empty for the root itself).
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub is_ignored: bool,
    pub git_status: Option<FileStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenedItem {
    pub item_id: ItemId,
    pub kind: EditorTabKind,
    /// For go-to-definition: where to place the cursor (UTF-16 offset).
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitLog {
    pub repository_id: Option<RepositoryId>,
    pub branch: Option<String>,
    pub commits: Vec<CommitSummary>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitSummary {
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// Unix seconds.
    pub timestamp: i64,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
}

// ---------------------------------------------------------------------------
// Full-screen item views

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemContent {
    File {
        title: String,
        buffer: BufferState,
    },
    /// Uncommitted changes. Unlike Zed's desktop view, untracked files are
    /// always included.
    Diff {
        title: String,
        repository_id: RepositoryId,
        files: Vec<DiffFile>,
    },
    /// A commit's changes (read-only).
    Commit {
        title: String,
        sha: String,
        message: String,
        author: String,
        timestamp: i64,
        files: Vec<CommitFile>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffFile {
    pub path: String,
    pub status: FileStatus,
    pub is_staged: bool,
    pub is_partially_staged: bool,
    /// `None` for deleted files.
    pub buffer: Option<BufferState>,
    /// Text the buffer is compared against (`None` for new/untracked files).
    pub base_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitFile {
    pub path: String,
    pub status: FileStatus,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BufferState {
    pub buffer: BufferHandle,
    /// The sync version `text` corresponds to (see `buffer.push`).
    pub version: u64,
    pub text: String,
    pub path: Option<String>,
    pub language: Option<String>,
    pub read_only: bool,
    pub is_dirty: bool,
    pub has_lsp: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// Tab size, for indentation.
    pub tab_size: u32,
    pub hard_tabs: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientUpdate {
    /// `ChangeSet.toJSON()` relative to the document after the previous update.
    pub changes: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerUpdate {
    pub client_id: String,
    pub changes: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushResult {
    pub accepted: bool,
    /// The buffer's current version.
    pub version: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BufferUpdatesEvent {
    pub buffer: BufferHandle,
    /// The version the first update applies to. Updates are consecutive, so
    /// after applying them the client is at `version + updates.len()`.
    pub version: u64,
    pub updates: Vec<ServerUpdate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BufferMetadataEvent {
    pub buffer: BufferHandle,
    pub is_dirty: bool,
    pub language: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BufferDiagnosticsEvent {
    pub buffer: BufferHandle,
    pub version: u64,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BufferReparsedEvent {
    pub buffer: BufferHandle,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffBaseChangedEvent {
    pub buffer: BufferHandle,
    pub base_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffFilesChangedEvent {
    pub workspace_id: WorkspaceId,
    pub item_id: ItemId,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub from: usize,
    pub to: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Clone, Serialize)]
pub struct Highlights {
    pub version: u64,
    /// `[from, to, syntax_style_index]`, sorted and non-overlapping.
    pub tokens: Vec<(usize, usize, usize)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Completion {
    pub completion_id: u64,
    pub label: String,
    pub detail: Option<String>,
    /// LSP completion item kind name, e.g. "function", "variable".
    pub kind: Option<String>,
    /// Range replaced by `insert_text`, at the requested version.
    pub from: usize,
    pub to: usize,
    pub insert_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hover {
    pub markdown: String,
    pub from: usize,
    pub to: usize,
}

// ---------------------------------------------------------------------------
// Terminals

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerminalScreen {
    pub item_id: ItemId,
    pub title: String,
    pub columns: usize,
    pub rows: usize,
    /// One entry per visible row, each a list of styled runs.
    pub lines: Vec<Vec<TerminalRun>>,
    pub cursor: Option<TerminalCursor>,
    /// Lines scrolled back into history (0 = following output).
    pub display_offset: usize,
    pub history_size: usize,
    /// Whether the app enabled bracketed paste / application cursor keys,
    /// which affects how input should be encoded.
    pub bracketed_paste: bool,
    pub application_cursor: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerminalRun {
    pub text: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dim: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerminalCursor {
    pub row: usize,
    pub column: usize,
    pub shape: TerminalCursorShape,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCursorShape {
    Block,
    Underline,
    Bar,
    Hollow,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalClosedEvent {
    pub workspace_id: WorkspaceId,
    pub item_id: ItemId,
}
