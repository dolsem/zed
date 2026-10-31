//! Serves Zed's HTTP API: authenticates clients with the API secret, relays
//! their HTTP requests and WebSocket frames to Zed, and streams Zed's answers
//! back. When enabled, also serves the web UI (see the `web_ui` crate). Zed
//! spawns this process and drives it over stdio (see `mobile_protocol`).

mod relay;
mod websocket;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, get};
use axum::{Json, middleware};
use mobile_protocol::{ClientId, HostMessage, RequestId, ServerMessage};
use parking_lot::Mutex;
use serde_json::json;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{Semaphore, mpsc};
use web_ui::WebUi;

use crate::relay::PendingRequest;
use crate::websocket::ClientHandle;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const HOST_QUEUE_CAPACITY: usize = 1024;
const PUSH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Server {
    secret: String,
    clients: Mutex<HashMap<ClientId, ClientHandle>>,
    next_client_id: AtomicU64,
    pending_websockets: Arc<Semaphore>,
    requests: Mutex<HashMap<RequestId, PendingRequest>>,
    next_request_id: AtomicU64,
    host: mpsc::Sender<ServerMessage>,
    web_ui: Option<Arc<WebUi>>,
}

struct Arguments {
    address: String,
    port: u16,
    state_dir: PathBuf,
    web_ui: bool,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            report_startup_failure(&anyhow!(error).context("failed to start the async runtime"));
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(run());
    // Reading stdin happens on a blocking thread that can't be interrupted, so
    // don't wait for it when shutting down.
    runtime.shutdown_timeout(Duration::from_secs(1));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Startup(error)) => {
            report_startup_failure(&error);
            ExitCode::FAILURE
        }
        Err(RunError::Runtime(error)) => {
            log::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

enum RunError {
    /// Failed before announcing `Listening`, so Zed should hear `Failed`.
    Startup(anyhow::Error),
    Runtime(anyhow::Error),
}

fn report_startup_failure(error: &anyhow::Error) {
    log::error!("{error:#}");
    let message = ServerMessage::Failed {
        message: format!("{error:#}"),
    };
    let result = mobile_protocol::encode(&message).and_then(|line| {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line.as_bytes())?;
        stdout.flush()?;
        Ok(())
    });
    if let Err(error) = result {
        log::error!("failed to report startup failure to Zed: {error:#}");
    }
}

async fn run() -> Result<(), RunError> {
    let arguments = parse_arguments(std::env::args().skip(1)).map_err(RunError::Startup)?;
    let secret = std::env::var(mobile_protocol::SECRET_ENV_VAR)
        .ok()
        .filter(|secret| !secret.trim().is_empty())
        .with_context(|| format!("{} is not set", mobile_protocol::SECRET_ENV_VAR))
        .map_err(RunError::Startup)?;

    let (host_sender, host_receiver) = mpsc::channel(HOST_QUEUE_CAPACITY);
    let web_ui = if arguments.web_ui {
        let http_client = reqwest::Client::builder()
            .use_preconfigured_tls(http_client_tls::tls_config())
            .timeout(PUSH_REQUEST_TIMEOUT)
            .build()
            .context("failed to create HTTP client")
            .map_err(RunError::Startup)?;
        Some(
            WebUi::new(
                &arguments.state_dir,
                secret.clone(),
                host_sender.clone(),
                http_client,
            )
            .map_err(RunError::Startup)?,
        )
    } else {
        None
    };

    let listener = tokio::net::TcpListener::bind((arguments.address.as_str(), arguments.port))
        .await
        .with_context(|| {
            format!(
                "failed to listen on {}:{}",
                arguments.address, arguments.port
            )
        })
        .map_err(RunError::Startup)?;
    let local_address = listener
        .local_addr()
        .context("failed to read the bound address")
        .map_err(RunError::Startup)?;

    let server = Arc::new(Server {
        secret,
        clients: Mutex::new(HashMap::default()),
        next_client_id: AtomicU64::new(1),
        pending_websockets: Arc::new(Semaphore::new(websocket::MAX_PENDING_CONNECTIONS)),
        requests: Mutex::new(HashMap::default()),
        next_request_id: AtomicU64::new(1),
        host: host_sender,
        web_ui,
    });

    log::info!("listening on {local_address}");
    server
        .send_to_host(ServerMessage::Listening {
            address: local_address.to_string(),
        })
        .await;

    let router = build_router(server.clone());
    tokio::select! {
        result = axum::serve(listener, router).into_future() => {
            result.context("HTTP server failed").map_err(RunError::Runtime)
        }
        result = write_server_messages(host_receiver) => {
            result.context("failed to write to stdout").map_err(RunError::Runtime)
        }
        result = read_host_messages(server.clone()) => {
            result.map_err(RunError::Runtime)
        }
    }
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> Result<Arguments> {
    let mut address = None;
    let mut port = None;
    let mut state_dir = None;
    let mut web_ui = false;

    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            mobile_protocol::ADDRESS_FLAG => address = Some(value),
            mobile_protocol::PORT_FLAG => {
                port = Some(
                    value
                        .parse::<u16>()
                        .with_context(|| format!("invalid port {value:?}"))?,
                );
            }
            mobile_protocol::STATE_DIR_FLAG => state_dir = Some(PathBuf::from(value)),
            mobile_protocol::WEB_UI_FLAG => {
                web_ui = value
                    .parse::<bool>()
                    .with_context(|| format!("invalid value {value:?} for {flag}"))?;
            }
            _ => bail!("unknown argument {flag:?}"),
        }
    }

    Ok(Arguments {
        address: address
            .with_context(|| format!("{} is required", mobile_protocol::ADDRESS_FLAG))?,
        port: port.with_context(|| format!("{} is required", mobile_protocol::PORT_FLAG))?,
        state_dir: state_dir
            .with_context(|| format!("{} is required", mobile_protocol::STATE_DIR_FLAG))?,
        web_ui,
    })
}

