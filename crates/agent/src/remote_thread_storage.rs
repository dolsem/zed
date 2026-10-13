use crate::{DataType, DbThread, DbThreadMetadata, ThreadsDatabase, db::serialize_thread};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use client::{AnyProtoClient, proto};
use gpui::{BackgroundExecutor, EntityId, Task};
use remote::RemoteConnectionOptions;
use util::path_list::{PathList, SerializedPathList};

/// Native agent threads stored in the databases of a remote host, so that the
/// Zed running on that host sees the same threads.
#[derive(Clone)]
pub struct RemoteThreadStorage {
    host_id: EntityId,
    client: AnyProtoClient,
    connection_options: RemoteConnectionOptions,
}

impl RemoteThreadStorage {
    pub(crate) fn new(
        host_id: EntityId,
        client: AnyProtoClient,
        connection_options: RemoteConnectionOptions,
    ) -> Self {
        Self {
            host_id,
            client,
            connection_options,
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
        let request = self.client.request(proto::LoadAgentThread {
            id: id.0.to_string(),
        });
        executor.spawn(async move {
            let response = request.await?;
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
            client
                .request(proto::SaveAgentThread {
                    summary: Some(metadata_to_proto(&metadata)),
                    data_type: data_type.as_str().to_string(),
                    data,
                })
                .await?;
            Ok(metadata)
        })
    }

    pub fn delete_thread(&self, id: acp::SessionId) -> impl Future<Output = Result<()>> + use<> {
        let request = self.client.request(proto::DeleteAgentThread {
            id: id.0.to_string(),
        });
        async move {
            request.await?;
            Ok(())
        }
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
