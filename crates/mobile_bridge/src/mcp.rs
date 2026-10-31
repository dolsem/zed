//! Serves the HTTP API as an MCP server (see `mobile_protocol::mcp`). Every
//! tool goes through the REST routes, so the two behave the same.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use gpui::{AppContext as _, AsyncApp, Context, Task, WeakEntity};
use mobile_protocol::mcp::{
    self, DEFAULT_WAIT_SECONDS, INVALID_PARAMS, INVALID_REQUEST, JsonRpcMessage, JsonRpcResponse,
    MAX_WAIT_SECONDS, METHOD_NOT_FOUND, PARSE_ERROR, PROTOCOL_VERSIONS, ToolCallParams,
};
use serde_json::{Map, Value, json};

use crate::MobileBridge;
use crate::rest::Reply;

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_MAX_RUNS: usize = 10;

enum ToolError {
    /// The arguments don't fit the tool, reported as a JSON-RPC error.
    Invalid(String),
    /// The tool ran and failed, reported in its result for the model to see.
    Failed(String),
}

impl MobileBridge {
    pub(crate) fn handle_mcp_request(
        &mut self,
        body: Option<Value>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Reply>> {
        let Some(body) = body else {
            let response = JsonRpcResponse::error(Value::Null, PARSE_ERROR, "the body is empty");
            return Task::ready(
                serde_json::to_value(response)
                    .map(|response| Reply::Json(400, response))
                    .map_err(Into::into),
            );
        };
        let is_batch = body.is_array();
        let messages = match body {
            Value::Array(messages) => messages,
            message => vec![message],
        };
        let responses = messages
            .into_iter()
            .map(|message| self.handle_mcp_message(message, cx))
            .collect::<Vec<_>>();
        cx.background_spawn(async move {
            let mut responses = futures::future::join_all(responses)
                .await
                .into_iter()
                .flatten()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            // Notifications and responses get no answer.
            if responses.is_empty() {
                return Ok(Reply::Json(202, Value::Null));
            }
            let body = if is_batch {
                Value::Array(responses)
            } else {
                responses.remove(0)
            };
            Ok(Reply::Json(200, body))
        })
    }

    fn handle_mcp_message(
        &mut self,
        message: Value,
        cx: &mut Context<Self>,
    ) -> Task<Option<JsonRpcResponse>> {
        let message = match serde_json::from_value::<JsonRpcMessage>(message) {
            Ok(message) => message,
            Err(error) => {
                return Task::ready(Some(JsonRpcResponse::error(
                    Value::Null,
                    INVALID_REQUEST,
                    format!("invalid JSON-RPC message: {error}"),
                )));
            }
        };
        let (Some(method), Some(id)) = (message.method, message.id) else {
            return Task::ready(None);
        };
        let response = match method.as_str() {
            "initialize" => {
                let requested = message.params["protocolVersion"].as_str();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|version| Some(**version) == requested)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                JsonRpcResponse::result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": {
                            "name": mcp::SERVER_NAME,
                            "version": release_channel::AppVersion::global(cx).to_string(),
                        },
                        "instructions": mcp::INSTRUCTIONS,
                    }),
                )
            }
            "ping" => JsonRpcResponse::result(id, json!({})),
            "tools/list" => JsonRpcResponse::result(id, json!({ "tools": mcp::tools() })),
            "tools/call" => {
                let params = match serde_json::from_value::<ToolCallParams>(message.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return Task::ready(Some(JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            format!("invalid tool call: {error}"),
                        )));
                    }
                };
                let bridge = cx.weak_entity();
                return cx.spawn(async move |_, cx| {
                    Some(
                        match call_tool(&bridge, &params.name, &params.arguments, cx).await {
                            Ok(value) => {
                                JsonRpcResponse::result(id, mcp::tool_result(&value, false))
                            }
                            Err(ToolError::Invalid(message)) => {
                                JsonRpcResponse::error(id, INVALID_PARAMS, message)
                            }
                            Err(ToolError::Failed(message)) => {
                                JsonRpcResponse::result(id, mcp::tool_result(&json!(message), true))
                            }
                        },
                    )
                });
            }
            method => {
                JsonRpcResponse::error(id, METHOD_NOT_FOUND, format!("unknown method {method:?}"))
            }
        };
        Task::ready(Some(response))
    }
}

