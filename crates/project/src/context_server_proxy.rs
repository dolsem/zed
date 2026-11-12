//! Lets agents that run their own MCP client, like external ACP agents, use the
//! app-wide context servers instead of each starting its own copy. Other Zed
//! processes on this machine use it the same way, to share a copy this one runs
//! (see [`crate::shared_context_server`]).
//!
//! The proxy serves every app-wide server over MCP's Streamable HTTP transport on
//! localhost, at `/mcp/<server id>`, behind a token that only Zed hands out. It
//! answers `initialize` from the shared server's own handshake, which already
//! happened, and forwards every other request to it. Requests run concurrently on
//! the one connection; the MCP client matches the responses up by id.
//!
//! Only plain request/response traffic goes through: the proxy keeps no
//! event stream open, so notifications from the server, such as a changed tool
//! list, don't reach the agent.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use context_server::{ContextServer, ContextServerId};
use gpui::{App, AppContext as _, AsyncApp, Entity, Global};
use serde_json::{Value, json};
use util::ResultExt as _;

use crate::context_server_store::{ContextServerStatus, ContextServerStore};

/// How long a request waits for its server to finish starting. Agents connect as
/// soon as a thread opens, which can be before the shared server is up.
const STARTUP_WAIT: Duration = Duration::from_secs(45);
const STARTUP_POLL: Duration = Duration::from_millis(250);

/// Where an agent reaches one app-wide server.
pub struct ProxyEndpoint {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

struct GlobalContextServerProxy {
    base_url: String,
    token: String,
}

impl Global for GlobalContextServerProxy {}

/// The endpoint for `server_id`, if the proxy is running.
pub fn endpoint(server_id: &ContextServerId, cx: &App) -> Option<ProxyEndpoint> {
    let (base_url, token) = address(cx)?;
    Some(endpoint_at(&base_url, &token, server_id))
}

/// The proxy's base URL and token, if it is running.
pub(crate) fn address(cx: &App) -> Option<(String, String)> {
    let proxy = cx.try_global::<GlobalContextServerProxy>()?;
    Some((proxy.base_url.clone(), proxy.token.clone()))
}

/// The endpoint for `server_id` on a proxy at `base_url`, which may be another
/// process's.
pub(crate) fn endpoint_at(
    base_url: &str,
    token: &str,
    server_id: &ContextServerId,
) -> ProxyEndpoint {
    ProxyEndpoint {
        url: format!("{base_url}/mcp/{}", urlencoding::encode(&server_id.0)),
        headers: vec![("Authorization".to_string(), format!("Bearer {token}"))],
    }
}

/// Starts serving `store`'s servers, once per app.
pub(crate) fn start(store: Entity<ContextServerStore>, cx: &mut App) {
    if cx.has_global::<GlobalContextServerProxy>() {
        return;
    }
    let server = match tiny_http::Server::http("127.0.0.1:0") {
        Ok(server) => server,
        Err(error) => {
            log::error!("failed to start the context server proxy: {error}");
            return;
        }
    };
    let Some(address) = server.server_addr().to_ip() else {
        log::error!("context server proxy is not listening on an IP address");
        return;
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    cx.set_global(GlobalContextServerProxy {
        base_url: format!("http://{address}"),
        token: token.clone(),
    });

    let (request_tx, request_rx) = async_channel::unbounded::<(tiny_http::Request, String)>();
    std::thread::Builder::new()
        .name("context-server-proxy".into())
        .spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
                    respond(request, 400, None);
                    continue;
                }
                if request_tx.send_blocking((request, body)).is_err() {
                    break;
                }
            }
        })
        .log_err();

    let store = store.downgrade();
    cx.spawn(async move |cx| {
        while let Ok((request, body)) = request_rx.recv().await {
            let store = store.clone();
            let token = token.clone();
            cx.spawn(async move |cx| {
                let (status, body) = match store.upgrade() {
                    Some(store) => handle(&request, &body, &token, store, cx).await,
                    None => (503, None),
                };
                cx.background_spawn(async move { respond(request, status, body) })
                    .await;
            })
            .detach();
        }
    })
    .detach();
}

fn respond(request: tiny_http::Request, status: u16, body: Option<Value>) {
    let response = match body {
        Some(body) => tiny_http::Response::from_string(body.to_string())
            .with_header(
                tiny_http::Header::from_bytes("Content-Type", "application/json")
                    .expect("valid header"),
            )
            .with_status_code(status),
        None => tiny_http::Response::from_string(String::new()).with_status_code(status),
    };
    request.respond(response).log_err();
}

async fn handle(
    request: &tiny_http::Request,
    body: &str,
    token: &str,
    store: Entity<ContextServerStore>,
    cx: &mut AsyncApp,
) -> (u16, Option<Value>) {
    let authorized = request.headers().iter().any(|header| {
        header.field.equiv("Authorization") && header.value.as_str() == format!("Bearer {token}")
    });
    if !authorized {
        return (401, None);
    }
    let Some(server_id) = request
        .url()
        .strip_prefix("/mcp/")
        .and_then(|id| urlencoding::decode(id).ok())
        .map(|id| ContextServerId(id.into_owned().into()))
    else {
        return (404, None);
    };
    // No event stream to open and no session to end: every response comes back
    // on the POST that asked for it.
    if *request.method() != tiny_http::Method::Post {
        return (405, None);
    }

    let Ok(message) = serde_json::from_str::<Value>(body) else {
        return (
            400,
            Some(error_response(Value::Null, -32700, "parse error")),
        );
    };
    match handle_message(message, &server_id, store, cx).await {
        Some(response) => (200, Some(response)),
        None => (202, None),
    }
}

/// Answers one JSON-RPC message for `server_id`, a server in the app-wide `store`.
/// Returns `None` for messages that get no response.
pub(crate) async fn handle_message(
    message: Value,
    server_id: &ContextServerId,
    store: Entity<ContextServerStore>,
    cx: &mut AsyncApp,
) -> Option<Value> {
    // Without a method, the message is a response to a request the server sent;
    // the proxy sends none.
    let method = message.get("method").and_then(Value::as_str)?;
    // Notifications, like `notifications/initialized`, are for the shared
    // connection's own handshake, which is already done.
    let id = message.get("id").cloned()?;

    Some(
        match forward(method, message.get("params").cloned(), server_id, store, cx).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => error_response(id, -32603, &format!("{error:#}")),
        },
    )
}

async fn forward(
    method: &str,
    params: Option<Value>,
    server_id: &ContextServerId,
    store: Entity<ContextServerStore>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let server = running_server(server_id, store, cx).await?;
    let client = server
        .client()
        .with_context(|| format!("context server `{server_id}` is not running"))?;
    match method {
        "initialize" => Ok(serde_json::to_value(&client.initialize)?),
        "ping" => Ok(json!({})),
        _ => client.request_raw(method, params).await,
    }
}

async fn running_server(
    server_id: &ContextServerId,
    store: Entity<ContextServerStore>,
    cx: &mut AsyncApp,
) -> Result<Arc<ContextServer>> {
    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    loop {
        let (server, status) = store.read_with(cx, |store, _| {
            (
                store.get_running_server(server_id),
                store.status_for_server(server_id),
            )
        });
        if let Some(server) = server {
            return Ok(server);
        }
        let starting = matches!(status, Some(ContextServerStatus::Starting) | None);
        if !starting || std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "context server `{server_id}` is not running{}",
                match status {
                    Some(ContextServerStatus::Error(error)) => format!(": {error}"),
                    _ => String::new(),
                }
            );
        }
        cx.background_executor().timer(STARTUP_POLL).await;
    }
}

pub(crate) fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}
