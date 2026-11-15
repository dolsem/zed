//! The HTTP API described in [`crate::rest`], as an OpenAPI document that
//! `http_api` serves at `/api/openapi.json`.
//!
//! The schemas are derived from the request and response types, so they can't
//! drift from them. The operations below only exist to be documented and are
//! kept in sync with the router in `mobile_bridge`'s `rest.rs` by hand.

#![allow(dead_code)]

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::api::ThreadsList;
use crate::rest::*;

/// The document, serialized as JSON.
pub fn json() -> String {
    ApiDoc::openapi()
        .to_json()
        .expect("the OpenAPI document serializes")
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Zed HTTP API",
        description = "Controls Zed and the agents running in it.\n\n\
            Anything that takes a while starts an `Operation` (answered with `202 Accepted`), \
            whose progress is streamed as server-sent events. Threads can be subscribed to the same way.\n\n\
            Path segments are percent-decoded.",
    ),
    paths(
        session,
        projects,
        agents,
        create_worktree,
        threads,
        create_thread,
        thread,
        thread_events,
        runs,
        run,
        command_output,
        send_message,
        cancel,
        respond_to_permission,
        answer_question,
        set_control,
        remove_queued_message,
        focus_thread,
        operations,
        operation,
        operation_events,
        pairing,
        reviews,
        review_comments,
        mcp,
    ),
    // Only referenced from the server-sent events' descriptions.
    components(schemas(RunItemEvent, StreamError, crate::api::ThreadState)),
    modifiers(&BearerAuth),
    security(("bearer" = [])),
    tags(
        (name = "projects", description = "Projects open in Zed and the agents that can work on them."),
        (name = "threads", description = "Agent threads, their runs and everything the agent waits on."),
        (name = "operations", description = "Work that takes a while, like creating a worktree or a thread."),
        (name = "reviews", description = "Code reviews submitted in the git panel's review mode, with their comments."),
        (name = "session", description = "The connection to Zed itself."),
    ),
)]
pub struct ApiDoc;

struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("The `http_api.secret` setting."))
                    .build(),
            ),
        );
    }
}

/// Checks the secret.
#[utoipa::path(
    get,
    path = "/api/session",
    tag = "session",
    responses(
        (status = 200, body = EmptyResponse),
        (status = 401, body = ErrorResponse, description = "The secret is missing or wrong."),
    ),
)]
fn session() {}

/// Lists the projects open in Zed, with their git worktrees and branches.
#[utoipa::path(
    get,
    path = "/api/projects",
    tag = "projects",
    responses((status = 200, body = ProjectsResponse)),
)]
fn projects() {}

/// Lists the agents that can run threads.
#[utoipa::path(
    get,
    path = "/api/agents",
    tag = "projects",
    responses((status = 200, body = AgentsResponse)),
)]
fn agents() {}

/// Creates a git worktree for a project, running its `create_worktree` hooks.
#[utoipa::path(
    post,
    path = "/api/worktrees",
    tag = "operations",
    request_body = CreateWorktreeRequest,
    responses(
        (status = 202, body = Operation, description = "The operation that creates the worktree."),
        (status = 400, body = ErrorResponse),
        (status = 404, body = ErrorResponse, description = "No such project."),
    ),
)]
fn create_worktree() {}

/// Lists every thread of every project, whether or not the project is open or
/// the thread is loaded. Threads that aren't loaded are `idle`. Projects
/// without matching threads are left out.
#[utoipa::path(
    get,
    path = "/api/threads",
    tag = "threads",
    params(ThreadsQueryParams),
    responses(
        (status = 200, body = ThreadsList),
        (status = 400, body = ErrorResponse, description = "A filter has an invalid value."),
    ),
)]
fn threads() {}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ThreadsQueryParams {
    /// Which threads to list by whether they're archived (default `unarchived`).
    archived: Option<ArchivedFilter>,
    /// `true` to only list threads whose agent is generating.
    running: Option<bool>,
    /// Only threads with this flair, by id or name, or `none` for threads
    /// without one.
    flair: Option<String>,
}

/// Creates a thread in one of the project's worktrees.
///
/// The operation succeeds once the thread exists. Its agent's session keeps
/// loading after that: setting controls and sending messages answer `409`
/// until it's ready, which `GET /api/threads/{thread_id}` waits for.
#[utoipa::path(
    post,
    path = "/api/threads",
    tag = "threads",
    request_body = CreateThreadRequest,
    responses(
        (status = 202, body = Operation, description = "The operation that creates the thread."),
        (status = 400, body = ErrorResponse, description = "Invalid body, e.g. with a field this request no longer takes."),
        (status = 404, body = ErrorResponse, description = "No such project."),
    ),
)]
fn create_thread() {}

/// The thread's state and runs, loading it in Zed if needed.
#[utoipa::path(
    get,
    path = "/api/threads/{thread_id}",
    tag = "threads",
    params(ThreadIdParam, DetailParam),
    responses(
        (status = 200, body = ThreadDetails),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "The thread couldn't be loaded."),
    ),
)]
fn thread() {}

