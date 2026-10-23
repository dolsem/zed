use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use mobile_protocol::{ClientId, ServerMessage};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use crate::Server;

pub const MAX_INCOMING_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// Browsers answer pings automatically, so this much silence means the
/// connection is dead even if TCP hasn't noticed yet.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const OUTGOING_QUEUE_CAPACITY: usize = 1024;
pub const MAX_PENDING_CONNECTIONS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct CloseReason {
    code: u16,
    reason: &'static str,
}

impl CloseReason {
    pub const UNAUTHORIZED: Self = Self {
        code: 4001,
        reason: "unauthorized",
    };
    const AUTH_TIMEOUT: Self = Self {
        code: 1008,
        reason: "authentication timed out",
    };
    const EXPECTED_AUTH: Self = Self {
        code: 1008,
        reason: "expected an auth message",
    };
    const BINARY_UNSUPPORTED: Self = Self {
        code: 1003,
        reason: "binary messages are not supported",
    };
    const TOO_SLOW: Self = Self {
        code: 1013,
        reason: "client is not keeping up",
    };
}

pub struct ClientHandle {
    outgoing: mpsc::Sender<Utf8Bytes>,
    close: oneshot::Sender<CloseReason>,
}

impl ClientHandle {
    pub fn close(self, reason: CloseReason) {
        if self.close.send(reason).is_err() {
            log::debug!("client connection already ended before it could be closed");
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientAuthMessage {
    Auth { token: String },
}

enum AuthFailure {
    Close(CloseReason),
    Disconnected,
}

pub async fn upgrade(State(server): State<Arc<Server>>, upgrade: WebSocketUpgrade) -> Response {
    // Bounds the memory unauthenticated sockets can hold, since each may send
    // one maximum-size frame before it is checked.
    let Ok(pending_permit) = server.pending_websockets.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(MAX_INCOMING_MESSAGE_BYTES)
        .max_frame_size(MAX_INCOMING_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            run_connection(server, socket, pending_permit).await;
        })
}

async fn run_connection(
    server: Arc<Server>,
    mut socket: WebSocket,
    pending_permit: tokio::sync::OwnedSemaphorePermit,
) {
    let token = match tokio::time::timeout(AUTH_TIMEOUT, read_auth_token(&mut socket)).await {
        Ok(Ok(token)) => token,
        Ok(Err(AuthFailure::Close(reason))) => {
            send_close(&mut socket, reason).await;
            return;
        }
        Ok(Err(AuthFailure::Disconnected)) => return,
        Err(_) => {
            send_close(&mut socket, CloseReason::AUTH_TIMEOUT).await;
            return;
        }
    };
    drop(pending_permit);

    let (outgoing_sender, outgoing_receiver) = mpsc::channel(OUTGOING_QUEUE_CAPACITY);
    let (close_sender, close_receiver) = oneshot::channel();
    let client_id = server.next_client_id.fetch_add(1, Ordering::Relaxed);
    let handle = ClientHandle {
        outgoing: outgoing_sender,
        close: close_sender,
    };
    let Some(device_name) = server.register_client(&token, client_id, handle) else {
        send_close(&mut socket, CloseReason::UNAUTHORIZED).await;
        return;
    };

    let auth_ok = serde_json::json!({ "type": "auth_ok", "client_id": client_id }).to_string();
    if !send_with_timeout(&mut socket, Message::Text(auth_ok.into())).await {
        server.clients.lock().remove(&client_id);
        return;
    }

    log::info!("client {client_id} connected ({device_name})");
    server
        .send_to_host(ServerMessage::ClientConnected {
            client_id,
            device_name,
        })
        .await;

    relay(
        &server,
        client_id,
        &mut socket,
        outgoing_receiver,
        close_receiver,
    )
    .await;

    server.clients.lock().remove(&client_id);
    log::info!("client {client_id} disconnected");
    server
        .send_to_host(ServerMessage::ClientDisconnected { client_id })
        .await;
}

async fn read_auth_token(socket: &mut WebSocket) -> Result<String, AuthFailure> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                return match serde_json::from_str::<ClientAuthMessage>(text.as_str()) {
                    Ok(ClientAuthMessage::Auth { token }) => Ok(token),
                    Err(_) => Err(AuthFailure::Close(CloseReason::EXPECTED_AUTH)),
                };
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Binary(_))) => {
                return Err(AuthFailure::Close(CloseReason::EXPECTED_AUTH));
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                return Err(AuthFailure::Disconnected);
            }
        }
    }
}

