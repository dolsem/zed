//! Zed spawns the `mobile_server` executable and talks to it over stdio, one
//! JSON message per line: [`HostMessage`]s go to the server's stdin and
//! [`ServerMessage`]s come back on its stdout. The server's stderr carries logs.
//!
//! The server owns everything HTTP-facing (static PWA assets, device pairing,
//! authentication, Web Push), while Zed owns all editor state. WebSocket text
//! frames are relayed verbatim in both directions, so the server never needs to
//! understand the API spoken between the PWA and Zed, which lives in [`api`].

pub mod api;

use serde::{Deserialize, Serialize};

pub type ClientId = u64;

/// Command-line flags Zed passes when spawning the server.
pub const ADDRESS_FLAG: &str = "--address";
pub const PORT_FLAG: &str = "--port";
pub const STATE_DIR_FLAG: &str = "--state-dir";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// Sends a WebSocket text frame to one authenticated client, or to every
    /// authenticated client when `client_id` is `None`.
    Send {
        client_id: Option<ClientId>,
        message: String,
    },
    /// Delivers a Web Push notification to every paired device.
    Notify {
        title: String,
        body: String,
        /// Collapses notifications about the same subject (e.g. a thread).
        tag: Option<String>,
        /// Location (relative to the app root, e.g. `#/thread/123`) to open
        /// when the notification is tapped.
        url: Option<String>,
    },
    /// Allows exactly one device to pair using `code` until it expires.
    StartPairing {
        code: String,
        expires_in_seconds: u64,
    },
    CancelPairing,
    /// Revokes every paired device and disconnects its clients.
    UnpairAllDevices,
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
        device_name: String,
    },
    ClientDisconnected {
        client_id: ClientId,
    },
    /// A WebSocket text frame received from an authenticated client.
    Received {
        client_id: ClientId,
        message: String,
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
    }
}