/// Streams the thread's changes as server-sent events.
///
/// Events, by name:
/// - `snapshot`: `ThreadDetails`, sent first, and again whenever runs were removed
///   (e.g. the thread was rewound to a checkpoint).
/// - `state`: `ThreadState`, when the status, title, controls, plan, queue or error changes.
/// - `run`: `Run` without items, when a run starts or its status, pending requests or
///   final message change.
/// - `item`: `RunItemEvent`, with `?detail=full`, when an item is added or changes.
/// - `error`: `StreamError`, after which the stream ends, e.g. when the thread is closed.
#[utoipa::path(
    get,
    path = "/api/threads/{thread_id}/events",
    tag = "threads",
    params(ThreadIdParam, DetailParam),
    responses(
        (status = 200, body = String, content_type = "text/event-stream"),
        (status = 404, body = ErrorResponse, description = "No such thread."),
    ),
)]
fn thread_events() {}

/// The thread's runs, oldest first.
#[utoipa::path(
    get,
    path = "/api/threads/{thread_id}/runs",
    tag = "threads",
    params(ThreadIdParam, DetailParam),
    responses(
        (status = 200, body = RunsResponse),
        (status = 404, body = ErrorResponse, description = "No such thread."),
    ),
)]
fn runs() {}

#[utoipa::path(
    get,
    path = "/api/threads/{thread_id}/runs/{run_id}",
    tag = "threads",
    params(
        ThreadIdParam,
        ("run_id" = String, Path, description = "See `Run::run_id`."),
        DetailParam,
    ),
    responses(
        (status = 200, body = Run),
        (status = 404, body = ErrorResponse, description = "No such thread or run."),
    ),
)]
fn run() {}

/// The output of a tool call that ran a command, which runs leave out.
#[utoipa::path(
    get,
    path = "/api/threads/{thread_id}/tool_calls/{tool_call_id}/output",
    tag = "threads",
    params(ThreadIdParam, ToolCallIdParam),
    responses(
        (status = 200, body = CommandOutput),
        (status = 404, body = ErrorResponse, description = "No such thread or tool call."),
    ),
)]
fn command_output() {}

/// Sends a message, which the agent queues while it's busy.
#[utoipa::path(
    post,
    path = "/api/threads/{thread_id}/messages",
    tag = "threads",
    params(ThreadIdParam),
    request_body = SendMessageRequest,
    responses(
        (status = 200, body = SentMessage),
        (status = 400, body = ErrorResponse, description = "The message is empty."),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "The agent's session isn't ready yet (`the agent's session is not ready yet`; a thread that wasn't loaded starts loading), or the thread can't take messages right now."),
    ),
)]
fn send_message() {}

/// Stops the agent.
#[utoipa::path(
    post,
    path = "/api/threads/{thread_id}/cancel",
    tag = "threads",
    params(ThreadIdParam),
    responses(
        (status = 200, body = EmptyResponse),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse),
    ),
)]
fn cancel() {}

/// Answers a permission prompt (`PendingRequest::Permission`).
#[utoipa::path(
    post,
    path = "/api/threads/{thread_id}/permissions/{tool_call_id}",
    tag = "threads",
    params(ThreadIdParam, ToolCallIdParam),
    request_body = PermissionResponse,
    responses(
        (status = 200, body = EmptyResponse),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "The tool call isn't waiting for permission."),
    ),
)]
fn respond_to_permission() {}

/// Answers a question (`PendingRequest::Question`).
#[utoipa::path(
    post,
    path = "/api/threads/{thread_id}/questions/{question_id}",
    tag = "threads",
    params(ThreadIdParam, ("question_id" = String, Path)),
    request_body = QuestionResponse,
    responses(
        (status = 200, body = EmptyResponse),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "The question was already answered."),
    ),
)]
fn answer_question() {}

/// Sets one of the thread's controls (`ThreadState::controls`), e.g. its model or mode.
#[utoipa::path(
    put,
    path = "/api/threads/{thread_id}/controls/{control_id}",
    tag = "threads",
    params(ThreadIdParam, ("control_id" = String, Path, description = "See `ThreadControl::id`.")),
    request_body = SetControlRequest,
    responses(
        (status = 200, body = EmptyResponse),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "The agent's session isn't ready yet (`the agent's session is not ready yet`; a thread that wasn't loaded starts loading), no such control, or the value doesn't fit it."),
    ),
)]
fn set_control() {}

/// Removes a message from the thread's queue (`ThreadState::queued_messages`).
#[utoipa::path(
    delete,
    path = "/api/threads/{thread_id}/queue/{index}",
    tag = "threads",
    params(ThreadIdParam, ("index" = usize, Path, description = "Position in the queue, starting at 0.")),
    responses(
        (status = 200, body = EmptyResponse),
        (status = 400, body = ErrorResponse, description = "The index isn't a number."),
        (status = 404, body = ErrorResponse, description = "No such thread."),
        (status = 409, body = ErrorResponse, description = "No queued message at that index."),
    ),
)]
fn remove_queued_message() {}

