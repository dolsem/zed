//! Serves Zed's mobile PWA, pairs and authenticates devices, relays WebSocket
//! frames between the PWA and Zed, and delivers Web Push notifications. Zed
//! spawns this process and drives it over stdio (see `mobile_protocol`).

mod api;
mod assets;
mod store;
mod web_push;
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
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, Method, Uri, header};
use axum::response::Response;
use axum::routing::{any, get, post};
use mobile_protocol::{ClientId, HostMessage, ServerMessage};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{Semaphore, mpsc};

use crate::api::PendingPairing;
use crate::assets::AssetSource;
use crate::store::{PushSubscription, Store};
use crate::websocket::{ClientHandle, CloseReason};

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const HOST_QUEUE_CAPACITY: usize = 1024;
const PUSH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_NOTIFICATION_TITLE_CHARS: usize = 200;

pub struct Server {
    store: Mutex<Store>,
    pairing: Mutex<Option<PendingPairing>>,
    clients: Mutex<HashMap<ClientId, ClientHandle>>,
    next_client_id: AtomicU64,
    pending_websockets: Arc<Semaphore>,
    host: mpsc::Sender<ServerMessage>,
    http_client: reqwest::Client,
    assets: AssetSource,
}

struct Arguments {
    address: String,
    port: u16,
    state_dir: PathBuf,
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
    let store = Store::load_or_create(&arguments.state_dir).map_err(RunError::Startup)?;
    let http_client = reqwest::Client::builder()
        .use_preconfigured_tls(http_client_tls::tls_config())
        .timeout(PUSH_REQUEST_TIMEOUT)
        .build()
        .context("failed to create HTTP client")
        .map_err(RunError::Startup)?;
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

    let (host_sender, host_receiver) = mpsc::channel(HOST_QUEUE_CAPACITY);
    let server = Arc::new(Server {
        store: Mutex::new(store),
        pairing: Mutex::new(None),
        clients: Mutex::new(HashMap::default()),
        next_client_id: AtomicU64::new(1),
        pending_websockets: Arc::new(Semaphore::new(websocket::MAX_PENDING_CONNECTIONS)),
        host: host_sender,
        http_client,
        assets: AssetSource::from_environment(),
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
            _ => bail!("unknown argument {flag:?}"),
        }
    }

    Ok(Arguments {
        address: address
            .with_context(|| format!("{} is required", mobile_protocol::ADDRESS_FLAG))?,
        port: port.with_context(|| format!("{} is required", mobile_protocol::PORT_FLAG))?,
        state_dir: state_dir
            .with_context(|| format!("{} is required", mobile_protocol::STATE_DIR_FLAG))?,
    })
}

fn build_router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/api/pair", post(api::pair))
        .route("/api/session", get(api::session))
        .route("/api/unpair", post(api::unpair))
        .route("/api/ws", get(websocket::upgrade))
        .route("/api/push/key", get(api::push_key))
        .route("/api/push/subscribe", post(api::push_subscribe))
        .route("/api/push/unsubscribe", post(api::push_unsubscribe))
        .route("/api", any(api::not_found))
        .route("/api/{*rest}", any(api::not_found))
        .fallback(serve_asset)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(axum::middleware::map_response(add_security_headers))
        .with_state(server)
}

