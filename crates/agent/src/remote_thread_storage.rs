use crate::{
    DataType, DbThread, DbThreadMetadata, ThreadsDatabase,
    db::{PendingRemoteThread, serialize_thread},
};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use client::{AnyProtoClient, proto};
use futures::future::Shared;
use gpui::{BackgroundExecutor, EntityId, Task};
use remote::RemoteConnectionOptions;
use std::sync::Arc;
use util::{
    ResultExt as _,
    path_list::{PathList, SerializedPathList},
};

type DatabaseFuture = Shared<Task<Result<Arc<ThreadsDatabase>, Arc<anyhow::Error>>>>;

/// Native agent threads stored in the databases of a remote host, so that the
/// Zed running on that host sees the same threads.
///
/// The content of a thread being saved is kept in the local database until the
/// host has it, so that it survives Zed quitting while the host is unreachable.
#[derive(Clone)]
pub struct RemoteThreadStorage {
    host_id: EntityId,
    client: AnyProtoClient,
    connection_options: RemoteConnectionOptions,
    /// The host's connection identity, as a persistence key.
    host_key: String,
    database: DatabaseFuture,
}

impl RemoteThreadStorage {
    pub(crate) fn new(
        host_id: EntityId,
        client: AnyProtoClient,
        connection_options: RemoteConnectionOptions,
        database: DatabaseFuture,
    ) -> Self {
        let host_key = remote::remote_connection_identity(&connection_options).persistence_key();
        Self {
            host_id,
            client,
            connection_options,
            host_key,
            database,
        }
    }

    /// Identifies the remote host, as the entity id of its `RemoteClient`.
    pub fn host_id(&self) -> EntityId {
        self.host_id
    }

    pub fn client(&self) -> &AnyProtoClient {
        &self.client
    }

    pub fn connection_options(&self) -> &RemoteConnectionOptions {
        &self.connection_options
    }

    pub fn list_threads(&self) -> impl Future<Output = Result<Vec<DbThreadMetadata>>> + use<> {
        let request = self.client.request(proto::ListAgentThreads {});
        async move {
            request
                .await?
                .threads
                .into_iter()
                .map(metadata_from_proto)
                .collect()
        }
    }

    pub fn load_thread(
        &self,
        id: acp::SessionId,
        executor: &BackgroundExecutor,
    ) -> Task<Result<Option<DbThread>>> {
        let client = self.client.clone();
        let database = self.database.clone();
        executor.spawn(async move {
            // A copy the host doesn't have yet is newer than the host's.
            let database = database.await.map_err(|error| anyhow!(error))?;
            if let Some(thread) = database.load_pending_remote_thread(id.clone()).await? {
                return Ok(Some(thread));
            }
            let response = client
                .request(proto::LoadAgentThread {
                    id: id.0.to_string(),
                })
                .await?;
            let (Some(data_type), Some(data)) = (response.data_type, response.data) else {
                return Ok(None);
            };
            let data_type = DataType::parse(&data_type)?;
            ThreadsDatabase::deserialize_thread(data_type, data).map(Some)
        })
    }

    /// Saves the thread on the remote host, returning the metadata the host
    /// now lists for it.
    pub fn save_thread(
        &self,
        id: acp::SessionId,
        thread: DbThread,
        folder_paths: PathList,
        executor: &BackgroundExecutor,
    ) -> Task<Result<DbThreadMetadata>> {
        let client = self.client.clone();
        let database = self.database.clone();
        let host = self.host_key.clone();
        executor.spawn(async move {
            let metadata = DbThreadMetadata {
                id: id.clone(),
                parent_session_id: thread
                    .subagent_context
                    .as_ref()
                    .map(|context| context.parent_thread_id.clone()),
                title: thread.title.clone(),
                updated_at: thread.updated_at,
                created_at: None,
                folder_paths,
            };
            let (data_type, data) = serialize_thread(thread)?;
            let pending = PendingRemoteThread {
                host,
                generation: uuid::Uuid::new_v4().to_string(),
                metadata,
                data_type,
                data,
            };
            let database = database.await.map_err(|error| anyhow!(error)).log_err();
            if let Some(database) = &database {
                database
                    .save_pending_remote_thread(pending.clone())
                    .await
                    .context("keeping a local copy of a remote thread")
                    .log_err();
            }
            Self::upload(&client, database.as_deref(), pending).await
        })
    }