/// Brings Zed to the front with the thread shown, as clicking it in the sidebar does,
/// loading it first if needed (an archived thread is unarchived). Optionally scrolls to one
/// of its runs, and opens a commit's or the uncommitted changes, scrolled to a file and line.
#[utoipa::path(
    post,
    path = "/api/threads/{thread_id}/focus",
    tag = "threads",
    params(ThreadIdParam),
    request_body(content = FocusThreadRequest, description = "Optional: an empty body only shows the thread."),
    responses(
        (status = 200, body = EmptyResponse),
        (status = 400, body = ErrorResponse, description = "Invalid body, e.g. `item_index` without `run_id`."),
        (status = 404, body = ErrorResponse, description = "No such thread, run, item, file or repository."),
        (status = 409, body = ErrorResponse, description = "The thread couldn't be loaded."),
        (status = 500, body = ErrorResponse, description = "The commit couldn't be opened, e.g. because no such commit exists."),
    ),
)]
fn focus_thread() {}

/// Lists recent operations.
#[utoipa::path(
    get,
    path = "/api/operations",
    tag = "operations",
    responses((status = 200, body = OperationsResponse)),
)]
fn operations() {}

#[utoipa::path(
    get,
    path = "/api/operations/{operation_id}",
    tag = "operations",
    params(OperationIdParam),
    responses(
        (status = 200, body = Operation),
        (status = 404, body = ErrorResponse, description = "No such operation."),
    ),
)]
fn operation() {}

/// Streams the operation's `OperationEvent`s as server-sent events, named by their
/// `type`. Past events are replayed first, and the stream ends after `succeeded` or
/// `failed`.
#[utoipa::path(
    get,
    path = "/api/operations/{operation_id}/events",
    tag = "operations",
    params(OperationIdParam),
    responses(
        (status = 200, body = String, content_type = "text/event-stream"),
        (status = 404, body = ErrorResponse, description = "No such operation."),
    ),
)]
fn operation_events() {}

/// Starts pairing a device with the web UI.
#[utoipa::path(
    post,
    path = "/api/pairing",
    tag = "session",
    responses(
        (status = 200, body = PairingResponse),
        (status = 409, body = ErrorResponse, description = "The web UI isn't enabled."),
    ),
)]
fn pairing() {}

/// Lists the submitted reviews of every worktree, whether or not it's open in
/// Zed. Without `all`, only reviews with unaddressed comments are listed.
#[utoipa::path(
    get,
    path = "/api/reviews",
    tag = "reviews",
    params(ReviewsQueryParams),
    responses(
        (status = 200, body = ReviewsResponse),
        (status = 400, body = ErrorResponse, description = "A filter has an invalid value."),
    ),
)]
fn reviews() {}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ReviewsQueryParams {
    /// `true` to also list reviews whose comments were all addressed.
    all: Option<bool>,
    /// Only the reviews of the worktree at this path.
    worktree_path: Option<String>,
}

/// A review's comments: by default the unaddressed ones.
#[utoipa::path(
    get,
    path = "/api/reviews/{review_id}/comments",
    tag = "reviews",
    params(
        ("review_id" = String, Path, description = "See `ReviewSummary::review_id`."),
        ReviewCommentsQueryParams,
    ),
    responses(
        (status = 200, body = ReviewCommentsResponse),
        (status = 400, body = ErrorResponse, description = "A filter has an invalid value."),
        (status = 404, body = ErrorResponse, description = "No such review."),
    ),
)]
fn review_comments() {}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ReviewCommentsQueryParams {
    /// Which comments to return by whether they were addressed (default `unaddressed`).
    status: Option<CommentStatusFilter>,
    /// Only comments with this tag, with or without its `#`.
    tag: Option<String>,
}

/// The same API as an MCP server (streamable HTTP transport, JSON-RPC 2.0).
#[utoipa::path(
    post,
    path = "/api/mcp",
    tag = "session",
    request_body(content = serde_json::Value, description = "A JSON-RPC request or notification."),
    responses(
        (status = 200, body = serde_json::Value, description = "The JSON-RPC response."),
        (status = 202, description = "A notification was received."),
    ),
)]
fn mcp() {}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Path)]
struct ThreadIdParam {
    /// See `ThreadSummary::thread_id`.
    thread_id: String,
}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Path)]
struct ToolCallIdParam {
    /// See `RunToolCall::tool_call_id`.
    tool_call_id: String,
}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Path)]
struct OperationIdParam {
    operation_id: String,
}

#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct DetailParam {
    /// `full` to include every `RunItem` instead of only each run's outcome.
    detail: Option<Detail>,
}

#[derive(utoipa::ToSchema)]
#[schema(rename_all = "snake_case")]
enum Detail {
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document() {
        let document: serde_json::Value = serde_json::from_str(&json()).unwrap();
        let paths = document["paths"].as_object().unwrap();
        assert_eq!(paths.len(), 24);
        let thread = &paths["/api/threads/{thread_id}"]["get"];
        assert_eq!(
            thread["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ThreadDetails"
        );
        let schemas = document["components"]["schemas"].as_object().unwrap();
        for name in ["Run", "RunItem", "OperationEvent", "ControlValue", "RunItemEvent"] {
            assert!(schemas.contains_key(name), "missing schema {name}");
        }
    }
}