async fn serve_asset(
    State(server): State<Arc<Server>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    server.assets.respond(method, uri, headers).await
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
    // API responses can carry tokens and must never be stored by proxies.
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

    fn handle_host_message(self: &Arc<Self>, message: HostMessage) {
        match message {
            HostMessage::Send { client_id, message } => self.send_to_clients(client_id, message),
            HostMessage::Notify {
                title,
                body,
                tag,
                url,
            } => {
                let server = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = server.notify_devices(title, body, tag, url).await {
                        log::error!("failed to send notifications: {error:#}");
                    }
                });
            }
            HostMessage::StartPairing {
                code,
                expires_in_seconds,
            } => match PendingPairing::new(&code, expires_in_seconds) {
                Some(pending) => {
                    log::info!("pairing armed for {expires_in_seconds}s");
                    *self.pairing.lock() = Some(pending);
                }
                None => log::error!("ignoring StartPairing with an empty code"),
            },
            HostMessage::CancelPairing => {
                log::info!("pairing cancelled");
                *self.pairing.lock() = None;
            }
            HostMessage::UnpairAllDevices => {
                // The store lock is held while disconnecting so that no client
                // can authenticate with a revoked token in between.
                let mut store = self.store.lock();
                if let Err(error) = store.remove_all_devices() {
                    log::error!("failed to persist removal of paired devices: {error:#}");
                }
                self.disconnect_all_clients(CloseReason::UNAUTHORIZED);
                log::info!("unpaired all devices");
            }
        }
    }

    async fn notify_devices(
        &self,
        title: String,
        body: String,
        tag: Option<String>,
        url: Option<String>,
    ) -> Result<()> {
        let payload = notification_payload(&title, &body, tag.as_deref(), url.as_deref())?;
        let (vapid_key, targets) = {
            let store = self.store.lock();
            let targets: Vec<(String, PushSubscription)> = store
                .devices()
                .iter()
                .flat_map(|device| {
                    device
                        .push_subscriptions
                        .iter()
                        .map(|subscription| (device.id.clone(), subscription.clone()))
                })
                .collect();
            (store.vapid_key().clone(), targets)
        };
        if targets.is_empty() {
            log::info!("no push subscriptions to notify");
            return Ok(());
        }

        let deliveries = targets.iter().map(|(_, subscription)| {
            web_push::send(&self.http_client, &vapid_key, subscription, &payload)
        });
        let results = futures::future::join_all(deliveries).await;

        let mut gone = Vec::new();
        for ((device_id, subscription), result) in targets.iter().zip(results) {
            match result {
                Ok(web_push::DeliveryOutcome::Delivered) => {}
                Ok(web_push::DeliveryOutcome::Gone) => {
                    log::info!("removing expired push subscription");
                    gone.push((device_id, &subscription.endpoint));
                }
                Err(error) => log::warn!("push delivery failed: {error:#}"),
            }
        }

        if !gone.is_empty() {
            let mut store = self.store.lock();
            for (device_id, endpoint) in gone {
                store.remove_push_subscription(device_id, endpoint)?;
            }
        }
        Ok(())
    }
}

/// Serializes the notification, shortening the body if needed to fit within
/// what push services accept.
fn notification_payload(
    title: &str,
    body: &str,
    tag: Option<&str>,
    url: Option<&str>,
) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Payload<'a> {
        title: &'a str,
        body: &'a str,
        tag: Option<&'a str>,
        url: Option<&'a str>,
    }

    let title = truncate_chars(title, MAX_NOTIFICATION_TITLE_CHARS);
    let mut body_chars = body.chars().count();
    loop {
        let body = truncate_chars(body, body_chars);
        let payload = serde_json::to_vec(&Payload {
            title: &title,
            body: &body,
            tag,
            url,
        })?;
        if payload.len() <= web_push::MAX_PLAINTEXT_LENGTH {
            return Ok(payload);
        }
        if body_chars == 0 {
            bail!("notification is too large to deliver");
        }
        body_chars = body_chars * 3 / 4;
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => {
            let mut truncated = text[..byte_index].to_string();
            truncated.push('…');
            truncated
        }
        None => text.to_string(),
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
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(arguments.address, "127.0.0.1");
        assert_eq!(arguments.port, 8080);
        assert_eq!(arguments.state_dir, PathBuf::from("/tmp/x"));

        assert!(parse_arguments(["--port", "8080"].map(String::from)).is_err());
        assert!(parse_arguments(["--port", "99999"].map(String::from)).is_err());
        assert!(parse_arguments(["--bogus", "1"].map(String::from)).is_err());
    }

    #[test]
    fn test_notification_payload_fits() {
        let payload = notification_payload("Title", &"é".repeat(10_000), Some("t"), None).unwrap();
        assert!(payload.len() <= web_push::MAX_PLAINTEXT_LENGTH);
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["title"], "Title");
        assert_eq!(value["tag"], "t");
        assert!(value["url"].is_null());
        assert!(value["body"].as_str().unwrap().ends_with('…'));

        let payload = notification_payload("Hi", "short", None, Some("#/thread/1")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["body"], "short");
        assert_eq!(value["url"], "#/thread/1");
    }
}
