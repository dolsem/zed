use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use mobile_protocol::{RequestId, ServerMessage};
use tokio::sync::{mpsc, oneshot};

use crate::{Server, error_response};

/// Long enough for Zed to answer anything that isn't an operation (those
/// answer right away and report progress separately).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_QUEUE_CAPACITY: usize = 1024;

pub enum PendingRequest {
    Waiting(oneshot::Sender<Reply>),
    Streaming(mpsc::Sender<Event>),
}

pub enum Reply {
    Json {
        status: u16,
        body: serde_json::Value,
    },
    Stream(mpsc::Receiver<Event>),
}

/// Tells Zed when a request ends before Zed finished answering it, e.g.
/// because the client disconnected from a stream.
struct RequestGuard {
    server: Arc<Server>,
    request_id: RequestId,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self
            .server
            .requests
            .lock()
            .remove(&self.request_id)
            .is_some()
        {
            self.server.report_request_closed(self.request_id);
        }
    }
}

pub async fn forward(
    State(server): State<Arc<Server>>,
    method: Method,
    uri: Uri,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !server.is_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let body = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(body) => Some(body),
            Err(error) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("the body isn't valid JSON: {error}"),
                );
            }
        }
    };
    let path = match uri.path().strip_prefix("/api") {
        Some("") | None => "/".to_string(),
        Some(path) => path.to_string(),
    };

    let request_id = server.next_request_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = oneshot::channel();
    server
        .requests
        .lock()
        .insert(request_id, PendingRequest::Waiting(sender));
    let guard = RequestGuard {
        server: server.clone(),
        request_id,
    };
    server
        .send_to_host(ServerMessage::Request {
            request_id,
            method: method.to_string(),
            path,
            query,
            body,
        })
        .await;

    match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(Reply::Json { status, body })) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(body)).into_response()
        }
        Ok(Ok(Reply::Stream(events))) => {
            let stream = futures::stream::unfold((events, guard), |(mut events, guard)| async {
                let event = events.recv().await?;
                Some((Ok::<_, Infallible>(event), (events, guard)))
            });
            let mut response = Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response();
            // Keeps reverse proxies (e.g. nginx) from buffering the events.
            response
                .headers_mut()
                .insert("x-accel-buffering", HeaderValue::from_static("no"));
            response
        }
        Ok(Err(_)) => error_response(StatusCode::BAD_GATEWAY, "Zed dropped the request"),
        Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "Zed didn't answer in time"),
    }
}

impl Server {
    fn report_request_closed(self: &Arc<Self>, request_id: RequestId) {
        let server = self.clone();
        tokio::spawn(async move {
            server
                .send_to_host(ServerMessage::RequestClosed { request_id })
                .await;
        });
    }

    pub fn respond(self: &Arc<Self>, request_id: RequestId, status: u16, body: serde_json::Value) {
        let mut requests = self.requests.lock();
        match requests.remove(&request_id) {
            Some(PendingRequest::Waiting(sender)) => {
                if sender.send(Reply::Json { status, body }).is_err() {
                    log::debug!("request {request_id} ended before Zed answered it");
                }
            }
            Some(streaming @ PendingRequest::Streaming(_)) => {
                log::error!("Zed answered request {request_id}, which is already streaming");
                requests.insert(request_id, streaming);
            }
            None => log::debug!("dropping the answer to closed request {request_id}"),
        }
    }

    pub fn start_stream(self: &Arc<Self>, request_id: RequestId) {
        let mut requests = self.requests.lock();
        match requests.remove(&request_id) {
            Some(PendingRequest::Waiting(sender)) => {
                let (event_sender, event_receiver) = mpsc::channel(STREAM_QUEUE_CAPACITY);
                if sender.send(Reply::Stream(event_receiver)).is_ok() {
                    requests.insert(request_id, PendingRequest::Streaming(event_sender));
                } else {
                    self.report_request_closed(request_id);
                }
            }
            Some(streaming @ PendingRequest::Streaming(_)) => {
                log::error!("Zed started streaming request {request_id} twice");
                requests.insert(request_id, streaming);
            }
            None => self.report_request_closed(request_id),
        }
    }

    pub fn send_stream_event(
        self: &Arc<Self>,
        request_id: RequestId,
        event: String,
        data: serde_json::Value,
    ) {
        // SSE can't carry line breaks in event names.
        if event.contains(['\n', '\r']) {
            log::error!("dropping stream event with an invalid name {event:?}");
            return;
        }
        let data = match serde_json::to_string(&data) {
            Ok(data) => data,
            Err(error) => {
                log::error!("failed to serialize stream event {event}: {error}");
                return;
            }
        };
        let mut requests = self.requests.lock();
        let Some(PendingRequest::Streaming(sender)) = requests.get(&request_id) else {
            log::debug!("dropping event {event} for request {request_id}, which isn't streaming");
            return;
        };
        match sender.try_send(Event::default().event(event).data(data)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!("client of request {request_id} is not keeping up, closing its stream");
                requests.remove(&request_id);
                self.report_request_closed(request_id);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                requests.remove(&request_id);
                self.report_request_closed(request_id);
            }
        }
    }

    pub fn end_stream(&self, request_id: RequestId) {
        let mut requests = self.requests.lock();
        match requests.remove(&request_id) {
            Some(PendingRequest::Streaming(_)) | None => {}
            Some(waiting @ PendingRequest::Waiting(_)) => {
                log::error!("Zed ended request {request_id}, which never started streaming");
                requests.insert(request_id, waiting);
            }
        }
    }
}
