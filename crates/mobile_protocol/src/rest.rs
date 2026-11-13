//! The HTTP API Zed serves through the `http_api` executable.
//!
//! Every request needs `Authorization: Bearer <http_api.secret>`. Bodies are
//! JSON. Errors are `{"error": string}` with a 4xx/5xx status.
//!
//! Anything that takes a while starts an [`Operation`] (answered with
//! `202 Accepted`), whose progress — including each worktree hook that runs
//! and every error — is streamed as server-sent events. Threads can be
//! subscribed to the same way.
//!
//! | Method | Path | Body | Response |
//! |---|---|---|---|
//! | GET | `/api/openapi.json` | | this API as an OpenAPI document, see [`crate::openapi`] (needs no secret) |
//! | GET | `/api/session` | | `{}` (checks the secret) |
//! | GET | `/api/projects` | | [`ProjectsResponse`] |
//! | GET | `/api/agents` | | [`AgentsResponse`] |
//! | POST | `/api/worktrees` | [`CreateWorktreeRequest`] | [`Operation`] |
//! | GET | `/api/threads` | | [`crate::api::ThreadsList`], filtered by [`ThreadsQuery`] |
//! | POST | `/api/threads` | [`CreateThreadRequest`] | [`Operation`] |
//! | GET | `/api/threads/{thread_id}` | | [`ThreadDetails`] |
//! | GET | `/api/threads/{thread_id}/events` | | SSE, see [`thread_events`] |
//! | GET | `/api/threads/{thread_id}/runs` | | [`RunsResponse`] |
//! | GET | `/api/threads/{thread_id}/runs/{run_id}` | | [`Run`] |
//! | GET | `/api/threads/{thread_id}/tool_calls/{tool_call_id}/output` | | [`CommandOutput`] |
//! | POST | `/api/threads/{thread_id}/messages` | [`SendMessageRequest`] | [`SentMessage`] |
//! | POST | `/api/threads/{thread_id}/cancel` | | `{}` |
//! | POST | `/api/threads/{thread_id}/permissions/{tool_call_id}` | [`PermissionResponse`] | `{}` |
//! | POST | `/api/threads/{thread_id}/questions/{question_id}` | [`QuestionResponse`] | `{}` |
//! | PUT | `/api/threads/{thread_id}/controls/{control_id}` | [`SetControlRequest`] | `{}` |
//! | DELETE | `/api/threads/{thread_id}/queue/{index}` | | `{}` |
//! | POST | `/api/threads/{thread_id}/focus` | [`FocusThreadRequest`] (optional) | `{}` |
//! | GET | `/api/operations` | | [`OperationsResponse`] |
//! | GET | `/api/operations/{operation_id}` | | [`Operation`] |
//! | GET | `/api/operations/{operation_id}/events` | | SSE, see [`OperationEvent`] |
//! | POST | `/api/pairing` | | [`PairingResponse`] |
//! | POST | `/api/mcp` | JSON-RPC | the same API as an MCP server, see [`crate::mcp`] |
//!
//! Path segments are percent-decoded. The run and thread endpoints take
//! `?detail=full` to include every [`RunItem`] instead of only the run's
//! outcome.
//!
//! Starting work in a new worktree takes separate requests: create the
//! worktree, create the thread in it, then set its controls and send it
//! messages. Those last two need the agent's session, and answer `409` with
//! [`SESSION_NOT_READY`] until it's loaded rather than waiting for it (a
//! thread that isn't loaded starts loading). `GET /api/threads/{thread_id}`
//! waits for the session, so it tells when the thread is ready.

/// The error of setting a control or sending a message before the agent's
/// session is loaded.
pub const SESSION_NOT_READY: &str = "the agent's session is not ready yet";

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::{
    AgentOption, ControlValue, NewThreadProject, PermissionOption, ThreadState, ToolCallStatus,
};

/// The body of every 4xx/5xx response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

/// `{}`, answered by endpoints that have nothing to report.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EmptyResponse {}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProjectsResponse {
    pub projects: Vec<NewThreadProject>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentsResponse {
    pub agents: Vec<AgentOption>,
    /// The agent Zed would pick for a new thread.
    pub default_agent_id: String,
}