fn build_router(server: Arc<Server>) -> Router {
    let api = Router::new()
        .route("/api/session", get(session))
        .route("/api/ws", get(websocket::upgrade))
        .route("/api", any(relay::forward))
        .route("/api/{*rest}", any(relay::forward))
        .with_state(server.clone());

    let router =
        match &server.web_ui {
            Some(web_ui) => {
                api.merge(web_ui.authenticated_router().route_layer(
                    middleware::from_fn_with_state(server.clone(), require_secret),
                ))
                .merge(web_ui.public_router())
            }
            None => api.fallback(not_found),
        };
    router
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::map_response(add_security_headers))
}

pub fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}

async fn session(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    if !server.is_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    Json(json!({})).into_response()
}

async fn require_secret(
    State(server): State<Arc<Server>>,
    request: Request,
    next: Next,
) -> Response {
    if !server.is_authorized(request.headers()) {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(request).await
}

async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // API responses can carry secrets and must never be stored by proxies.
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn write_server_messages(mut receiver: mpsc::Receiver<ServerMessage>) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    while let Some(message) = receiver.recv().await {
        let line = mobile_protocol::encode(&message)?;
        stdout.write_all(line.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

async fn read_host_messages(server: Arc<Server>) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("failed to read stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        match mobile_protocol::decode::<HostMessage>(&line) {
            Ok(message) => server.handle_host_message(message),
            Err(error) => log::error!("ignoring invalid message from Zed: {error:#}"),
        }
    }
    log::info!("stdin closed, shutting down");
    Ok(())
}

impl Server {
    async fn send_to_host(&self, message: ServerMessage) {
        if self.host.send(message).await.is_err() {
            log::error!("failed to send a message to Zed: stdout writer has stopped");
        }
    }

    fn is_authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
            })
            .is_some_and(|token| self.is_secret(token))
    }

    fn is_secret(&self, candidate: &str) -> bool {
        candidate.as_bytes().ct_eq(self.secret.as_bytes()).into()
    }

    fn handle_host_message(self: &Arc<Self>, message: HostMessage) {
        match message {
            HostMessage::Send { client_id, message } => self.send_to_clients(client_id, message),
            HostMessage::Respond {
                request_id,
                status,
                body,
            } => self.respond(request_id, status, body),
            HostMessage::StartStream { request_id } => self.start_stream(request_id),
            HostMessage::StreamEvent {
                request_id,
                event,
                data,
            } => self.send_stream_event(request_id, event, data),
            HostMessage::EndStream { request_id } => self.end_stream(request_id),
            HostMessage::Notify {
                title,
                body,
                tag,
                url,
            } => {
                let Some(web_ui) = self.web_ui.clone() else {
                    log::warn!("ignoring a notification because the web UI is disabled");
                    return;
                };
                tokio::spawn(async move {
                    if let Err(error) = web_ui.notify(title, body, tag, url).await {
                        log::error!("failed to send notifications: {error:#}");
                    }
                });
            }
            HostMessage::StartPairing {
                code,
                expires_in_seconds,
            } => match &self.web_ui {
                Some(web_ui) => web_ui.start_pairing(&code, expires_in_seconds),
                None => log::warn!("ignoring StartPairing because the web UI is disabled"),
            },
            HostMessage::CancelPairing => {
                if let Some(web_ui) = &self.web_ui {
                    web_ui.cancel_pairing();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_arguments() {
        let arguments = parse_arguments(
            [
                "--address",
                "127.0.0.1",
                "--port",
                "8080",
                "--state-dir",
                "/tmp/x",
                "--web-ui",
                "true",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(arguments.address, "127.0.0.1");
        assert_eq!(arguments.port, 8080);
        assert_eq!(arguments.state_dir, PathBuf::from("/tmp/x"));
        assert!(arguments.web_ui);

        assert!(parse_arguments(["--port", "8080"].map(String::from)).is_err());
        assert!(parse_arguments(["--port", "99999"].map(String::from)).is_err());
        assert!(parse_arguments(["--bogus", "1"].map(String::from)).is_err());
        assert!(parse_arguments(["--web-ui", "yes"].map(String::from)).is_err());
    }
}