/// Answers a REST request, returning its status and body.
async fn rest(
    bridge: &WeakEntity<MobileBridge>,
    method: &'static str,
    segments: &[&str],
    query: &[(&str, &str)],
    body: Option<Value>,
    cx: &mut AsyncApp,
) -> Result<(u16, Value), ToolError> {
    let segments = segments
        .iter()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    let query = query
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>();
    let task = bridge
        .update(cx, |bridge, cx| {
            // Request id 0 is never relayed, and no route used here streams.
            bridge.route_rest_request(0, method, &segments, query, body, cx)
        })
        .map_err(|error| ToolError::Failed(format!("{error:#}")))?;
    match task.await {
        Ok(Reply::Json(status, body)) => Ok((status, body)),
        Ok(Reply::Streaming) => Err(ToolError::Failed("unexpected stream".to_string())),
        Err(error) => Ok((
            crate::rest::error_status(&error),
            json!({ "error": format!("{error:#}") }),
        )),
    }
}

/// Like [`rest`], failing the tool on error statuses.
async fn rest_ok(
    bridge: &WeakEntity<MobileBridge>,
    method: &'static str,
    segments: &[&str],
    query: &[(&str, &str)],
    body: Option<Value>,
    cx: &mut AsyncApp,
) -> Result<Value, ToolError> {
    let (status, body) = rest(bridge, method, segments, query, body, cx).await?;
    if status >= 400 {
        return Err(ToolError::Failed(
            body["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| body.to_string()),
        ));
    }
    // Actions answer `{}`, which reads oddly as a tool's result.
    if body.as_object().is_some_and(Map::is_empty) {
        return Ok(json!({ "ok": true }));
    }
    Ok(body)
}

fn string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Invalid(format!("{key} is required and must be a string")))
}

fn optional_string<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ToolError::Invalid(format!("{key} must be a string"))),
    }
}

fn flag(arguments: &Map<String, Value>, key: &str, default: bool) -> Result<bool, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolError::Invalid(format!("{key} must be a boolean"))),
    }
}

fn number(arguments: &Map<String, Value>, key: &str) -> Result<Option<u64>, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::Invalid(format!("{key} must be a non-negative integer"))),
    }
}

fn wait_timeout(arguments: &Map<String, Value>) -> Result<Duration, ToolError> {
    let seconds = number(arguments, "timeout_seconds")?
        .unwrap_or(DEFAULT_WAIT_SECONDS)
        .clamp(1, MAX_WAIT_SECONDS);
    Ok(Duration::from_secs(seconds))
}

fn detail_query(
    arguments: &Map<String, Value>,
) -> Result<Vec<(&'static str, &'static str)>, ToolError> {
    Ok(if flag(arguments, "detail", false)? {
        vec![("detail", "full")]
    } else {
        Vec::new()
    })
}

/// Arguments copied into a REST body, leaving out the tool's own ones.
fn body_of(arguments: &Map<String, Value>, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                arguments
                    .get(*key)
                    .filter(|value| !value.is_null())
                    .map(|value| (key.to_string(), value.clone()))
            })
            .collect(),
    )
}

