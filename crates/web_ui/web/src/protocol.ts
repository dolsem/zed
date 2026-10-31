// TypeScript mirror of `crates/mobile_protocol/src/api.rs`, the WebSocket API
// between this PWA and Zed. Keep the two in sync. Field names are exactly what
// serde serializes (snake_case).
//
// Framing (one JSON object per WebSocket text frame):
// - client → Zed: `{"id": number, "method": string, "params": object}`
// - Zed → client: `{"id": number, "result": any}` or `{"id": number, "error": string}`
// - Zed → client: `{"event": string, "data": any}` for unsolicited updates
//
// All text positions are UTF-16 code unit offsets. Document changes use
// CodeMirror's `ChangeSet.toJSON()` format.

export type WorkspaceId = number;
export type ItemId = number;
export type BufferHandle = number;
export type RepositoryId = number;
export type WorktreeId = number;

// ---------------------------------------------------------------------------
// Framing

export interface OutgoingRequest<M extends MethodName = MethodName> {
  id: number;
  method: M;
  params: Methods[M]["params"];
}

export type IncomingFrame =
  | { id: number; result: unknown }
  | { id: number; error: string }
  | { event: string; data: unknown };

// ---------------------------------------------------------------------------
// Theme & general events

export interface Hello {
  /** Identifies this connection in `ServerUpdate.client_id`. */
  client_id: string;
  zed_version: string;
  theme: Theme;
}

export type Appearance = "light" | "dark";

export interface Theme {
  appearance: Appearance;
  /** CSS colors (`#rrggbbaa`) keyed by role. */
  colors: Record<string, string>;
  /** Indexed by `HighlightToken[2]`. */
  syntax: SyntaxStyle[];
  /** The 16 ANSI colors followed by bright variants. */
  terminal_ansi: string[];
  buffer_font_family: string;
  ui_font_family: string;
}

export interface SyntaxStyle {
  color: string | null;
  background: string | null;
  italic: boolean;
  bold: boolean;
  underline: boolean;
  strikethrough: boolean;
}

export type ToastLevel = "info" | "error";

export interface ToastEvent {
  level: ToastLevel;
  message: string;
}

// ---------------------------------------------------------------------------
// Threads list

export interface ThreadsList {
  projects: ProjectThreads[];
}

export interface ProjectThreads {
  project_id: string;
  name: string;
  remote_host: string | null;
  is_active: boolean;
  threads: ThreadSummary[];
}

export interface ThreadSummary {
  thread_id: string;
  kind: ThreadKind;
  title: string;
  status: ThreadStatus;
  agent_name: string | null;
  worktrees: string[];
  lines_added: number;
  lines_removed: number;
  has_notification: boolean;
  is_active: boolean;
  updated_at: string | null;
}

export type ThreadKind = "agent" | "terminal";

export type ThreadStatus = "idle" | "running" | "waiting_for_confirmation" | "error";

export interface NewThreadOptions {
  projects: NewThreadProject[];
  agents: AgentOption[];
  default_agent_id: string;
}

export interface NewThreadProject {
  project_id: string;
  name: string;
  remote_host: string | null;
  worktrees: WorktreeOption[];
  can_create_worktree: boolean;
  branches: string[];
}

export interface WorktreeOption {
  path: string;
  name: string;
  branch: string | null;
  is_main: boolean;
}

export interface AgentOption {
  /** `"zed"` for Zed's native agent, otherwise the external agent's id. */
  id: string;
  name: string;
  is_external: boolean;
}

export interface CreateThreadParams {
  project_id: string;
  worktree: WorktreeChoice;
  agent_id: string;
}

export type WorktreeChoice =
  | { kind: "existing"; path: string }
  | { kind: "new"; name: string | null; branch: string | null };

export interface CreatedThread {
  thread_id: string;
}

// ---------------------------------------------------------------------------
// Thread view

export interface ThreadSnapshot {
  thread_id: string;
  workspace_id: WorkspaceId;
  entries: ThreadEntry[];
  state: ThreadState;
}

export interface ThreadState {
  thread_id: string;
  title: string;
  agent_name: string;
  status: ThreadStatus;
  controls: ThreadControl[];
  plan: PlanEntry[];
  queued_messages: string[];
  token_usage: TokenUsage | null;
  error: string | null;
  needs_desktop_input: boolean;
}

export interface TokenUsage {
  used: number;
  max: number;
}

export interface PlanEntry {
  content: string;
  status: PlanEntryStatus;
}