    /// Saves a pending thread on the remote host, then forgets the local copy.
    async fn upload(
        client: &AnyProtoClient,
        database: Option<&ThreadsDatabase>,
        pending: PendingRemoteThread,
    ) -> Result<DbThreadMetadata> {
        client
            .request(proto::SaveAgentThread {
                summary: Some(metadata_to_proto(&pending.metadata)),
                data_type: pending.data_type.as_str().to_string(),
                data: pending.data,
            })
            .await?;
        if let Some(database) = database {
            database
                .delete_pending_remote_thread(pending.metadata.id.clone(), Some(pending.generation))
                .await
                .log_err();
        }
        Ok(pending.metadata)
    }

    /// Saves the threads whose content the host doesn't have yet, e.g. because
    /// Zed quit while the host was unreachable.
    pub fn upload_pending_threads(
        &self,
        executor: &BackgroundExecutor,
    ) -> Task<Result<Vec<DbThreadMetadata>>> {
        let client = self.client.clone();
        let database = self.database.clone();
        let host = self.host_key.clone();
        executor.spawn(async move {
            let database = database.await.map_err(|error| anyhow!(error))?;
            let mut uploaded = Vec::new();
            for pending in database.pending_remote_threads(host).await? {
                uploaded.push(Self::upload(&client, Some(&database), pending).await?);
            }
            Ok(uploaded)
        })
    }

    pub fn delete_thread(
        &self,
        id: acp::SessionId,
        executor: &BackgroundExecutor,
    ) -> Task<Result<()>> {
        let client = self.client.clone();
        let database = self.database.clone();
        executor.spawn(async move {
            if let Some(database) = database.await.map_err(|error| anyhow!(error)).log_err() {
                database
                    .delete_pending_remote_thread(id.clone(), None)
                    .await
                    .log_err();
            }
            client
                .request(proto::DeleteAgentThread {
                    id: id.0.to_string(),
                })
                .await?;
            Ok(())
        })
    }
}

fn metadata_to_proto(metadata: &DbThreadMetadata) -> proto::AgentThreadSummary {
    let (folder_paths, folder_paths_order) = if metadata.folder_paths.is_empty() {
        (None, None)
    } else {
        let serialized = metadata.folder_paths.serialize();
        (Some(serialized.paths), Some(serialized.order))
    };
    proto::AgentThreadSummary {
        id: metadata.id.0.to_string(),
        parent_id: metadata
            .parent_session_id
            .as_ref()
            .map(|id| id.0.to_string()),
        folder_paths,
        folder_paths_order,
        summary: metadata.title.to_string(),
        updated_at: metadata.updated_at.to_rfc3339(),
        created_at: metadata
            .created_at
            .map(|created_at| created_at.to_rfc3339()),
    }
}

fn metadata_from_proto(summary: proto::AgentThreadSummary) -> Result<DbThreadMetadata> {
    let folder_paths = summary
        .folder_paths
        .map(|paths| {
            PathList::deserialize(&SerializedPathList {
                paths,
                order: summary.folder_paths_order.unwrap_or_default(),
            })
        })
        .unwrap_or_default();
    let created_at = summary
        .created_at
        .as_deref()
        .map(parse_timestamp)
        .transpose()?;
    Ok(DbThreadMetadata {
        id: acp::SessionId::new(summary.id),
        parent_session_id: summary.parent_id.map(acp::SessionId::new),
        title: summary.summary.into(),
        updated_at: parse_timestamp(&summary.updated_at)?,
        created_at,
        folder_paths,
    })
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid timestamp {value:?}"))?
        .with_timezone(&Utc))
}