async fn call_tool(
    bridge: &WeakEntity<MobileBridge>,
    name: &str,
    arguments: &Map<String, Value>,
    cx: &mut AsyncApp,
) -> Result<Value, ToolError> {
    match name {
        "list_projects" => rest_ok(bridge, "GET", &["projects"], &[], None, cx).await,
        "list_agents" => rest_ok(bridge, "GET", &["agents"], &[], None, cx).await,
        "list_threads" => rest_ok(bridge, "GET", &["threads"], &[], None, cx).await,
        "list_operations" => rest_ok(bridge, "GET", &["operations"], &[], None, cx).await,
        "start_pairing" => rest_ok(bridge, "POST", &["pairing"], &[], None, cx).await,
        "create_worktree" => {
            string(arguments, "project_id")?;
            let body = body_of(arguments, &["project_id", "name", "branch"]);
            let operation = rest_ok(bridge, "POST", &["worktrees"], &[], Some(body), cx).await?;
            if !flag(arguments, "wait", true)? {
                return Ok(operation);
            }
            let deadline = cx.background_executor().now() + wait_timeout(arguments)?;
            wait_for_operation(bridge, operation_id(&operation)?, deadline, cx).await
        }
        "create_thread" => {
            string(arguments, "project_id")?;
            let mut body = body_of(
                arguments,
                &["project_id", "worktree", "agent_id", "controls", "message"],
            );
            if body.get("worktree").is_none() {
                // An empty path stands for the project's main worktree.
                body["worktree"] = json!({ "kind": "existing", "path": "" });
            }
            let operation = rest_ok(bridge, "POST", &["threads"], &[], Some(body), cx).await?;
            if !flag(arguments, "wait", true)? {
                return Ok(operation);
            }
            let deadline = cx.background_executor().now() + wait_timeout(arguments)?;
            let operation =
                wait_for_operation(bridge, operation_id(&operation)?, deadline, cx).await?;
            let result = &operation["result"];
            let (Some(thread_id), Some(run_id)) =
                (result["thread_id"].as_str(), result["run_id"].as_str())
            else {
                return Ok(operation);
            };
            let run = wait_for_run(bridge, thread_id, Some(run_id), deadline, cx).await?;
            Ok(json!({ "operation": operation, "run": run }))
        }
        "get_thread" => {
            let thread_id = string(arguments, "thread_id")?;
            let max_runs = number(arguments, "max_runs")?
                .map_or(DEFAULT_MAX_RUNS, |max_runs| max_runs.max(1) as usize);
            let query = detail_query(arguments)?;
            let mut thread =
                rest_ok(bridge, "GET", &["threads", thread_id], &query, None, cx).await?;
            if let Some(runs) = thread["runs"].as_array_mut()
                && runs.len() > max_runs
            {
                let omitted = runs.len() - max_runs;
                runs.drain(..omitted);
                thread["earlier_runs_omitted"] = json!(omitted);
            }
            Ok(thread)
        }
        "get_run" => {
            let thread_id = string(arguments, "thread_id")?;
            let run_id = string(arguments, "run_id")?;
            let query = detail_query(arguments)?;
            rest_ok(
                bridge,
                "GET",
                &["threads", thread_id, "runs", run_id],
                &query,
                None,
                cx,
            )
            .await
        }
        "get_command_output" => {
            let thread_id = string(arguments, "thread_id")?;
            let tool_call_id = string(arguments, "tool_call_id")?;
            rest_ok(
                bridge,
                "GET",
                &["threads", thread_id, "tool_calls", tool_call_id, "output"],
                &[],
                None,
                cx,
            )
            .await
        }
        "send_message" => {
            let thread_id = string(arguments, "thread_id")?;
            let body = body_of(arguments, &["text"]);
            string(arguments, "text")?;
            let sent = rest_ok(
                bridge,
                "POST",
                &["threads", thread_id, "messages"],
                &[],
                Some(body),
                cx,
            )
            .await?;
            let run_id = sent["run_id"].as_str();
            if !flag(arguments, "wait", true)? || run_id.is_none() {
                return Ok(sent);
            }
            let deadline = cx.background_executor().now() + wait_timeout(arguments)?;
            let run = wait_for_run(bridge, thread_id, run_id, deadline, cx).await?;
            Ok(json!({ "sent": sent, "run": run }))
        }
        "wait_for_run" => {
            let thread_id = string(arguments, "thread_id")?;
            let run_id = optional_string(arguments, "run_id")?;
            let deadline = cx.background_executor().now() + wait_timeout(arguments)?;
            wait_for_run(bridge, thread_id, run_id, deadline, cx).await
        }
        "cancel_run" => {
            let thread_id = string(arguments, "thread_id")?;
            rest_ok(
                bridge,
                "POST",
                &["threads", thread_id, "cancel"],
                &[],
                None,
                cx,
            )
            .await
        }
        "respond_to_permission" => {
            let thread_id = string(arguments, "thread_id")?;
            let tool_call_id = string(arguments, "tool_call_id")?;
            let body = body_of(arguments, &["option_id", "kind"]);
            rest_ok(
                bridge,
                "POST",
                &["threads", thread_id, "permissions", tool_call_id],
                &[],
                Some(body),
                cx,
            )
            .await
        }
        "answer_question" => {
            let thread_id = string(arguments, "thread_id")?;
            let question_id = string(arguments, "question_id")?;
            string(arguments, "action")?;
            let body = body_of(arguments, &["action", "content"]);
            rest_ok(
                bridge,
                "POST",
                &["threads", thread_id, "questions", question_id],
                &[],
                Some(body),
                cx,
            )
            .await
        }
        "set_thread_control" => {
            let thread_id = string(arguments, "thread_id")?;
            let control_id = string(arguments, "control_id")?;
            let body = body_of(arguments, &["value"]);
            rest_ok(
                bridge,
                "PUT",
                &["threads", thread_id, "controls", control_id],
                &[],
                Some(body),
                cx,
            )
            .await
        }
        "remove_queued_message" => {
            let thread_id = string(arguments, "thread_id")?;
            let index = number(arguments, "index")?
                .ok_or_else(|| ToolError::Invalid("index is required".to_string()))?
                .to_string();
            rest_ok(
                bridge,
                "DELETE",
                &["threads", thread_id, "queue", &index],
                &[],
                None,
                cx,
            )
            .await
        }
        "wait_for_operation" => {
            let operation_id = string(arguments, "operation_id")?;
            let deadline = cx.background_executor().now() + wait_timeout(arguments)?;
            wait_for_operation(bridge, operation_id, deadline, cx).await
        }
        name => Err(ToolError::Invalid(format!("unknown tool {name:?}"))),
    }
}