async fn relay(
    server: &Server,
    client_id: ClientId,
    socket: &mut WebSocket,
    mut outgoing_receiver: mpsc::Receiver<Utf8Bytes>,
    mut close_receiver: oneshot::Receiver<CloseReason>,
) {
    let mut ping_interval = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_received_at = Instant::now();

    loop {
        tokio::select! {
            // Closing drops the outgoing sender at the same time, so the close
            // reason must win over the outgoing queue ending.
            biased;
            reason = &mut close_receiver => {
                if let Ok(reason) = reason {
                    send_close(socket, reason).await;
                }
                return;
            }
            incoming = socket.recv() => {
                last_received_at = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        server
                            .send_to_host(ServerMessage::Received {
                                client_id,
                                message: text.as_str().to_owned(),
                            })
                            .await;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        send_close(socket, CloseReason::BINARY_UNSUPPORTED).await;
                        return;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(error)) => {
                        log::info!("client {client_id} connection error: {error}");
                        return;
                    }
                }
            }
            outgoing = outgoing_receiver.recv() => {
                let Some(text) = outgoing else {
                    return;
                };
                if !send_with_timeout(socket, Message::Text(text)).await {
                    return;
                }
            }
            _ = ping_interval.tick() => {
                if last_received_at.elapsed() > IDLE_TIMEOUT {
                    log::info!("client {client_id} stopped responding");
                    return;
                }
                if !send_with_timeout(socket, Message::Ping(Default::default())).await {
                    return;
                }
            }
        }
    }
}

async fn send_with_timeout(socket: &mut WebSocket, message: Message) -> bool {
    match tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            log::info!("failed to write to WebSocket: {error}");
            false
        }
        Err(_) => {
            log::info!("timed out writing to WebSocket");
            false
        }
    }
}

async fn send_close(socket: &mut WebSocket, reason: CloseReason) {
    let frame = CloseFrame {
        code: reason.code,
        reason: reason.reason.into(),
    };
    send_with_timeout(socket, Message::Close(Some(frame))).await;
}

impl Server {
    /// Returns the device name, or `None` if the token is not valid.
    fn register_client(
        &self,
        token: &str,
        client_id: ClientId,
        handle: ClientHandle,
    ) -> Option<String> {
        // Holding the store lock while registering means a concurrent unpair
        // either rejects this token or sees and closes this client.
        let store = self.store.lock();
        let device = store.device_for_token(token)?;
        self.clients.lock().insert(client_id, handle);
        Some(device.name.clone())
    }

    /// Queues `message` for one client, or for all clients when `client_id`
    /// is `None`. Clients whose queue is full are disconnected instead of
    /// buffering without bound.
    pub fn send_to_clients(&self, client_id: Option<ClientId>, message: String) {
        let message = Utf8Bytes::from(message);
        let mut clients = self.clients.lock();
        let recipients: Vec<ClientId> = match client_id {
            Some(client_id) if clients.contains_key(&client_id) => vec![client_id],
            Some(client_id) => {
                log::warn!("dropping message for unknown client {client_id}");
                Vec::new()
            }
            None => clients.keys().copied().collect(),
        };

        for recipient in recipients {
            let Some(handle) = clients.get(&recipient) else {
                continue;
            };
            match handle.outgoing.try_send(message.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    log::warn!("client {recipient} is not keeping up, disconnecting it");
                    if let Some(handle) = clients.remove(&recipient) {
                        handle.close(CloseReason::TOO_SLOW);
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    clients.remove(&recipient);
                }
            }
        }
    }

    pub fn disconnect_all_clients(&self, reason: CloseReason) {
        let clients = std::mem::take(&mut *self.clients.lock());
        for (_, handle) in clients {
            handle.close(reason);
        }
    }
}
