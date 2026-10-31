//! Zed spawns the `http_api` executable and talks to it over stdio, one JSON
//! message per line: [`HostMessage`]s go to the server's stdin and
//! [`ServerMessage`]s come back on its stdout. The server's stderr carries logs.
//!
//! The server owns everything HTTP-facing (authentication with the API secret,
//! and when the web UI is enabled, its static assets, pairing and Web Push),
//! while Zed owns all editor state. The server doesn't understand the APIs it
//! relays: WebSocket text frames are forwarded verbatim in both directions (see
//! [`api`] for what the web UI speaks over them), and authenticated HTTP
//! requests under `/api/` are forwarded as [`ServerMessage::Request`] and
//! answered with [`HostMessage::Respond`] or a stream of server-sent events
//! (see [`rest`]).

pub mod api;
pub mod rest;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type ClientId = u64;
pub type RequestId = u64;

/// Command-line flags Zed passes when spawning the server.
pub const ADDRESS_FLAG: &str = "--address";
pub const PORT_FLAG: &str = "--port";
pub const STATE_DIR_FLAG: &str = "--state-dir";
/// `true` to also serve the web UI.
pub const WEB_UI_FLAG: &str = "--web-ui";
/// The secret clients authenticate with. Passed in the environment rather
/// than as a flag so that other users can't read it from the process list.
pub const SECRET_ENV_VAR: &str = "ZED_HTTP_API_SECRET";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// Sends a WebSocket text frame to one authenticated client, or to every
    /// authenticated client when `client_id` is `None`.
    Send {
        client_id: Option<ClientId>,
        message: String,
    },
    /// Answers a [`ServerMessage::Request`] with a JSON body.
    Respond {
        request_id: RequestId,
        status: u16,
        body: serde_json::Value,
    },
    /// Answers a [`ServerMessage::Request`] with a `text/event-stream`
    /// response, whose events follow as [`HostMessage::StreamEvent`]s.
    StartStream {
        request_id: RequestId,
    },
    StreamEvent {
        request_id: RequestId,
        event: String,
        data: serde_json::Value,
    },
    /// Ends a stream started with [`HostMessage::StartStream`].
    EndStream {
        request_id: RequestId,
    },
    /// Delivers a Web Push notification to every device that subscribed to
    /// them in the web UI.
    Notify {
        title: String,
        body: String,
        /// Collapses notifications about the same subject (e.g. a thread).
        tag: Option<String>,
        /// Location (relative to the app root, e.g. `#/thread/123`) to open
        /// when the notification is tapped.
        url: Option<String>,
    },
    /// Allows exactly one web UI client to obtain the API secret with `code`
    /// until it expires.
    StartPairing {
        code: String,
        expires_in_seconds: u64,
    },
    CancelPairing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Listening {
        address: String,
    },
    /// The server could not start (e.g. the address is already in use).
    Failed {
        message: String,
    },
    ClientConnected {
        client_id: ClientId,
        /// The name the client gave itself, if any.
        device_name: Option<String>,
    },
    ClientDisconnected {
        client_id: ClientId,
    },
    /// A WebSocket text frame received from an authenticated client.
    Received {
        client_id: ClientId,
        message: String,
    },
    /// An authenticated HTTP request under `/api/`, which Zed must answer
    /// with exactly one [`HostMessage::Respond`] or
    /// [`HostMessage::StartStream`].
    Request {
        request_id: RequestId,
        method: String,
        /// The path without the `/api` prefix, e.g. `/threads/123`.
        path: String,
        query: BTreeMap<String, String>,
        /// `None` when the request had no body.
        body: Option<serde_json::Value>,
    },
    /// The client went away (or timed out) before its request was answered
    /// or while it was streaming, so Zed can stop working on it.
    RequestClosed {
        request_id: RequestId,
    },
    DevicePaired {
        device_name: String,
    },
    PairingFailed {
        reason: String,
    },
}

pub fn encode<T: Serialize>(message: &T) -> anyhow::Result<String> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> anyhow::Result<T> {
    Ok(serde_json::from_str(line.trim_end())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let message = HostMessage::Send {
            client_id: Some(3),
            message: "{\"event\":\"x\"}\n".into(),
        };
        let line = encode(&message).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(decode::<HostMessage>(&line).unwrap(), message);

        let message = ServerMessage::Request {
            request_id: 7,
            method: "POST".into(),
            path: "/threads".into(),
            query: BTreeMap::from_iter([("detail".to_string(), "full".to_string())]),
            body: Some(serde_json::json!({ "text": "hi\nthere" })),
        };
        let line = encode(&message).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(decode::<ServerMessage>(&line).unwrap(), message);
    }
}
