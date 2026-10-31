//! Answers the HTTP API's requests, which the `http_api` server relays
//! from its clients. The endpoints are documented in `mobile_protocol::rest`.

use std::collections::BTreeMap;
use std::fmt;

use agent_ui::ThreadId;
use agent_ui::mobile::{self as agent_mobile, MobileConversation};
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Context, Task};
use mobile_protocol::rest::{self, thread_events};
use mobile_protocol::{HostMessage, RequestId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use util::ResultExt as _;

use crate::MobileBridge;
use crate::threads::{build_new_thread_options, build_threads_list, thread_details};

#[derive(Default)]
pub(crate) struct RestState {
    /// Requests still being worked on, cancelled if their client goes away.
    pending: HashMap<RequestId, Task<()>>,
    pub(crate) streams: HashMap<RequestId, StreamTarget>,
}

pub(crate) enum StreamTarget {
    Thread(String),
    Operation(String),
}

pub(crate) enum Reply {
    Json(u16, serde_json::Value),
    /// The handler started a stream with [`MobileBridge::start_stream`].
    Streaming,
}

/// An error with the HTTP status it should be reported with. Other errors
/// are reported as 500.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: u16,
    message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

fn api_error(status: u16, message: impl Into<String>) -> anyhow::Error {
    ApiError {
        status,
        message: message.into(),
    }
    .into()
}

pub(crate) fn bad_request(message: impl Into<String>) -> anyhow::Error {
    api_error(400, message)
}

pub(crate) fn not_found(message: impl Into<String>) -> anyhow::Error {
    api_error(404, message)
}

/// For requests that don't fit the thread's current state, e.g. answering a
/// permission prompt that was already answered.
pub(crate) fn conflict(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<ApiError>().is_some() {
        return error;
    }
    api_error(409, format!("{error:#}"))
}

pub(crate) fn error_status(error: &anyhow::Error) -> u16 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
        .map_or(500, |error| error.status)
}

pub(crate) fn json<T: Serialize>(value: T) -> Result<Reply> {
    Ok(Reply::Json(200, serde_json::to_value(value)?))
}

fn empty() -> Result<Reply> {
    Ok(Reply::Json(200, serde_json::json!({})))
}

fn parse_body<T: DeserializeOwned>(body: Option<serde_json::Value>) -> Result<T> {
    let body = body.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(body).map_err(|error| bad_request(format!("invalid body: {error}")))
}