export type PlanEntryStatus = "pending" | "in_progress" | "completed";

/** `ThreadControlKind` is flattened into the control. */
export type ThreadControl = { id: string; label: string } & ThreadControlKind;

export type ThreadControlKind =
  | { kind: "select"; value: string | null; options: ControlOption[] }
  | { kind: "toggle"; value: boolean };

export interface ControlOption {
  value: string;
  label: string;
  group: string | null;
  description: string | null;
}

/** Untagged: a toggle's bool or a select's option value. */
export type ControlValue = boolean | string;

export type ThreadEntry =
  | { kind: "user"; text: string }
  | { kind: "assistant"; chunks: AssistantChunk[] }
  | ({ kind: "tool_call" } & ToolCall)
  | { kind: "compaction"; summary: string | null }
  | { kind: "elicitation"; message: string };

export type AssistantChunk =
  | { kind: "message"; markdown: string }
  | { kind: "thought"; markdown: string };

export interface ToolCall {
  id: string;
  /** Markdown title. */
  title: string;
  /** read, edit, delete, move, search, execute, think, fetch, switch_mode, other */
  tool_kind: string;
  status: ToolCallStatus;
  content: ToolCallContent[];
  permission_options: PermissionOption[] | null;
}

export type ToolCallStatus =
  | "pending"
  | "waiting_for_confirmation"
  | "in_progress"
  | "completed"
  | "failed"
  | "rejected"
  | "canceled";

export type ToolCallContent =
  | { kind: "markdown"; markdown: string }
  | { kind: "diff"; path: string; old_text: string | null; new_text: string }
  | { kind: "terminal"; command: string | null; output: string; exit_code: number | null };

export interface PermissionOption {
  id: string;
  label: string;
  /** allow_once, allow_always, reject_once, reject_always */
  kind: string;
}

export interface ThreadEntryEvent {
  thread_id: string;
  index: number;
  entry: ThreadEntry;
}

export interface ThreadEntriesRemovedEvent {
  thread_id: string;
  start: number;
  end: number;
}

// ---------------------------------------------------------------------------
// Tabs screen

export interface WorkspaceItems {
  workspace_id: WorkspaceId;
  name: string;
  editors: EditorTab[];
  terminals: TerminalTab[];
  active_item_id: ItemId | null;
}

export interface EditorTab {
  item_id: ItemId;
  kind: EditorTabKind;
  title: string;
  path: string | null;
  is_dirty: boolean;
}

export type EditorTabKind = "file" | "diff" | "commit" | "other";

export interface TerminalTab {
  item_id: ItemId;
  title: string;
  has_bell: boolean;
}

export interface WorkspaceItemsChangedEvent {
  workspace_id: WorkspaceId;
}

export interface DirectoryListing {
  entries: DirectoryEntry[];
}

export interface DirectoryEntry {
  worktree_id: WorktreeId;
  path: string;
  name: string;
  is_dir: boolean;
  is_ignored: boolean;
  git_status: FileStatus | null;
}

export interface OpenedItem {
  item_id: ItemId;
  kind: EditorTabKind;
  offset: number | null;
}

export interface GitLog {
  repository_id: RepositoryId | null;
  branch: string | null;
  commits: CommitSummary[];
  has_more: boolean;
}

export interface CommitSummary {
  sha: string;
  subject: string;
  author: string;
  /** Unix seconds. */
  timestamp: number;
  refs: string[];
}

export type FileStatus = "added" | "modified" | "deleted" | "renamed" | "untracked" | "conflict";

// ---------------------------------------------------------------------------
// Full-screen item views

export type ItemContent =
  | { kind: "file"; title: string; buffer: BufferState }
  | { kind: "diff"; title: string; repository_id: RepositoryId; files: DiffFile[] }
  | {
      kind: "commit";
      title: string;
      sha: string;
      message: string;
      author: string;
      timestamp: number;
      files: CommitFile[];
    };

export interface DiffFile {
  path: string;
  status: FileStatus;
  is_staged: boolean;
  is_partially_staged: boolean;
  buffer: BufferState | null;
  base_text: string | null;
}

export interface CommitFile {
  path: string;
  status: FileStatus;
  old_text: string | null;
  new_text: string | null;
  is_binary: boolean;
}

export interface BufferState {
  buffer: BufferHandle;
  version: number;
  text: string;
  path: string | null;
  language: string | null;
  read_only: boolean;
  is_dirty: boolean;
  has_lsp: boolean;
  diagnostics: Diagnostic[];
  tab_size: number;
  hard_tabs: boolean;
}

