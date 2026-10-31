//! The HTTP API as an MCP server, so agents can drive Zed: `POST /api/mcp`
//! speaks MCP's Streamable HTTP transport (JSON-RPC 2.0, answered with plain
//! JSON; there is no server-to-client stream, so `GET` answers 405). It needs
//! the same `Authorization: Bearer <http_api.secret>` header as the rest of
//! the API, e.g.:
//!
//! ```sh
//! claude mcp add --transport http zed https://example.com/api/mcp \
//!     --header "Authorization: Bearer <secret>"
//! ```
//!
//! Every tool wraps an endpoint of [`crate::rest`] and returns its JSON. Tools
//! that start something slow wait for it by default, up to
//! [`MAX_WAIT_SECONDS`], since MCP has no event streams.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Newest first; the server answers `initialize` with the client's version
/// when it knows it, else with the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub const SERVER_NAME: &str = "zed";

/// Waits stop before the HTTP API's request timeout; a tool that waited this
/// long returns what it has, and can be called again.
pub const MAX_WAIT_SECONDS: u64 = 100;
pub const DEFAULT_WAIT_SECONDS: u64 = 90;

pub const INSTRUCTIONS: &str = "Controls the Zed editor and the AI agents running in it. \
Start with list_projects and list_agents. create_thread starts an agent thread (optionally in a \
new git worktree, whose setup hooks it waits for) and can send its first message; send_message \
continues a thread. Every user message starts a run, and tools that wait return the run once the \
agent finished it or needs something: answer permission prompts with respond_to_permission and \
questions with answer_question, then call wait_for_run again. A run only shows its outcome unless \
detail is requested; command output is left out of it, get it with get_command_output.";

#[derive(Debug, Deserialize)]
pub struct JsonRpcMessage {
    /// Absent for notifications.
    #[serde(default)]
    pub id: Option<Value>,
    /// Absent for responses to requests of the server, which never sends any.
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

impl JsonRpcResponse {
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Map<String, Value>,
}

/// The result of a tool: its JSON as text and as structured content.
pub fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    });
    if value.is_object() {
        result["structuredContent"] = value.clone();
    }
    result
}

fn wait_properties() -> Value {
    json!({
        "wait": {
            "type": "boolean",
            "description": "Wait until it's done (default true).",
        },
        "timeout_seconds": {
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_WAIT_SECONDS,
            "description": format!(
                "How long to wait at most (default {DEFAULT_WAIT_SECONDS}). When it runs out, \
                 the current state is returned; wait again with wait_for_run or \
                 wait_for_operation."
            ),
        },
    })
}