fn decode_path(path: &str) -> Result<Vec<String>> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| percent_decode(segment).context("invalid percent-encoding"))
        .collect::<Result<_>>()
        .map_err(|error| bad_request(format!("{error:#}")))
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(byte) = bytes.get(index) {
        if *byte == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(*byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

impl MobileBridge {
    pub(crate) fn handle_rest_request(
        &mut self,
        request_id: RequestId,
        method: &str,
        path: &str,
        query: BTreeMap<String, String>,
        body: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        let mut task = match decode_path(path) {
            Ok(segments) => self.route_rest_request(request_id, method, &segments, query, body, cx),
            Err(error) => Task::ready(Err(error)),
        };
        let waker = futures::task::noop_waker();
        if let std::task::Poll::Ready(result) =
            task.poll_unpin(&mut std::task::Context::from_waker(&waker))
        {
            self.finish_rest_request(request_id, result);
            return;
        }
        let pending = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, _| {
                this.rest.pending.remove(&request_id);
                this.finish_rest_request(request_id, result);
            })
            .ok();
        });
        self.rest.pending.insert(request_id, pending);
    }

    fn finish_rest_request(&mut self, request_id: RequestId, result: Result<Reply>) {
        let (status, body) = match result {
            Ok(Reply::Json(status, body)) => (status, body),
            Ok(Reply::Streaming) => return,
            Err(error) => {
                let status = error_status(&error);
                if status >= 500 {
                    log::error!("HTTP API request failed: {error:#}");
                }
                (status, serde_json::json!({ "error": format!("{error:#}") }))
            }
        };
        self.send_to_running_server(HostMessage::Respond {
            request_id,
            status,
            body,
        });
    }

    pub(crate) fn handle_rest_request_closed(&mut self, request_id: RequestId, _cx: &mut App) {
        self.rest.pending.remove(&request_id);
        self.forget_stream(request_id);
    }

    fn forget_stream(&mut self, request_id: RequestId) {
        match self.rest.streams.remove(&request_id) {
            Some(StreamTarget::Thread(thread_id)) => {
                self.threads.remove_stream(request_id, &thread_id)
            }
            Some(StreamTarget::Operation(operation_id)) => {
                self.operations.remove_stream(request_id, &operation_id)
            }
            None => {}
        }
    }

    /// Forgets every request, e.g. because the server exited.
    pub(crate) fn close_all_requests(&mut self, _cx: &mut App) {
        self.rest.pending.clear();
        let request_ids = self.rest.streams.keys().copied().collect::<Vec<_>>();
        for request_id in request_ids {
            self.forget_stream(request_id);
        }
    }

    fn send_to_running_server(&self, message: HostMessage) {
        if self.server.is_some() {
            self.send_to_server(message).log_err();
        }
    }

    pub(crate) fn start_stream(&self, request_id: RequestId) {
        self.send_to_running_server(HostMessage::StartStream { request_id });
    }

    pub(crate) fn send_stream_event<T: Serialize>(
        &self,
        request_id: RequestId,
        event: &str,
        data: &T,
    ) {
        match serde_json::to_value(data) {
            Ok(data) => self.send_to_running_server(HostMessage::StreamEvent {
                request_id,
                event: event.to_string(),
                data,
            }),
            Err(error) => log::error!("failed to serialize stream event {event}: {error}"),
        }
    }

    pub(crate) fn end_stream(&mut self, request_id: RequestId) {
        self.rest.streams.remove(&request_id);
        self.send_to_running_server(HostMessage::EndStream { request_id });
    }

    pub(crate) fn end_stream_with_error(&mut self, request_id: RequestId, message: &str) {
        self.send_stream_event(
            request_id,
            thread_events::ERROR,
            &rest::StreamError {
                message: message.to_string(),
            },
        );
        self.end_stream(request_id);
    }

    pub(crate) fn route_rest_request(
        &mut self,
        request_id: RequestId,
        method: &str,
        segments: &[String],
        query: BTreeMap<String, String>,
        body: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Reply>> {
        let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
        let include_items = query.get("detail").is_some_and(|detail| detail == "full");
        match (method, segments.as_slice()) {
            ("GET", ["projects"]) => {
                let options = build_new_thread_options(cx);
                cx.background_spawn(async move {
                    json(rest::ProjectsResponse {
                        projects: options.await?.projects,
                    })
                })
            }
            ("GET", ["agents"]) => {
                let options = build_new_thread_options(cx);
                cx.background_spawn(async move {
                    let options = options.await?;
                    json(rest::AgentsResponse {
                        agents: options.agents,
                        default_agent_id: options.default_agent_id,
                    })
                })
            }
            ("POST", ["worktrees"]) => Task::ready(
                parse_body(body).and_then(|request| self.start_create_worktree(request, cx)),
            ),
            ("GET", ["threads"]) => Task::ready(json(build_threads_list(cx))),
            ("POST", ["threads"]) => Task::ready(
                parse_body(body).and_then(|request| self.start_create_thread(request, cx)),
            ),
            ("GET", ["threads", thread_id]) => {
                let thread_id = thread_id.to_string();
                self.with_thread(&thread_id.clone(), cx, move |_, conversation, cx| {
                    json(
                        thread_details(&conversation, &thread_id, include_items, cx)
                            .context("the thread is still loading")?,
                    )
                })
            }
            ("GET", ["threads", thread_id, "events"]) => {
                let thread_id = thread_id.to_string();
                self.with_thread(&thread_id.clone(), cx, move |this, conversation, cx| {
                    this.subscribe_thread_stream(
                        request_id,
                        &thread_id,
                        conversation,
                        include_items,
                        cx,
                    )?;
                    Ok(Reply::Streaming)
                })
            }
            ("GET", ["threads", thread_id, "runs"]) => {
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    json(rest::RunsResponse {
                        runs: agent_mobile::thread_runs(&conversation, include_items, cx),
                    })
                })
            }
            ("GET", ["threads", thread_id, "runs", run_id]) => {
                let run_id = run_id.to_string();
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    json(
                        agent_mobile::thread_run(&conversation, &run_id, include_items, cx)
                            .ok_or_else(|| not_found(format!("no run {run_id:?}")))?,
                    )
                })
            }
            ("GET", ["threads", thread_id, "tool_calls", tool_call_id, "output"]) => {
                let tool_call_id = tool_call_id.to_string();
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    json(
                        agent_mobile::command_output(&conversation, &tool_call_id, cx)
                            .map_err(|error| not_found(format!("{error:#}")))?,
                    )
                })
            }
            ("POST", ["threads", thread_id, "messages"]) => {
                let request = match parse_body::<rest::SendMessageRequest>(body) {
                    Ok(request) => request,
                    Err(error) => return Task::ready(Err(error)),
                };
                if request.text.trim().is_empty() {
                    return Task::ready(Err(bad_request("the message is empty")));
                }
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    json(send_message(&conversation, request.text, cx).map_err(conflict)?)
                })
            }
            ("POST", ["threads", thread_id, "cancel"]) => {
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    agent_mobile::cancel_generation(&conversation, cx).map_err(conflict)?;
                    empty()
                })
            }
            ("POST", ["threads", thread_id, "permissions", tool_call_id]) => {
                let response = match parse_body::<rest::PermissionResponse>(body) {
                    Ok(response) => response,
                    Err(error) => return Task::ready(Err(error)),
                };
                let tool_call_id = tool_call_id.to_string();
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    agent_mobile::respond_to_permission(&conversation, &tool_call_id, response, cx)
                        .map_err(conflict)?;
                    empty()
                })
            }
            ("POST", ["threads", thread_id, "questions", question_id]) => {
                let response = match parse_body::<rest::QuestionResponse>(body) {
                    Ok(response) => response,
                    Err(error) => return Task::ready(Err(error)),
                };
                let question_id = question_id.to_string();
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    agent_mobile::answer_question(&conversation, &question_id, response, cx)
                        .map_err(conflict)?;
                    empty()
                })
            }
            ("PUT", ["threads", thread_id, "controls", control_id]) => {
                let request = match parse_body::<rest::SetControlRequest>(body) {
                    Ok(request) => request,
                    Err(error) => return Task::ready(Err(error)),
                };
                let thread_id = thread_id.to_string();
                let control_id = control_id.to_string();
                let conversation = self.ensure_conversation(&thread_id, cx);
                cx.spawn(async move |this, cx| {
                    let conversation = conversation.await?;
                    let task = cx.update(|cx| {
                        agent_mobile::set_control(&conversation, &control_id, request.value, cx)
                    });
                    task.await.map_err(conflict)?;
                    this.update(cx, |this, cx| this.mark_thread_state_dirty(&thread_id, cx))?;
                    empty()
                })
            }
            ("DELETE", ["threads", thread_id, "queue", index]) => {
                let Ok(index) = index.parse::<usize>() else {
                    return Task::ready(Err(bad_request("the index must be a number")));
                };
                self.with_thread(thread_id, cx, move |_, conversation, cx| {
                    agent_mobile::remove_queued_message(&conversation, index, cx)
                        .map_err(conflict)?;
                    empty()
                })
            }
            ("POST", ["mcp"]) => self.handle_mcp_request(body, cx),
            ("POST", ["pairing"]) => {
                Task::ready(self.begin_pairing().map_err(conflict).and_then(|code| {
                    json(rest::PairingResponse {
                        code,
                        expires_in_seconds: crate::PAIRING_CODE_TTL_SECONDS,
                    })
                }))
            }
            ("GET", ["operations"]) => Task::ready(json(rest::OperationsResponse {
                operations: self.operations.list(),
            })),
            ("GET", ["operations", operation_id]) => Task::ready(
                self.operations
                    .get(operation_id)
                    .ok_or_else(|| not_found(format!("no operation {operation_id:?}")))
                    .and_then(json),
            ),
            ("GET", ["operations", operation_id, "events"]) => Task::ready(
                self.subscribe_operation_stream(request_id, operation_id)
                    .map(|()| Reply::Streaming),
            ),
            (_, path) if is_known_path(path) => {
                Task::ready(Err(api_error(405, format!("{method} is not allowed here"))))
            }
            _ => Task::ready(Err(not_found("not found"))),
        }
    }

    /// The thread's conversation, loading it in Zed if needed.
    pub(crate) fn ensure_conversation(
        &mut self,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<MobileConversation>> {
        let Ok(parsed_thread_id) = ThreadId::from_key_string(thread_id) else {
            return Task::ready(Err(bad_request("invalid thread id")));
        };
        if let Ok(conversation) = self.conversation_for(thread_id, cx) {
            if conversation.thread(cx).is_some() {
                return Task::ready(Ok(conversation));
            }
            if let Some(error) = agent_mobile::conversation_error(&conversation, cx) {
                return Task::ready(Err(api_error(409, error)));
            }
        }
        let is_known = agent_ui::thread_metadata_store::ThreadMetadataStore::try_global(cx)
            .is_some_and(|store| store.read(cx).entry(parsed_thread_id).is_some());
        if !is_known {
            return Task::ready(Err(not_found(format!("no thread {thread_id:?}"))));
        }
        let conversation = self.load_conversation(thread_id, cx);
        cx.background_spawn(async move { conversation.await.map_err(conflict) })
    }

    fn with_thread(
        &mut self,
        thread_id: &str,
        cx: &mut Context<Self>,
        respond: impl FnOnce(&mut Self, MobileConversation, &mut Context<Self>) -> Result<Reply>
        + 'static,
    ) -> Task<Result<Reply>> {
        let conversation = self.ensure_conversation(thread_id, cx);
        cx.spawn(async move |this, cx| {
            let conversation = conversation.await?;
            this.update(cx, |this, cx| respond(this, conversation, cx))?
        })
    }
}

