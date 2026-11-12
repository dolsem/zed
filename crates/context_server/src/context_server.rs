pub mod client;
pub mod listener;
pub mod oauth;
pub mod protocol;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
pub mod transport;
pub mod types;

use collections::HashMap;
use http_client::HttpClient;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Display, path::PathBuf};

use anyhow::Result;
use client::Client;
use gpui::AsyncApp;
use parking_lot::RwLock;
pub use settings::ContextServerCommand;
use url::Url;

use crate::oauth::WwwAuthenticate;
use crate::transport::HttpTransport;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextServerId(pub Arc<str>);

impl Display for ContextServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

enum ContextServerTransport {
    Stdio(ContextServerCommand, Option<PathBuf>),
    Custom(Arc<dyn crate::transport::Transport>),
    Connector(Arc<dyn Connector>),
}

/// Holds a server's connection. A [`Connector`] may replace it while the server
/// runs, such as when it has to reconnect.
pub type ClientSlot = Arc<RwLock<Option<Arc<crate::protocol::InitializedContextServerProtocol>>>>;

/// Connects a server in a way that one fixed transport can't express, like
/// joining a copy of it that another process already runs.
#[async_trait::async_trait(?Send)]
pub trait Connector: Send + Sync {
    /// Returns an initialized connection. `slot` is where the server keeps it, for
    /// connectors that later replace it.
    async fn connect(
        &self,
        slot: ClientSlot,
        cx: &AsyncApp,
    ) -> Result<Arc<crate::protocol::InitializedContextServerProtocol>>;

    /// Called when the server is stopped.
    fn disconnect(&self) {}
}

pub struct ContextServer {
    id: ContextServerId,
    client: ClientSlot,
    configuration: ContextServerTransport,
    request_timeout: Option<Duration>,
}

impl ContextServer {
    pub fn stdio(
        id: ContextServerId,
        command: ContextServerCommand,
        working_directory: Option<Arc<Path>>,
    ) -> Self {
        Self {
            id,
            client: ClientSlot::default(),
            configuration: ContextServerTransport::Stdio(
                command,
                working_directory.map(|directory| directory.to_path_buf()),
            ),
            request_timeout: None,
        }
    }

    pub fn http(
        id: ContextServerId,
        endpoint: &Url,
        headers: HashMap<String, String>,
        http_client: Arc<dyn HttpClient>,
        executor: gpui::BackgroundExecutor,
        request_timeout: Option<Duration>,
    ) -> Result<Self> {
        let transport = match endpoint.scheme() {
            "http" | "https" => {
                log::info!("Using HTTP transport for {}", endpoint);
                let transport =
                    HttpTransport::new(http_client, endpoint.to_string(), headers, executor);
                Arc::new(transport) as _
            }
            _ => anyhow::bail!("unsupported MCP url scheme {}", endpoint.scheme()),
        };
        Ok(Self::new_with_timeout(id, transport, request_timeout))
    }

    pub fn new(id: ContextServerId, transport: Arc<dyn crate::transport::Transport>) -> Self {
        Self::new_with_timeout(id, transport, None)
    }

    pub fn new_with_timeout(
        id: ContextServerId,
        transport: Arc<dyn crate::transport::Transport>,
        request_timeout: Option<Duration>,
    ) -> Self {
        Self {
            id,
            client: ClientSlot::default(),
            configuration: ContextServerTransport::Custom(transport),
            request_timeout,
        }
    }

    pub fn with_connector(id: ContextServerId, connector: Arc<dyn Connector>) -> Self {
        Self {
            id,
            client: ClientSlot::default(),
            configuration: ContextServerTransport::Connector(connector),
            request_timeout: None,
        }
    }

    pub fn id(&self) -> ContextServerId {
        self.id.clone()
    }

    pub fn client(&self) -> Option<Arc<crate::protocol::InitializedContextServerProtocol>> {
        self.client.read().clone()
    }

    /// The authentication challenge from the last `401 Unauthorized` response
    /// this server's transport gave up on, if any. See
    /// [`crate::transport::Transport::auth_challenge`].
    pub fn auth_challenge(&self) -> Option<WwwAuthenticate> {
        match &self.configuration {
            ContextServerTransport::Stdio(..) | ContextServerTransport::Connector(..) => None,
            ContextServerTransport::Custom(transport) => transport.auth_challenge(),
        }
    }

    pub async fn start(&self, cx: &AsyncApp) -> Result<()> {
        if let ContextServerTransport::Connector(connector) = &self.configuration {
            let client = connector.connect(self.client.clone(), cx).await?;
            *self.client.write() = Some(client);
            return Ok(());
        }
        self.initialize(self.new_client(cx)?).await
    }

    fn new_client(&self, cx: &AsyncApp) -> Result<Client> {
        Ok(match &self.configuration {
            ContextServerTransport::Connector(_) => {
                anyhow::bail!("a connector makes its own client")
            }
            ContextServerTransport::Stdio(command, working_directory) => Client::stdio(
                client::ContextServerId(self.id.0.clone()),
                client::ModelContextServerBinary {
                    executable: Path::new(&command.path).to_path_buf(),
                    args: command.args.clone(),
                    env: command.env.clone(),
                    timeout: command.timeout,
                },
                working_directory,
                cx.clone(),
            )?,
            ContextServerTransport::Custom(transport) => Client::new(
                client::ContextServerId(self.id.0.clone()),
                self.id().0,
                transport.clone(),
                self.request_timeout,
                cx.clone(),
            )?,
        })
    }

    async fn initialize(&self, client: Client) -> Result<()> {
        log::debug!("starting context server {}", self.id);
        let protocol = crate::protocol::ModelContextProtocol::new(client);
        let client_info = types::Implementation {
            name: "Zed".to_string(),
            title: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
        };
        let initialized_protocol = protocol.initialize(client_info).await?;

        log::debug!(
            "context server {} initialized: {:?}",
            self.id,
            initialized_protocol.initialize,
        );

        *self.client.write() = Some(Arc::new(initialized_protocol));
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut client = self.client.write();
        if let Some(protocol) = client.take() {
            drop(protocol);
        }
        drop(client);
        if let ContextServerTransport::Connector(connector) = &self.configuration {
            connector.disconnect();
        }
        Ok(())
    }
}