/** A serialized CodeMirror `ChangeSet` (`ChangeSet.toJSON()`). */
export type ChangeSetJson = (number | [number, ...string[]])[];

export interface ClientUpdate {
  changes: ChangeSetJson;
}

export interface ServerUpdate {
  client_id: string;
  changes: ChangeSetJson;
}

export interface PushResult {
  accepted: boolean;
  version: number;
}

export interface BufferUpdatesEvent {
  buffer: BufferHandle;
  /** The version the first update applies to. */
  version: number;
  updates: ServerUpdate[];
}

export interface BufferMetadataEvent {
  buffer: BufferHandle;
  is_dirty: boolean;
  language: string | null;
  path: string | null;
}

export interface BufferDiagnosticsEvent {
  buffer: BufferHandle;
  version: number;
  diagnostics: Diagnostic[];
}

export interface BufferReparsedEvent {
  buffer: BufferHandle;
}

export interface DiffBaseChangedEvent {
  buffer: BufferHandle;
  base_text: string | null;
}

export interface DiffFilesChangedEvent {
  workspace_id: WorkspaceId;
  item_id: ItemId;
}

export interface Diagnostic {
  from: number;
  to: number;
  severity: DiagnosticSeverity;
  message: string;
  source: string | null;
}

export type DiagnosticSeverity = "error" | "warning" | "info" | "hint";

/** `[from, to, syntax_style_index]` */
export type HighlightToken = [number, number, number];

export interface Highlights {
  version: number;
  /** Sorted and non-overlapping. */
  tokens: HighlightToken[];
}

export interface Completion {
  completion_id: number;
  label: string;
  detail: string | null;
  kind: string | null;
  from: number;
  to: number;
  insert_text: string;
}

export interface Hover {
  markdown: string;
  from: number;
  to: number;
}

// ---------------------------------------------------------------------------
// Terminals

export interface TerminalScreen {
  item_id: ItemId;
  title: string;
  columns: number;
  rows: number;
  lines: TerminalRun[][];
  cursor: TerminalCursor | null;
  display_offset: number;
  history_size: number;
  bracketed_paste: boolean;
  application_cursor: boolean;
}

/** The style flags are omitted when false. */
export interface TerminalRun {
  text: string;
  fg: string | null;
  bg: string | null;
  bold?: boolean;
  italic?: boolean;
  underline?: boolean;
  dim?: boolean;
  strikethrough?: boolean;
}

export interface TerminalCursor {
  row: number;
  column: number;
  shape: TerminalCursorShape;
}

export type TerminalCursorShape = "block" | "underline" | "bar" | "hollow";

export interface TerminalClosedEvent {
  workspace_id: WorkspaceId;
  item_id: ItemId;
}

// ---------------------------------------------------------------------------
// Methods

type Empty = Record<string, never>;

/**
 * Every request, keyed by method name. Methods that api.rs doesn't give a
 * result type for are typed `unknown` (Zed answers them with `null`).
 */
export interface Methods {
  "threads.list": { params: Empty; result: ThreadsList };
  "threads.new_thread_options": { params: Empty; result: NewThreadOptions };
  "threads.create": { params: CreateThreadParams; result: CreatedThread };

  "thread.open": { params: { thread_id: string }; result: ThreadSnapshot };
  "thread.close": { params: { thread_id: string }; result: unknown };
  "thread.send": { params: { thread_id: string; text: string }; result: unknown };
  "thread.cancel": { params: { thread_id: string }; result: unknown };
  "thread.authorize": {
    params: { thread_id: string; tool_call_id: string; option_id: string };
    result: unknown;
  };
  "thread.set_control": {
    params: { thread_id: string; control_id: string; value: ControlValue };
    result: unknown;
  };
  "thread.remove_queued": { params: { thread_id: string; index: number }; result: unknown };

  "workspace.items": { params: { workspace_id?: WorkspaceId | null }; result: WorkspaceItems };
  "workspace.close_item": { params: { workspace_id: WorkspaceId; item_id: ItemId }; result: unknown };
  "workspace.new_terminal": { params: { workspace_id: WorkspaceId }; result: unknown };
  "workspace.list_directory": {
    params: { workspace_id: WorkspaceId; worktree_id?: WorktreeId | null; path: string };
    result: DirectoryListing;
  };
  "workspace.open_file": {
    params: { workspace_id: WorkspaceId; worktree_id: WorktreeId; path: string };
    result: OpenedItem;
  };