/// Sends `text`, which the agent queues while it's busy.
pub(crate) fn send_message(
    conversation: &MobileConversation,
    text: String,
    cx: &mut App,
) -> Result<rest::SentMessage> {
    let thread = conversation
        .thread(cx)
        .context("the thread is still loading")?;
    let queued = thread.read(cx).status() != acp_thread::ThreadStatus::Idle;
    // Runs are numbered by position, so the message's run is the next one.
    let run_id = (!queued).then(|| {
        let user_runs = agent_mobile::run_ranges(conversation, cx)
            .iter()
            .filter(|range| range.run_id != "0")
            .count();
        (user_runs + 1).to_string()
    });
    agent_mobile::send_message(conversation, text, cx)?;
    Ok(rest::SentMessage { run_id, queued })
}

fn is_known_path(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["projects"]
            | ["agents"]
            | ["mcp"]
            | ["pairing"]
            | ["worktrees"]
            | ["threads"]
            | ["threads", _]
            | ["threads", _, "events" | "runs" | "messages" | "cancel"]
            | [
                "threads",
                _,
                "runs" | "permissions" | "questions" | "controls" | "queue",
                _
            ]
            | ["threads", _, "tool_calls", _, "output"]
            | ["operations"]
            | ["operations", _]
            | ["operations", _, "events"]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn test_decode_path() {
        assert_eq!(
            decode_path("/threads/abc/tool_calls/tool%2F1/output").unwrap(),
            ["threads", "abc", "tool_calls", "tool/1", "output"]
        );
        assert_eq!(decode_path("/").unwrap(), Vec::<String>::new());
        assert!(decode_path("/threads/%zz").is_err());
        assert!(decode_path("/threads/%e2").is_err());
    }

    #[test]
    fn test_error_status() {
        assert_eq!(error_status(&not_found("x")), 404);
        assert_eq!(error_status(&conflict(anyhow!("busy"))), 409);
        assert_eq!(error_status(&conflict(bad_request("bad"))), 400);
        assert_eq!(error_status(&anyhow!("boom")), 500);
        assert_eq!(
            error_status(&not_found("missing").context("while loading")),
            404
        );
    }
}