/// The query of `GET /api/threads`, which lists every thread Zed knows of,
/// whether or not its project is open or the thread is loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadsQuery {
    pub archived: ArchivedFilter,
    /// Only threads whose agent is generating (`ThreadStatus::Running`).
    pub running: bool,
    pub flair: Option<FlairFilter>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchivedFilter {
    /// Only threads that aren't archived.
    #[default]
    Unarchived,
    /// Only archived threads.
    Archived,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlairFilter {
    /// Threads without a flair.
    None,
    /// A flair's id or name (names compare case-insensitively).
    Flair(String),
}

impl ThreadsQuery {
    /// Reads `?archived=unarchived|archived|all` (`true`/`false` work too),
    /// `?running=true` and `?flair=<id or name>|none`.
    pub fn parse(query: &BTreeMap<String, String>) -> Result<Self, String> {
        let archived = match query.get("archived").map(String::as_str) {
            None | Some("unarchived" | "false") => ArchivedFilter::Unarchived,
            Some("archived" | "true") => ArchivedFilter::Archived,
            Some("all") => ArchivedFilter::All,
            Some(other) => {
                return Err(format!(
                    "archived must be unarchived, archived or all, not {other:?}"
                ));
            }
        };
        let running = match query.get("running").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => return Err(format!("running must be true or false, not {other:?}")),
        };
        let flair = match query.get("flair").map(String::as_str) {
            None | Some("") => None,
            Some("none") => Some(FlairFilter::None),
            Some(flair) => Some(FlairFilter::Flair(flair.to_string())),
        };
        Ok(Self {
            archived,
            running,
            flair,
        })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateWorktreeRequest {
    pub project_id: String,
    /// Optional name for the worktree's directory.
    pub name: Option<String>,
    /// The branch to base the worktree on (default: the current branch).
    pub branch: Option<String>,
}

/// Only creates the thread: create a worktree for it with `POST
/// /api/worktrees` first, and set its controls and send it messages once its
/// session is ready (`GET /api/threads/{thread_id}` waits for that).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateThreadRequest {
    pub project_id: String,
    /// One of the project's worktree paths (see `WorktreeOption::path`).
    /// Defaults to the project itself, its main worktree.
    pub worktree_path: Option<String>,
    /// Defaults to the agent Zed would pick for a new thread.
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ThreadDetails {
    pub state: ThreadState,
    pub workspace_paths: Vec<String>,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunsResponse {
    pub runs: Vec<Run>,
}

/// Everything between one user message and the next: the message itself and
/// all the agent's output in response to it.
#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
pub struct Run {
    /// The run's position in the thread, starting at "1" ("0" holds anything
    /// the agent said before the first user message). It stays the same
    /// across restarts; only rewinding the thread to an earlier message
    /// removes runs, whose ids are then reused.
    pub run_id: String,
    pub status: RunStatus,
    pub user_message: String,
    /// The agent's last message, once the run is no longer running.
    pub final_message: Option<String>,
    /// Permission prompts and questions the agent is waiting on.
    pub pending: Vec<PendingRequest>,
    /// Why the run failed, for [`RunStatus::Failed`].
    pub error: Option<String>,
    pub message_count: usize,
    pub tool_call_count: usize,
    /// Only with `?detail=full`. Command output is omitted, see
    /// [`CommandOutput`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<RunItem>>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    WaitingForPermission,
    WaitingForAnswer,
    Completed,
    Cancelled,
    Failed,
}

impl RunStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingRequest {
    /// Answer with `POST /threads/{thread_id}/permissions/{tool_call_id}`.
    /// This is also how agents ask to leave plan mode (Claude's "ready to
    /// code?"), with the plan in `tool_call.content`.
    Permission {
        tool_call: RunToolCall,
        options: Vec<PermissionOption>,
    },
    /// Answer with `POST /threads/{thread_id}/questions/{question_id}`.
    Question(Question),
}

#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
pub struct Question {
    pub question_id: String,
    pub message: String,
    pub status: QuestionStatus,
    #[serde(flatten)]
    pub mode: QuestionMode,
}

#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum QuestionMode {
    /// Accept with `content` matching `schema` (a JSON Schema object whose
    /// properties are strings, numbers, booleans or enums).
    Form { schema: serde_json::Value },
    /// The user should open `url`, then accept.
    Url { url: String },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Pending,
    Accepted,
    Declined,
    Cancelled,
    Completed,
}

/// One per entry of the thread, so an item's index within its run is stable.
#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunItem {
    /// The run's own message comes first; later ones are prompts the agent
    /// sent its subagents.
    UserMessage {
        text: String,
    },
    AssistantMessage {
        markdown: String,
        /// The agent's reasoning, when it shares it.
        thinking: Option<String>,
        /// Written by a subagent rather than the agent itself.
        is_subagent: bool,
    },
    ToolCall(RunToolCall),
    Question(Question),
    /// The conversation was compacted to save context.
    Compaction {
        summary: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
pub struct RunToolCall {
    pub tool_call_id: String,
    /// Markdown, e.g. "Edit `src/main.rs`".
    pub title: String,
    /// ACP tool kind: read, edit, delete, move, search, execute, think,
    /// fetch, switch_mode, other.
    pub tool_kind: String,
    pub status: ToolCallStatus,
    /// The command line, for tools that run commands.
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    /// Size of the command's output, which is left out here. Fetch it from
    /// `/threads/{thread_id}/tool_calls/{tool_call_id}/output`.
    pub output_bytes: usize,
    /// Everything else the tool call shows: diffs, markdown.
    pub content: Vec<RunToolCallContent>,
    /// The tool's input as the agent sent it.
    pub raw_input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunToolCallContent {
    Markdown {
        markdown: String,
    },
    Diff {
        path: String,
        /// `None` for newly created files.
        old_text: Option<String>,
        new_text: String,
    },
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CommandOutput {
    pub tool_call_id: String,
    pub command: Option<String>,
    pub status: ToolCallStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SentMessage {
    /// The run the message started. `None` when it was queued because the
    /// agent is still busy; it starts its own run once sent.
    pub run_id: Option<String>,
    pub queued: bool,
}

/// Either the id of one of the permission's options, or the kind of option
/// to pick (the first one of that kind).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PermissionResponse {
    pub option_id: Option<String>,
    /// allow_once, allow_always, reject_once, reject_always
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionResponse {
    pub action: QuestionAction,
    /// The answers, for `accept` on form questions.
    #[serde(default)]
    pub content: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAction {
    Accept,
    Decline,
    Cancel,
}

/// Brings Zed to the front with the thread shown, as clicking it in the
/// sidebar does. Every field is optional, and so is the body.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusThreadRequest {
    /// Scrolls the thread to this run's message (see [`Run::run_id`]).
    pub run_id: Option<String>,
    /// With `run_id`: scrolls to this item of the run instead (see
    /// [`RunItemEvent::index`]; 0 is the run's message).
    pub item_index: Option<usize>,
    /// Also opens these changes in the thread's workspace.
    pub changes: Option<FocusChanges>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusChanges {
    /// The commit whose changes to show, by SHA. Without it, the uncommitted
    /// changes are shown.
    pub commit: Option<String>,
    /// Scrolls to this file: an absolute path, or one relative to the root of
    /// the workspace's repository. Absolute paths also pick the repository in
    /// workspaces with several.
    pub path: Option<String>,
    /// With `path`: scrolls to this line (starting at 1) of the file's new
    /// version, or as close to it as the diff shows.
    pub line: Option<u32>,
}

impl FocusThreadRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.item_index.is_some() && self.run_id.is_none() {
            return Err("item_index needs a run_id".to_string());
        }
        if let Some(changes) = &self.changes {
            if changes.line.is_some() && changes.path.is_none() {
                return Err("line needs a path".to_string());
            }
            if changes.line == Some(0) {
                return Err("lines start at 1".to_string());
            }
            if changes
                .commit
                .as_deref()
                .is_some_and(|commit| commit.trim().is_empty())
            {
                return Err("commit is empty".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetControlRequest {
    pub value: ControlValue,
}

/// Server-sent events of `/threads/{thread_id}/events`, by event name.
pub mod thread_events {
    /// [`super::ThreadDetails`], sent first, and again whenever runs were
    /// removed (e.g. the thread was rewound to a checkpoint).
    pub const SNAPSHOT: &str = "snapshot";
    /// [`crate::api::ThreadState`], when the status, title, controls, plan,
    /// queue or error changes.
    pub const STATE: &str = "state";
    /// [`super::Run`] without items, when a run starts or its status,
    /// pending requests or final message change.
    pub const RUN: &str = "run";
    /// [`super::RunItemEvent`], with `?detail=full`, when an item is added
    /// or changes (e.g. while a message streams in).
    pub const ITEM: &str = "item";
    /// [`super::StreamError`], after which the stream ends, e.g. when the
    /// thread is closed.
    pub const ERROR: &str = "error";
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunItemEvent {
    pub run_id: String,
    /// Position of the item within the run.
    pub index: usize,
    pub item: RunItem,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StreamError {
    pub message: String,
}

/// A code that lets one device pair with the web UI until it expires.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PairingResponse {
    pub code: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OperationsResponse {
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Operation {
    pub operation_id: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    /// Unix milliseconds.
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// Everything that happened so far, oldest first.
    pub events: Vec<OperationEvent>,
    pub result: Option<OperationResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CreateWorktree,
    CreateThread,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationResult {
    Worktree(CreatedWorktree),
    Thread(CreatedThread),
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreatedWorktree {
    /// One per repository of the project (usually just one).
    pub paths: Vec<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreatedThread {
    pub thread_id: String,
    pub workspace_paths: Vec<String>,
}

/// Streamed by `/operations/{operation_id}/events` with the event name set
/// to the type (`step`, `hook_started`, …). Past events are replayed first,
/// and the stream ends after `succeeded` or `failed`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OperationEvent {
    /// Increases by one with every event of the operation, starting at 0.
    pub sequence: u64,
    /// Unix milliseconds.
    pub timestamp: u64,
    #[serde(flatten)]
    pub kind: OperationEventKind,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationEventKind {
    Step {
        step: OperationStep,
        message: String,
    },
    WorktreeCreated {
        paths: Vec<String>,
        branch: Option<String>,
    },
    /// A `create_worktree` task hook started. The worktree is ready once
    /// all of them finished.
    HookStarted {
        label: String,
        command: String,
        cwd: Option<String>,
    },
    HookFinished {
        label: String,
        success: bool,
        error: Option<String>,
        /// The end of what the hook printed.
        output: Option<String>,
    },
    ThreadCreated {
        thread_id: String,
    },
    /// Something went wrong without failing the operation.
    Warning {
        message: String,
    },
    Succeeded {
        result: OperationResult,
    },
    Failed {
        error: String,
    },
}

impl OperationEventKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Step { .. } => "step",
            Self::WorktreeCreated { .. } => "worktree_created",
            Self::HookStarted { .. } => "hook_started",
            Self::HookFinished { .. } => "hook_finished",
            Self::ThreadCreated { .. } => "thread_created",
            Self::Warning { .. } => "warning",
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationStep {
    OpeningProject,
    CreatingWorktree,
    RunningHooks,
    StartingAgent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_event_shape() {
        let event = OperationEvent {
            sequence: 2,
            timestamp: 10,
            kind: OperationEventKind::HookFinished {
                label: "setup".into(),
                success: false,
                error: Some("exited with 1".into()),
                output: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({
                "sequence": 2,
                "timestamp": 10,
                "type": "hook_finished",
                "label": "setup",
                "success": false,
                "error": "exited with 1",
                "output": null,
            })
        );
        assert_eq!(event.kind.name(), "hook_finished");
    }

    #[test]
    fn test_requests_parse() {
        let request: CreateThreadRequest = serde_json::from_value(serde_json::json!({
            "project_id": "local|/repo",
            "worktree_path": "/worktrees/repo/a/repo",
            "agent_id": "claude-acp",
        }))
        .unwrap();
        assert_eq!(
            request.worktree_path.as_deref(),
            Some("/worktrees/repo/a/repo")
        );
        // Creating a thread no longer sends a message, so a request that asks
        // for one fails rather than leaving it unsent.
        assert!(
            serde_json::from_value::<CreateThreadRequest>(serde_json::json!({
                "project_id": "local|/repo",
                "message": "Fix the bug",
            }))
            .is_err()
        );

        let request: SetControlRequest =
            serde_json::from_value(serde_json::json!({ "value": true })).unwrap();
        assert!(matches!(request.value, ControlValue::Toggle(true)));

        let response: QuestionResponse = serde_json::from_value(serde_json::json!({
            "action": "accept",
            "content": { "choice": "B" },
        }))
        .unwrap();
        assert_eq!(response.action, QuestionAction::Accept);
        assert_eq!(response.content["choice"], "B");
    }

    #[test]
    fn test_focus_thread_request() {
        let parse = |value: serde_json::Value| {
            serde_json::from_value::<FocusThreadRequest>(value).map_err(|error| error.to_string())
        };
        let request = parse(serde_json::json!({
            "run_id": "3",
            "item_index": 2,
            "changes": { "commit": "abc123", "path": "src/main.rs", "line": 10 },
        }))
        .unwrap();
        assert_eq!(request.validate(), Ok(()));
        let changes = request.changes.unwrap();
        assert_eq!(changes.commit.as_deref(), Some("abc123"));
        assert_eq!(changes.line, Some(10));

        assert_eq!(
            parse(serde_json::json!({ "changes": {} }))
                .unwrap()
                .validate(),
            Ok(())
        );
        assert!(
            parse(serde_json::json!({ "item_index": 1 }))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({ "changes": { "line": 4 } }))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({ "changes": { "path": "a.rs", "line": 0 } }))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(parse(serde_json::json!({ "message": "3" })).is_err());
    }

    #[test]
    fn test_threads_query_parse() {
        let parse = |pairs: &[(&str, &str)]| {
            ThreadsQuery::parse(
                &pairs
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            )
        };
        assert_eq!(parse(&[]).unwrap(), ThreadsQuery::default());
        assert_eq!(
            parse(&[("archived", "all"), ("running", "true"), ("flair", "MCP")]).unwrap(),
            ThreadsQuery {
                archived: ArchivedFilter::All,
                running: true,
                flair: Some(FlairFilter::Flair("MCP".into())),
            }
        );
        assert_eq!(
            parse(&[("archived", "true"), ("flair", "none")]).unwrap(),
            ThreadsQuery {
                archived: ArchivedFilter::Archived,
                running: false,
                flair: Some(FlairFilter::None),
            }
        );
        assert!(parse(&[("archived", "maybe")]).is_err());
        assert!(parse(&[("running", "yes")]).is_err());
    }
}