  "git.log": {
    params: { workspace_id: WorkspaceId; offset: number; limit: number };
    result: GitLog;
  };
  "git.open_uncommitted": { params: { workspace_id: WorkspaceId }; result: OpenedItem };
  "git.open_commit": { params: { workspace_id: WorkspaceId; sha: string }; result: OpenedItem };
  "git.stage": {
    params: { workspace_id: WorkspaceId; repository_id: RepositoryId; paths: string[] };
    result: unknown;
  };
  "git.unstage": {
    params: { workspace_id: WorkspaceId; repository_id: RepositoryId; paths: string[] };
    result: unknown;
  };

  "item.open": { params: { workspace_id: WorkspaceId; item_id: ItemId }; result: ItemContent };
  "item.close": { params: { workspace_id: WorkspaceId; item_id: ItemId }; result: unknown };

  "buffer.push": {
    params: { buffer: BufferHandle; version: number; updates: ClientUpdate[] };
    result: PushResult;
  };
  "buffer.highlights": {
    params: { buffer: BufferHandle; from: number; to: number };
    result: Highlights;
  };
  "buffer.completions": {
    params: { buffer: BufferHandle; version: number; offset: number };
    result: Completion[];
  };
  "buffer.apply_completion": {
    params: { buffer: BufferHandle; completion_id: number };
    result: unknown;
  };
  "buffer.hover": {
    params: { buffer: BufferHandle; version: number; offset: number };
    result: Hover | null;
  };
  "buffer.definition": {
    params: { workspace_id: WorkspaceId; buffer: BufferHandle; version: number; offset: number };
    result: OpenedItem | null;
  };
  "buffer.format": { params: { buffer: BufferHandle }; result: unknown };
  "buffer.save": { params: { buffer: BufferHandle }; result: unknown };

  "terminal.open": {
    params: { workspace_id: WorkspaceId; item_id: ItemId; columns: number; rows: number };
    result: TerminalScreen;
  };
  "terminal.resize": {
    params: { workspace_id: WorkspaceId; item_id: ItemId; columns: number; rows: number };
    result: unknown;
  };
  "terminal.close": { params: { workspace_id: WorkspaceId; item_id: ItemId }; result: unknown };
  "terminal.input": {
    params: { workspace_id: WorkspaceId; item_id: ItemId; data: string };
    result: unknown;
  };
  "terminal.scroll": {
    params: { workspace_id: WorkspaceId; item_id: ItemId; lines: number };
    result: unknown;
  };
}

export type MethodName = keyof Methods;
export type Params<M extends MethodName> = Methods[M]["params"];
export type Result<M extends MethodName> = Methods[M]["result"];

// ---------------------------------------------------------------------------
// Events

export interface Events {
  hello: Hello;
  theme: Theme;
  "threads.changed": ThreadsList;
  "thread.entry": ThreadEntryEvent;
  "thread.entries_removed": ThreadEntriesRemovedEvent;
  "thread.state": ThreadState;
  "workspace.items_changed": WorkspaceItemsChangedEvent;
  "buffer.updates": BufferUpdatesEvent;
  "buffer.metadata": BufferMetadataEvent;
  "buffer.diagnostics": BufferDiagnosticsEvent;
  "buffer.reparsed": BufferReparsedEvent;
  "diff.base_changed": DiffBaseChangedEvent;
  "diff.files_changed": DiffFilesChangedEvent;
  "terminal.screen": TerminalScreen;
  "terminal.closed": TerminalClosedEvent;
  toast: ToastEvent;
}

export type EventName = keyof Events;

// ---------------------------------------------------------------------------
// HTTP surface of the server (same origin)

export interface PairRequest {
  code: string;
  device_name: string;
}

/** Pairing hands out the HTTP API's secret, which authenticates everything else. */
export interface PairResponse {
  secret: string;
}

export interface PushKeyResponse {
  /** base64url-encoded VAPID public key. */
  public_key: string;
}

/** The JSON payload of a Web Push message. */
export interface PushPayload {
  title: string;
  body: string;
  tag: string | null;
  /** e.g. `#/thread/<id>` */
  url: string | null;
}

/** WebSocket handshake frames, exchanged before the API frames. */
export type AuthFrame = { type: "auth"; token: string; device_name: string | null };
export type AuthReply = { type: "auth_ok"; client_id: number };

/** Close code the server uses when the secret is wrong (e.g. it was changed in Zed). */
export const CLOSE_UNAUTHORIZED = 4001;