fn operation_id(operation: &Value) -> Result<&str, ToolError> {
    operation["operation_id"]
        .as_str()
        .ok_or_else(|| ToolError::Failed("the operation has no id".to_string()))
}

async fn wait_for_operation(
    bridge: &WeakEntity<MobileBridge>,
    operation_id: &str,
    deadline: std::time::Instant,
    cx: &mut AsyncApp,
) -> Result<Value, ToolError> {
    loop {
        let mut operation =
            rest_ok(bridge, "GET", &["operations", operation_id], &[], None, cx).await?;
        if operation["status"] != "running" {
            return Ok(operation);
        }
        if cx.background_executor().now() >= deadline {
            operation["wait_timed_out"] = json!(true);
            return Ok(operation);
        }
        cx.background_executor().timer(WAIT_POLL_INTERVAL).await;
    }
}

/// Waits until the run (by default the thread's last) no longer runs.
async fn wait_for_run(
    bridge: &WeakEntity<MobileBridge>,
    thread_id: &str,
    run_id: Option<&str>,
    deadline: std::time::Instant,
    cx: &mut AsyncApp,
) -> Result<Value, ToolError> {
    loop {
        let run = match run_id {
            Some(run_id) => {
                let (status, run) = rest(
                    bridge,
                    "GET",
                    &["threads", thread_id, "runs", run_id],
                    &[],
                    None,
                    cx,
                )
                .await?;
                match status {
                    // A message's run only shows up once the agent got it.
                    404 => None,
                    status if status >= 400 => {
                        return Err(ToolError::Failed(
                            run["error"]
                                .as_str()
                                .unwrap_or("request failed")
                                .to_string(),
                        ));
                    }
                    _ => Some(run),
                }
            }
            None => rest_ok(
                bridge,
                "GET",
                &["threads", thread_id, "runs"],
                &[],
                None,
                cx,
            )
            .await?["runs"]
                .as_array()
                .and_then(|runs| runs.last().cloned()),
        };
        let is_running = run.as_ref().is_none_or(|run| run["status"] == "running");
        if !is_running {
            return Ok(run.unwrap_or_default());
        }
        if cx.background_executor().now() >= deadline {
            let mut run = run.unwrap_or_else(|| json!({ "run_id": run_id, "status": "starting" }));
            run["wait_timed_out"] = json!(true);
            return Ok(run);
        }
        cx.background_executor().timer(WAIT_POLL_INTERVAL).await;
    }
}
