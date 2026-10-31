//! Zed's web app, a PWA for controlling Zed from a phone. The `http_api`
//! server mounts it when the web UI is enabled: this crate serves its static
//! files, pairs devices and delivers Web Push notifications, while the app
//! itself talks to Zed through the HTTP API's WebSocket.
//!
//! Pairing hands the app the HTTP API's secret, which it then authenticates
//! with like any other API client.

mod assets;
mod store;
mod web_push;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mobile_protocol::ServerMessage;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq as _;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::assets::AssetSource;
use crate::store::{PushSubscription, Store};

pub use crate::assets::WEB_DIR_ENV_VAR;

const MAX_FAILED_PAIRING_ATTEMPTS: u32 = 5;
const MAX_PAIRING_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DEVICE_NAME_CHARS: usize = 64;
const MAX_CODE_BYTES: usize = 256;
const DEFAULT_DEVICE_NAME: &str = "Unnamed device";
const MAX_NOTIFICATION_TITLE_CHARS: usize = 200;

pub struct WebUi {
    secret: String,
    store: Mutex<Store>,
    pairing: Mutex<Option<PendingPairing>>,
    host: mpsc::Sender<ServerMessage>,
    http_client: reqwest::Client,
    assets: AssetSource,
}

impl WebUi {
    pub fn new(
        state_dir: &Path,
        secret: String,
        host: mpsc::Sender<ServerMessage>,
        http_client: reqwest::Client,
    ) -> Result<Arc<Self>> {
        let store = Store::load_or_create(state_dir, &secret)?;
        Ok(Arc::new(Self {
            secret,
            store: Mutex::new(store),
            pairing: Mutex::new(None),
            host,
            http_client,
            assets: AssetSource::from_environment(),
        }))
    }

    /// Routes anyone can reach: the app's files and pairing. Includes the
    /// fallback, so it must be the only router with one.
    pub fn public_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/pair", post(pair))
            .fallback(serve_asset)
            .with_state(self.clone())
    }

    /// Routes that the caller must protect with the API secret.
    pub fn authenticated_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/push/key", get(push_key))
            .route("/api/push/subscribe", post(push_subscribe))
            .route("/api/push/unsubscribe", post(push_unsubscribe))
            .with_state(self.clone())
    }

    /// Allows exactly one client to obtain the secret with `code` until it
    /// expires.
    pub fn start_pairing(&self, code: &str, expires_in_seconds: u64) {
        match PendingPairing::new(code, expires_in_seconds) {
            Some(pending) => {
                log::info!("pairing armed for {expires_in_seconds}s");
                *self.pairing.lock() = Some(pending);
            }
            None => log::error!("ignoring StartPairing with an empty code"),
        }
    }

    pub fn cancel_pairing(&self) {
        log::info!("pairing cancelled");
        *self.pairing.lock() = None;
    }

    pub async fn notify(
        &self,
        title: String,
        body: String,
        tag: Option<String>,
        url: Option<String>,
    ) -> Result<()> {
        let payload = notification_payload(&title, &body, tag.as_deref(), url.as_deref())?;
        let (vapid_key, targets) = {
            let store = self.store.lock();
            (
                store.vapid_key().clone(),
                store.push_subscriptions().to_vec(),
            )
        };
        if targets.is_empty() {
            log::info!("no push subscriptions to notify");
            return Ok(());
        }

        let deliveries = targets.iter().map(|subscription| {
            web_push::send(&self.http_client, &vapid_key, subscription, &payload)
        });
        let results = futures::future::join_all(deliveries).await;

        let mut gone = Vec::new();
        for (subscription, result) in targets.iter().zip(results) {
            match result {
                Ok(web_push::DeliveryOutcome::Delivered) => {}
                Ok(web_push::DeliveryOutcome::Gone) => {
                    log::info!("removing expired push subscription");
                    gone.push(&subscription.endpoint);
                }
                Err(error) => log::warn!("push delivery failed: {error:#}"),
            }
        }

        if !gone.is_empty() {
            let mut store = self.store.lock();
            for endpoint in gone {
                store.remove_push_subscription(endpoint)?;
            }
        }
        Ok(())
    }

    async fn send_to_host(&self, message: ServerMessage) {
        if self.host.send(message).await.is_err() {
            log::error!("failed to send a message to Zed: stdout writer has stopped");
        }
    }
}

struct PendingPairing {
    code: String,
    expires_at: Instant,
    failed_attempts: u32,
}

