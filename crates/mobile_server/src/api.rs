use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use mobile_protocol::ServerMessage;
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq as _;
use tokio::time::Instant;

use crate::Server;
use crate::store::{Device, PushSubscription};
use crate::web_push;

const MAX_FAILED_PAIRING_ATTEMPTS: u32 = 5;
const MAX_PAIRING_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DEVICE_NAME_CHARS: usize = 64;
const MAX_CODE_BYTES: usize = 256;
const DEFAULT_DEVICE_NAME: &str = "Unnamed device";

pub struct PendingPairing {
    code: String,
    expires_at: Instant,
    failed_attempts: u32,
}

impl PendingPairing {
    pub fn new(code: &str, expires_in_seconds: u64) -> Option<Self> {
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

pub fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn ok_response() -> Response {
    (StatusCode::OK, Json(json!({}))).into_response()
}

pub struct AuthenticatedDevice(pub Device);

impl FromRequestParts<Arc<Server>> for AuthenticatedDevice {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        server: &Arc<Server>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
            })
            .filter(|token| !token.is_empty());
        let device = token.and_then(|token| server.store.lock().device_for_token(token).cloned());
        match device {
            Some(device) => Ok(Self(device)),
            None => Err(error_response(StatusCode::UNAUTHORIZED, "unauthorized")),
        }
    }
}

#[derive(Deserialize)]
pub struct PairRequest {
    code: String,
    device_name: String,
}

enum PairingOutcome {
    Paired {
        device_name: String,
        token: String,
    },
    Rejected {
        message: &'static str,
        disarmed: bool,
    },
    StorageFailed,
}

pub async fn pair(State(server): State<Arc<Server>>, Json(request): Json<PairRequest>) -> Response {
    let outcome = {
        let mut pairing = server.pairing.lock();
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
                    // The device is added while the pairing lock is held so
                    // that a code can never pair more than one device.
                    let device_name = sanitize_device_name(&request.device_name);
                    match server.store.lock().add_device(device_name) {
                        Ok((device, token)) => {
                            *pairing = None;
                            PairingOutcome::Paired {
                                device_name: device.name,
                                token,
                            }
                        }
                        Err(error) => {
                            log::error!("failed to save paired device: {error:#}");
                            PairingOutcome::StorageFailed
                        }
                    }
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
        PairingOutcome::Paired { device_name, token } => {
            log::info!("paired device {device_name:?}");
            server
                .send_to_host(ServerMessage::DevicePaired { device_name })
                .await;
            (StatusCode::OK, Json(json!({ "token": token }))).into_response()
        }
        PairingOutcome::Rejected { message, disarmed } => {
            log::warn!("rejected pairing attempt: {message}");
            if disarmed {
                server
                    .send_to_host(ServerMessage::PairingFailed {
                        reason: "too many invalid pairing attempts".to_string(),
                    })
                    .await;
            }
            error_response(StatusCode::UNAUTHORIZED, message)
        }
        PairingOutcome::StorageFailed => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to save device")
        }
    }
}

pub async fn session(AuthenticatedDevice(device): AuthenticatedDevice) -> Response {
    Json(json!({ "device_name": device.name })).into_response()
}

pub async fn push_key(
    State(server): State<Arc<Server>>,
    AuthenticatedDevice(_device): AuthenticatedDevice,
) -> Response {
    let public_key = web_push::public_key_base64url(server.store.lock().vapid_key());
    Json(json!({ "public_key": public_key })).into_response()
}

pub async fn push_subscribe(
    State(server): State<Arc<Server>>,
    AuthenticatedDevice(device): AuthenticatedDevice,
    Json(subscription): Json<PushSubscription>,
) -> Response {
    if let Err(error) = web_push::validate_subscription(&subscription) {
        return error_response(StatusCode::BAD_REQUEST, &format!("{error:#}"));
    }
    let result = server
        .store
        .lock()
        .add_push_subscription(&device.id, subscription);
    match result {
        Ok(()) => {
            log::info!("added push subscription for {:?}", device.name);
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
pub struct UnsubscribeRequest {
    endpoint: String,
}

pub async fn push_unsubscribe(
    State(server): State<Arc<Server>>,
    AuthenticatedDevice(device): AuthenticatedDevice,
    Json(request): Json<UnsubscribeRequest>,
) -> Response {
    let result = server
        .store
        .lock()
        .remove_push_subscription(&device.id, &request.endpoint);
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

/// Revokes the calling device's token, e.g. when the user unpairs from the
/// app. The app closes its own socket afterwards.
pub async fn unpair(
    State(server): State<Arc<Server>>,
    AuthenticatedDevice(device): AuthenticatedDevice,
) -> Response {
    let result = server.store.lock().remove_device(&device.id);
    match result {
        Ok(()) => {
            log::info!("unpaired {:?}", device.name);
            ok_response()
        }
        Err(error) => {
            log::error!("failed to remove device: {error:#}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to unpair")
        }
    }
}

pub async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
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
}
