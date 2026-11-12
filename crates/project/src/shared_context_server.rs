//! Shares one copy of a stdio context server between every Zed process on a
//! machine: the app and each remote server process.
//!
//! The processes race for a lock file per server. The winner runs the server and
//! serves it through its [proxy](crate::context_server_proxy), and writes where to
//! find it next to the lock. The others connect to that address. The OS drops the
//! lock when its holder exits, so whoever notices first takes over.
//!
//! A remote project reaches the copy on its host through the remote server, with
//! [`remote_forwarder`].

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use context_server::{
    ClientSlot, Connector, ContextServer, ContextServerCommand, ContextServerId,
    protocol::InitializedContextServerProtocol, transport::Transport,
};
use futures::{FutureExt as _, Stream, future::BoxFuture};
use gpui::{AsyncApp, BackgroundExecutor};
use parking_lot::Mutex;
use rpc::{AnyProtoClient, proto};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::context_server_proxy;

/// How long to keep trying to join a copy that another process runs, which
/// covers that process still starting it.
const JOIN_WAIT: Duration = Duration::from_secs(60);
const JOIN_RETRY: Duration = Duration::from_millis(250);
/// How often a process using another's copy checks that the other is still there.
const HOST_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Sends one JSON-RPC message somewhere and resolves to the response, if the
/// message gets one.
pub(crate) type Forward =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<String>>> + Send + Sync>;

/// A transport that sends each request on its own and waits for its response, like
/// HTTP does. Requests run concurrently; the client matches responses up by id.
/// Nothing comes back but responses, so notifications from the server don't reach
/// the client.
pub(crate) struct ForwardingTransport {
    forward: Forward,
    executor: BackgroundExecutor,
    responses_tx: async_channel::Sender<String>,
    responses_rx: async_channel::Receiver<String>,
}

impl ForwardingTransport {
    pub(crate) fn new(forward: Forward, executor: BackgroundExecutor) -> Self {
        let (responses_tx, responses_rx) = async_channel::unbounded();
        Self {
            forward,
            executor,
            responses_tx,
            responses_rx,
        }
    }
}

#[async_trait]
impl Transport for ForwardingTransport {
    async fn send(&self, message: String) -> Result<()> {
        let request_id = serde_json::from_str::<Value>(&message)
            .ok()
            .filter(|message| message.get("method").is_some())
            .and_then(|message| message.get("id").cloned());
        // Notifications are for the shared connection, which made its own
        // handshake, and the server sends no requests to answer.
        let Some(request_id) = request_id else {
            return Ok(());
        };
        let response = (self.forward)(message);
        let responses_tx = self.responses_tx.clone();
        self.executor
            .spawn(async move {
                let response = match response.await {
                    Ok(Some(response)) => response,
                    Ok(None) => return,
                    Err(error) => context_server_proxy::error_response(
                        request_id,
                        -32603,
                        &format!("{error:#}"),
                    )
                    .to_string(),
                };
                responses_tx.send(response).await.ok();
            })
            .detach();
        Ok(())
    }

    fn receive(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.responses_rx.clone())
    }

    fn receive_err(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(futures::stream::empty())
    }
}

/// Sends messages through the remote server, to the copy of `server_id` it shares.
pub(crate) fn remote_forwarder(
    client: AnyProtoClient,
    project_id: u64,
    server_id: ContextServerId,
) -> Forward {
    Arc::new(move |message| {
        let response = client.request(proto::ContextServerRequest {
            project_id,
            server_id: server_id.0.to_string(),
            message,
        });
        async move {
            let response = response.await?;
            Ok((!response.message.is_empty()).then_some(response.message))
        }
        .boxed()
    })
}

/// Posts messages to another Zed process's proxy. It's on this machine, so plain
/// HTTP over a socket does, and stays clear of any HTTP proxy the user set. Each
/// request blocks a thread of its own until it is answered.
fn local_http_forwarder(url: String, headers: Vec<(String, String)>) -> Forward {
    let headers = Arc::new(headers);
    Arc::new(move |message| {
        let url = url.clone();
        let headers = headers.clone();
        let (response_tx, response_rx) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("context-server-request".into())
            .spawn(move || {
                response_tx
                    .send(post_locally(&url, &headers, &message))
                    .ok();
            });
        async move {
            spawned?;
            response_rx
                .await
                .context("context server request thread exited")?
        }
        .boxed()
    })
}