impl PendingPairing {
    fn new(code: &str, expires_in_seconds: u64) -> Option<Self> {
        let code = normalize_code(code);
        if code.is_empty() {
            return None;
        }
        let duration = Duration::from_secs(expires_in_seconds).min(MAX_PAIRING_DURATION);
        Some(Self {
            code,
            expires_at: Instant::now() + duration,
            failed_attempts: 0,
        })
    }
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

fn sanitize_device_name(name: &str) -> String {
    let name = name
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let name = name
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME_CHARS)
        .collect::<String>();
    let name = name.trim_end();
    if name.is_empty() {
        DEFAULT_DEVICE_NAME.to_string()
    } else {
        name.to_string()
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn ok_response() -> Response {
    (StatusCode::OK, Json(json!({}))).into_response()
}

async fn serve_asset(
    State(web_ui): State<Arc<WebUi>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    // Unknown API paths must not be answered with the app's HTML shell.
    if uri.path() == "/api" || uri.path().starts_with("/api/") {
        return error_response(StatusCode::NOT_FOUND, "not found");
    }
    web_ui.assets.respond(method, uri, headers).await
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    device_name: String,
}

#[derive(Serialize)]
struct PairResponse<'a> {
    secret: &'a str,
}

enum PairingOutcome {
    Paired,
    Rejected {
        message: &'static str,
        disarmed: bool,
    },
}

async fn pair(State(web_ui): State<Arc<WebUi>>, Json(request): Json<PairRequest>) -> Response {
    let outcome = {
        let mut pairing = web_ui.pairing.lock();
        match pairing.as_mut() {
            None => PairingOutcome::Rejected {
                message: "no pairing in progress",
                disarmed: false,
            },
            Some(pending) if Instant::now() >= pending.expires_at => {
                *pairing = None;
                PairingOutcome::Rejected {
                    message: "pairing code expired",
                    disarmed: false,
                }
            }
            Some(pending) => {
                let submitted_code = if request.code.len() <= MAX_CODE_BYTES {
                    normalize_code(&request.code)
                } else {
                    String::new()
                };
                let matches: bool = submitted_code
                    .as_bytes()
                    .ct_eq(pending.code.as_bytes())
                    .into();
                if matches {
                    // A code can only ever pair one device.
                    *pairing = None;
                    PairingOutcome::Paired
                } else {
                    pending.failed_attempts += 1;
                    let disarmed = pending.failed_attempts >= MAX_FAILED_PAIRING_ATTEMPTS;
                    if disarmed {
                        *pairing = None;
                    }
                    PairingOutcome::Rejected {
                        message: "invalid pairing code",
                        disarmed,
                    }
                }
            }
        }
    };

    match outcome {
        PairingOutcome::Paired => {
            let device_name = sanitize_device_name(&request.device_name);
            log::info!("paired device {device_name:?}");
            web_ui
                .send_to_host(ServerMessage::DevicePaired { device_name })
                .await;
            (
                StatusCode::OK,
                Json(PairResponse {
                    secret: &web_ui.secret,
                }),
            )
                .into_response()
        }
        PairingOutcome::Rejected { message, disarmed } => {
            log::warn!("rejected pairing attempt: {message}");
            if disarmed {
                web_ui
                    .send_to_host(ServerMessage::PairingFailed {
                        reason: "too many invalid pairing attempts".to_string(),
                    })
                    .await;
            }
            error_response(StatusCode::UNAUTHORIZED, message)
        }
    }
}

async fn push_key(State(web_ui): State<Arc<WebUi>>) -> Response {
    let public_key = web_push::public_key_base64url(web_ui.store.lock().vapid_key());
    Json(json!({ "public_key": public_key })).into_response()
}

async fn push_subscribe(
    State(web_ui): State<Arc<WebUi>>,
    Json(subscription): Json<PushSubscription>,
) -> Response {
    if let Err(error) = web_push::validate_subscription(&subscription) {
        return error_response(StatusCode::BAD_REQUEST, &format!("{error:#}"));
    }
    let result = web_ui.store.lock().add_push_subscription(subscription);
    match result {
        Ok(()) => {
            log::info!("added a push subscription");
            ok_response()
        }
        Err(error) => {
            log::error!("failed to save push subscription: {error:#}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to save subscription",
            )
        }
    }
}

#[derive(Deserialize)]
struct UnsubscribeRequest {
    endpoint: String,
}

async fn push_unsubscribe(
    State(web_ui): State<Arc<WebUi>>,
    Json(request): Json<UnsubscribeRequest>,
) -> Response {
    let result = web_ui
        .store
        .lock()
        .remove_push_subscription(&request.endpoint);
    match result {
        Ok(_) => ok_response(),
        Err(error) => {
            log::error!("failed to remove push subscription: {error:#}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to remove subscription",
            )
        }
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
    fn test_normalize_code() {
        assert_eq!(normalize_code(" ab3-x9 k "), "AB3X9K");
        assert_eq!(normalize_code("--"), "");
    }

    #[test]
    fn test_sanitize_device_name() {
        assert_eq!(sanitize_device_name("  iPhone\n "), "iPhone");
        assert_eq!(sanitize_device_name("\u{7}"), DEFAULT_DEVICE_NAME);
        assert_eq!(
            sanitize_device_name(&"x".repeat(500)).chars().count(),
            MAX_DEVICE_NAME_CHARS
        );
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