fn thread_id_property() -> Value {
    json!({ "type": "string", "description": "The thread's id, from list_threads or create_thread." })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn merge(mut left: Value, right: Value) -> Value {
    if let (Some(left), Value::Object(right)) = (left.as_object_mut(), right) {
        left.extend(right);
    }
    left
}

fn tool(name: &str, description: &str, input_schema: Value, read_only: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": { "readOnlyHint": read_only },
    })
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "list_projects",
            "Lists the projects open in Zed, with their git worktrees and branches.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "list_agents",
            "Lists the agents threads can be started with (Zed's own and external ones like Claude), and the default.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "list_threads",
            "Lists the agent threads of every project, with their status.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "create_worktree",
            "Creates a git worktree for a project and runs its create_worktree hooks; the worktree is ready once they finished. Returns the operation, with every step, hook and error.",
            object_schema(
                merge(
                    json!({
                        "project_id": { "type": "string", "description": "From list_projects." },
                        "name": { "type": "string", "description": "Name of the worktree's directory." },
                        "branch": { "type": "string", "description": "Branch to base it on (default: the current branch)." },
                    }),
                    wait_properties(),
                ),
                &["project_id"],
            ),
            false,
        ),
        tool(
            "create_thread",
            "Starts an agent thread in a project, in an existing or a new git worktree, and optionally sends its first message. Waits for the thread to start, and with a message, for its first run.",
            object_schema(
                merge(
                    json!({
                        "project_id": { "type": "string", "description": "From list_projects." },
                        "worktree": {
                            "description": "Where the thread works. Defaults to the project's main worktree.",
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "const": "existing" },
                                        "path": { "type": "string", "description": "One of the project's worktree paths (empty for projects without git)." },
                                    },
                                    "required": ["kind", "path"],
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "const": "new" },
                                        "name": { "type": "string" },
                                        "branch": { "type": "string" },
                                    },
                                    "required": ["kind"],
                                },
                            ],
                        },
                        "agent_id": { "type": "string", "description": "From list_agents (default: Zed's default agent)." },
                        "controls": {
                            "type": "object",
                            "description": "Controls to set before the first message, by id, e.g. {\"config:mode\": \"plan\"}. The ids are in get_thread's state.controls.",
                            "additionalProperties": { "type": ["string", "boolean"] },
                        },
                        "message": { "type": "string", "description": "The first message." },
                    }),
                    wait_properties(),
                ),
                &["project_id"],
            ),
            false,
        ),
        tool(
            "get_thread",
            "Returns a thread's state (status, controls, plan, queue, error) and its runs. Each run shows the agent's final message, or what it's waiting on.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "detail": { "type": "boolean", "description": "Include every message and tool call of the runs (command output is left out)." },
                    "max_runs": { "type": "integer", "minimum": 1, "description": "Only return the last this many runs (default 10)." },
                }),
                &["thread_id"],
            ),
            true,
        ),
        tool(
            "get_run",
            "Returns one run of a thread: everything from one user message to the next.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "run_id": { "type": "string" },
                    "detail": { "type": "boolean", "description": "Include every message and tool call (command output is left out)." },
                }),
                &["thread_id", "run_id"],
            ),
            true,
        ),
        tool(
            "get_command_output",
            "Returns the output of a command a tool call ran.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "tool_call_id": { "type": "string" },
                }),
                &["thread_id", "tool_call_id"],
            ),
            true,
        ),
        tool(
            "send_message",
            "Sends a message to a thread, which starts a new run (or is queued while the agent is busy). Waits for the run by default.",
            object_schema(
                merge(
                    json!({
                        "thread_id": thread_id_property(),
                        "text": { "type": "string" },
                    }),
                    wait_properties(),
                ),
                &["thread_id", "text"],
            ),
            false,
        ),
        tool(
            "wait_for_run",
            "Waits until a run is no longer running: it finished, or the agent waits for a permission or an answer. Returns the run.",
            object_schema(
                merge(
                    json!({
                        "thread_id": thread_id_property(),
                        "run_id": { "type": "string", "description": "Default: the thread's last run." },
                    }),
                    wait_properties(),
                ),
                &["thread_id"],
            ),
            true,
        ),
        tool(
            "cancel_run",
            "Stops the agent's current run.",
            object_schema(json!({ "thread_id": thread_id_property() }), &["thread_id"]),
            false,
        ),
        tool(
            "respond_to_permission",
            "Answers a permission prompt of a run's pending requests, by option id or kind. This is also how agents ask to leave plan mode.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "tool_call_id": { "type": "string" },
                    "option_id": { "type": "string" },
                    "kind": { "type": "string", "enum": ["allow_once", "allow_always", "reject_once", "reject_always"] },
                }),
                &["thread_id", "tool_call_id"],
            ),
            false,
        ),
        tool(
            "answer_question",
            "Answers a question of a run's pending requests.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "question_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["accept", "decline", "cancel"] },
                    "content": { "type": "object", "description": "The answers, matching the question's schema, for accept." },
                }),
                &["thread_id", "question_id", "action"],
            ),
            false,
        ),
        tool(
            "set_thread_control",
            "Sets a thread's control, e.g. its model or mode. The ids and options are in get_thread's state.controls.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "control_id": { "type": "string" },
                    "value": { "type": ["string", "boolean"] },
                }),
                &["thread_id", "control_id", "value"],
            ),
            false,
        ),
        tool(
            "remove_queued_message",
            "Removes a message from a thread's queue.",
            object_schema(
                json!({
                    "thread_id": thread_id_property(),
                    "index": { "type": "integer", "minimum": 0 },
                }),
                &["thread_id", "index"],
            ),
            false,
        ),
        tool(
            "list_operations",
            "Lists recent worktree and thread creations.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "wait_for_operation",
            "Waits until a worktree or thread creation finished, and returns it with every step, hook and error.",
            object_schema(
                merge(
                    json!({ "operation_id": { "type": "string" } }),
                    wait_properties(),
                ),
                &["operation_id"],
            ),
            true,
        ),
        tool(
            "start_pairing",
            "Returns a code that lets one phone pair with Zed's web UI for the next few minutes.",
            object_schema(json!({}), &[]),
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tools_are_well_formed() {
        let tools = tools();
        let mut names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count, "tool names must be unique");
        for tool in &tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            for required in tool["inputSchema"]["required"].as_array().unwrap() {
                assert!(properties.contains_key(required.as_str().unwrap()));
            }
        }
    }

    #[test]
    fn test_tool_result() {
        let result = tool_result(&json!({ "a": 1 }), false);
        assert_eq!(result["content"][0]["text"], "{\"a\":1}");
        assert_eq!(result["structuredContent"]["a"], 1);
        assert_eq!(result["isError"], false);
        let result = tool_result(&json!("boom"), true);
        assert_eq!(result["content"][0]["text"], "boom");
        assert!(result.get("structuredContent").is_none());
    }
}