fn post_locally(url: &str, headers: &[(String, String)], body: &str) -> Result<Option<String>> {
    use std::io::{Read as _, Write as _};

    let url = url::Url::parse(url)?;
    let host = url.host_str().context("proxy URL has no host")?;
    let port = url.port().context("proxy URL has no port")?;
    let mut stream = std::net::TcpStream::connect((host, port))?;
    // Longer than any request timeout, so that a hung process can't keep the
    // thread forever.
    stream.set_read_timeout(Some(Duration::from_secs(15 * 60)))?;

    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        url.path(),
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes())?;

    let mut response = Vec::new();
    let mut buffer = [0; 8192];
    let (head_len, content_len) = loop {
        let read = stream.read(&mut buffer)?;
        anyhow::ensure!(read > 0, "connection closed before the response headers");
        response.extend_from_slice(&buffer[..read]);
        if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = std::str::from_utf8(&response[..end])?;
            let content_len = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok());
            break (end + 4, content_len);
        }
    };
    let head = std::str::from_utf8(&response[..head_len])?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .context("malformed HTTP response")?;
    match content_len {
        Some(content_len) => {
            while response.len() < head_len + content_len {
                let read = stream.read(&mut buffer)?;
                anyhow::ensure!(read > 0, "connection closed before the response body");
                response.extend_from_slice(&buffer[..read]);
            }
            response.truncate(head_len + content_len);
        }
        None => {
            stream.read_to_end(&mut response)?;
        }
    }
    let body = String::from_utf8(response.split_off(head_len))?;
    match status {
        200 => Ok(Some(body)),
        202 => Ok(None),
        _ => anyhow::bail!("the Zed process running this server answered HTTP {status}"),
    }
}

/// Where the process running a server can be reached.
#[derive(Serialize, Deserialize)]
struct HostAddress {
    url: String,
    headers: Vec<(String, String)>,
}

/// Whether this app shares servers with the other Zed processes on the machine.
/// Tests run many apps in one process; they mustn't find each other's servers, nor
/// touch the user's directories.
pub(crate) fn shares_across_processes(cx: &gpui::App) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if cx.background_executor().dispatcher().as_test().is_some() {
        return false;
    }
    let _ = cx;
    true
}

/// Runs a stdio server once per machine. See the module docs.
pub struct SharedStdioConnector(Arc<SharedStdioServer>);

type MakeServer = Box<dyn Fn() -> ContextServer + Send + Sync>;

struct SharedStdioServer {
    id: ContextServerId,
    /// Makes the copy this process runs when it is the one to.
    make_server: MakeServer,
    request_timeout: Option<Duration>,
    lock_path: PathBuf,
    address_path: PathBuf,
    /// Held while this process runs the server.
    lease: Mutex<Option<File>>,
}

impl SharedStdioConnector {
    pub(crate) fn new(
        id: ContextServerId,
        command: ContextServerCommand,
        working_directory: Option<Arc<Path>>,
    ) -> Self {
        let key = sharing_key(&id, &command);
        let request_timeout = command.timeout.map(Duration::from_secs);
        let server_id = id.clone();
        Self::new_internal(
            id,
            &key,
            paths::shared_context_servers_dir(),
            request_timeout,
            Box::new(move || {
                ContextServer::stdio(
                    server_id.clone(),
                    command.clone(),
                    working_directory.clone(),
                )
            }),
        )
    }

    /// A connector that coordinates through `dir` and runs `make_server`'s server
    /// when it is the one to.
    #[cfg(feature = "test-support")]
    pub fn for_test(
        id: ContextServerId,
        dir: &Path,
        make_server: impl Fn() -> ContextServer + Send + Sync + 'static,
    ) -> Self {
        let key = id.0.to_string();
        Self::new_internal(id, &key, dir, None, Box::new(make_server))
    }

    fn new_internal(
        id: ContextServerId,
        key: &str,
        dir: &Path,
        request_timeout: Option<Duration>,
        make_server: MakeServer,
    ) -> Self {
        Self(Arc::new(SharedStdioServer {
            id,
            make_server,
            request_timeout,
            lock_path: dir.join(format!("{key}.lock")),
            address_path: dir.join(format!("{key}.json")),
            lease: Mutex::new(None),
        }))
    }
}

