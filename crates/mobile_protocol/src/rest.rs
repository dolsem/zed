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
//! | GET | `/api/session` | | `{}` (checks the secret) |
//! | GET | `/api/projects` | | [`ProjectsResponse`] |
//! | GET | `/api/agents` | | [`AgentsResponse`] |
//! | POST | `/api/worktrees` | [`CreateWorktreeRequest`] | [`Operation`] |
//! | GET | `/api/threads` | | [`crate::api::ThreadsList`] |
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
//! | GET | `/api/operations` | | [`OperationsResponse`] |
//! | GET | `/api/operations/{operation_id}` | | [`Operation`] |
//! | GET | `/api/operations/{operation_id}/events` | | SSE, see [`OperationEvent`] |
//! | POST | `/api/pairing` | | [`PairingResponse`] |
//! | POST | `/api/mcp` | JSON-RPC | the same API as an MCP server, see [`crate::mcp`] |
//!
//! Path segments are percent-decoded. The run and thread endpoints take
//! `?detail=full` to include every [`RunItem`] instead of only the run's
//! outcome.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api::{
    AgentOption, ControlValue, NewThreadProject, PermissionOption, ThreadState, ToolCallStatus,
    WorktreeChoice,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProjectsResponse {
    pub projects: Vec<NewThreadProject>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsResponse {
    pub agents: Vec<AgentOption>,
    /// The agent Zed would pick for a new thread.
    pub default_agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateWorktreeRequest {
    pub project_id: String,
    /// Optional name for the worktree's directory.
    pub name: Option<String>,
    /// The branch to base the worktree on (default: the current branch).
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateThreadRequest {
    pub project_id: String,
    pub worktree: WorktreeChoice,
    /// Defaults to the agent Zed would pick for a new thread.
    pub agent_id: Option<String>,
    /// Controls (see [`crate::api::ThreadControl`]) to set before the first
    /// message is sent, e.g. `{"mode": "plan"}`.
    #[serde(default)]
    pub controls: BTreeMap<String, ControlValue>,
    /// The first message. Without it the thread stays a draft.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadDetails {
    pub state: ThreadState,
    pub workspace_paths: Vec<String>,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunsResponse {
    pub runs: Vec<Run>,
}

/// Everything between one user message and the next: the message itself and
/// all the agent's output in response to it.
#[derive(Debug, Clone, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Question {
    pub question_id: String,
    pub message: String,
    pub status: QuestionStatus,
    #[serde(flatten)]
    pub mode: QuestionMode,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum QuestionMode {
    /// Accept with `content` matching `schema` (a JSON Schema object whose
    /// properties are strings, numbers, booleans or enums).
    Form { schema: serde_json::Value },
    /// The user should open `url`, then accept.
    Url { url: String },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Pending,
    Accepted,
    Declined,
    Cancelled,
    Completed,
}

/// One per entry of the thread, so an item's index within its run is stable.
#[derive(Debug, Clone, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize)]
pub struct CommandOutput {
    pub tool_call_id: String,
    pub command: Option<String>,
    pub status: ToolCallStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SentMessage {
    /// The run the message started. `None` when it was queued because the
    /// agent is still busy; it starts its own run once sent.
    pub run_id: Option<String>,
    pub queued: bool,
}

/// Either the id of one of the permission's options, or the kind of option
/// to pick (the first one of that kind).
#[derive(Debug, Deserialize)]
pub struct PermissionResponse {
    pub option_id: Option<String>,
    /// allow_once, allow_always, reject_once, reject_always
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuestionResponse {
    pub action: QuestionAction,
    /// The answers, for `accept` on form questions.
    #[serde(default)]
    pub content: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct RunItemEvent {
    pub run_id: String,
    /// Position of the item within the run.
    pub index: usize,
    pub item: RunItem,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamError {
    pub message: String,
}

/// A code that lets one device pair with the web UI until it expires.
#[derive(Debug, Clone, Serialize)]
pub struct PairingResponse {
    pub code: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationsResponse {
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CreateWorktree,
    CreateThread,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationResult {
    Worktree(CreatedWorktree),
    Thread(CreatedThread),
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedWorktree {
    /// One per repository of the project (usually just one).
    pub paths: Vec<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedThread {
    pub thread_id: String,
    pub workspace_paths: Vec<String>,
    /// The run started by `message`, if one was given.
    pub run_id: Option<String>,
}

/// Streamed by `/operations/{operation_id}/events` with the event name set
/// to the type (`step`, `hook_started`, …). Past events are replayed first,
/// and the stream ends after `succeeded` or `failed`.
#[derive(Debug, Clone, Serialize)]
pub struct OperationEvent {
    /// Increases by one with every event of the operation, starting at 0.
    pub sequence: u64,
    /// Unix milliseconds.
    pub timestamp: u64,
    #[serde(flatten)]
    pub kind: OperationEventKind,
}

#[derive(Debug, Clone, Serialize)]
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
    ControlSet {
        control_id: String,
    },
    MessageSent {
        run_id: Option<String>,
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
            Self::ControlSet { .. } => "control_set",
            Self::MessageSent { .. } => "message_sent",
            Self::Warning { .. } => "warning",
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStep {
    OpeningProject,
    CreatingWorktree,
    RunningHooks,
    StartingAgent,
    SendingMessage,
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
            "worktree": { "kind": "new", "branch": "main" },
            "agent_id": "claude-acp",
            "controls": { "mode": "plan", "fast": true },
            "message": "Fix the bug",
        }))
        .unwrap();
        assert!(matches!(request.worktree, WorktreeChoice::New { .. }));
        assert!(matches!(
            request.controls.get("fast"),
            Some(ControlValue::Toggle(true))
        ));

        let response: QuestionResponse = serde_json::from_value(serde_json::json!({
            "action": "accept",
            "content": { "choice": "B" },
        }))
        .unwrap();
        assert_eq!(response.action, QuestionAction::Accept);
        assert_eq!(response.content["choice"], "B");
    }
}