/// Processes share a server only when they'd run the same command, under the same
/// id. The timeout is only how long this process waits, so it doesn't count.
fn sharing_key(id: &ContextServerId, command: &ContextServerCommand) -> String {
    let env: Option<BTreeMap<&String, &String>> =
        command.env.as_ref().map(|env| env.iter().collect());
    let identity = serde_json::json!({
        "id": id.0,
        "path": command.path,
        "args": command.args,
        "env": env,
    });
    let digest = Sha256::digest(identity.to_string().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[async_trait(?Send)]
impl Connector for SharedStdioConnector {
    async fn connect(
        &self,
        slot: ClientSlot,
        cx: &AsyncApp,
    ) -> Result<Arc<InitializedContextServerProtocol>> {
        let (client, hosting) = self.0.connect(cx).await?;
        if !hosting {
            watch_host(Arc::downgrade(&self.0), slot, client.clone(), cx);
        }
        Ok(client)
    }

    fn disconnect(&self) {
        self.0.lease.lock().take();
    }
}

impl SharedStdioServer {
    /// Runs the server here unless another process does, and joins that process's
    /// copy otherwise. Returns whether this process runs it.
    async fn connect(
        &self,
        cx: &AsyncApp,
    ) -> Result<(Arc<InitializedContextServerProtocol>, bool)> {
        let Some((base_url, token)) = cx.update(|cx| context_server_proxy::address(cx)) else {
            // Without the proxy, no other process could reach this copy.
            return Ok((self.start_here(cx).await?, true));
        };
        let deadline = Instant::now() + JOIN_WAIT;
        loop {
            match self.try_lease() {
                Ok(Some(lease)) => {
                    let endpoint = context_server_proxy::endpoint_at(&base_url, &token, &self.id);
                    let address = HostAddress {
                        url: endpoint.url,
                        headers: endpoint.headers,
                    };
                    if let Err(error) = serde_json::to_string(&address)
                        .map_err(anyhow::Error::from)
                        .and_then(|address| Ok(std::fs::write(&self.address_path, address)?))
                    {
                        log::error!(
                            "failed to publish context server {}, other Zed processes will wait for it: {error:#}",
                            self.id
                        );
                    }
                    *self.lease.lock() = Some(lease);
                    return match self.start_here(cx).await {
                        Ok(client) => Ok((client, true)),
                        Err(error) => {
                            self.lease.lock().take();
                            Err(error)
                        }
                    };
                }
                Ok(None) => match self.join(cx).await {
                    Ok(client) => {
                        log::info!(
                            "context server {} joined the copy another Zed process runs",
                            self.id
                        );
                        return Ok((client, false));
                    }
                    Err(error) if Instant::now() >= deadline => {
                        return Err(error.context(format!(
                            "failed to join the copy of {} another Zed process runs",
                            self.id
                        )));
                    }
                    Err(error) => {
                        log::debug!("context server {} not joinable yet: {error:#}", self.id);
                    }
                },
                Err(error) => {
                    log::warn!(
                        "can't coordinate context server {} with other Zed processes, running a copy of its own: {error:#}",
                        self.id
                    );
                    return Ok((self.start_here(cx).await?, true));
                }
            }
            cx.background_executor().timer(JOIN_RETRY).await;
        }
    }

    /// The lease, if no other process holds it.
    fn try_lease(&self) -> Result<Option<File>> {
        if let Some(dir) = self.lock_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    async fn start_here(&self, cx: &AsyncApp) -> Result<Arc<InitializedContextServerProtocol>> {
        let server = (self.make_server)();
        server.start(cx).await?;
        server
            .client()
            .context("context server stopped while starting")
    }

    async fn join(&self, cx: &AsyncApp) -> Result<Arc<InitializedContextServerProtocol>> {
        let address: HostAddress =
            serde_json::from_str(&std::fs::read_to_string(&self.address_path)?)?;
        let transport = ForwardingTransport::new(
            local_http_forwarder(address.url, address.headers),
            cx.background_executor().clone(),
        );
        let server = ContextServer::new_with_timeout(
            self.id.clone(),
            Arc::new(transport),
            self.request_timeout,
        );
        server.start(cx).await?;
        server
            .client()
            .context("context server stopped while starting")
    }
}

/// Reconnects when the process running the server goes away: the lease frees up
/// once it exits.
fn watch_host(
    server: Weak<SharedStdioServer>,
    slot: ClientSlot,
    mut current: Arc<InitializedContextServerProtocol>,
    cx: &AsyncApp,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(HOST_CHECK_INTERVAL).await;
            let is_current =
                |slot: &ClientSlot, current: &Arc<InitializedContextServerProtocol>| {
                    slot.read()
                        .as_ref()
                        .is_some_and(|client| Arc::ptr_eq(client, current))
                };
            // Stopped or restarted since.
            if !is_current(&slot, &current) {
                return;
            }
            let Some(server) = server.upgrade() else {
                return;
            };
            match server.try_lease() {
                Ok(Some(lease)) => drop(lease),
                _ => continue,
            }
            log::info!(
                "the Zed process running context server {} is gone, reconnecting",
                server.id
            );
            match server.connect(cx).await {
                Ok((client, hosting)) => {
                    if !is_current(&slot, &current) {
                        if hosting {
                            server.lease.lock().take();
                        }
                        return;
                    }
                    *slot.write() = Some(client.clone());
                    if hosting {
                        return;
                    }
                    current = client;
                }
                Err(error) => {
                    log::error!(
                        "failed to reconnect context server {}: {error:#}",
                        server.id
                    );
                }
            }
        }
    })
    .detach();
}
