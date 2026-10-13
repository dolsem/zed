//! Serves the host's agent thread databases to the connected client.
//!
//! Threads created from a remote project are stored in the same databases the
//! Zed running on this host uses, so both see the same threads. This module
//! works on raw rows: thread contents are passed through as opaque blobs and
//! the client is responsible for (de)serializing them.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{AppContext as _, AsyncApp, Context, Entity, Task};
use release_channel::RELEASE_CHANNEL;
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use sqlez::{bindable::Column, connection::Connection, statement::Statement};
use util::ResultExt as _;

const CHANGES_WATCH_LATENCY: Duration = Duration::from_millis(300);

pub struct AgentThreadsHost {
    threads_db: Arc<Mutex<Option<Connection>>>,
    metadata_db: Arc<Mutex<Option<Connection>>>,
    /// Set under a fake file system (in tests), where the host's real
    /// databases must not be touched.
    in_memory: bool,
    _watch_changes: Task<()>,
}

impl AgentThreadsHost {
    pub fn new(session: AnyProtoClient, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        session.add_request_handler(cx.weak_entity(), Self::handle_list_threads);
        session.add_request_handler(cx.weak_entity(), Self::handle_load_thread);
        session.add_request_handler(cx.weak_entity(), Self::handle_save_thread);
        session.add_request_handler(cx.weak_entity(), Self::handle_delete_thread);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_metadata);
        session.add_request_handler(cx.weak_entity(), Self::handle_save_metadata);
        session.add_request_handler(cx.weak_entity(), Self::handle_delete_metadata);

        let in_memory = fs.is_fake();
        let watch_changes = cx.background_spawn(async move {
            if in_memory {
                return;
            }
            let changes_dir = paths::agent_threads_changes_dir();
            if let Err(error) = std::fs::create_dir_all(changes_dir) {
                log::warn!("failed to create agent threads changes directory: {error}");
                return;
            }
            let own_marker = changes_dir.join(std::process::id().to_string());
            let (mut events, _watcher) = fs.watch(changes_dir, CHANGES_WATCH_LATENCY).await;
            while let Some(events) = events.next().await {
                if events.iter().any(|event| event.path != own_marker) {
                    session.send(proto::AgentThreadsChanged {}).log_err();
                }
            }
        });

        Self {
            threads_db: Arc::new(Mutex::new(None)),
            metadata_db: Arc::new(Mutex::new(None)),
            in_memory,
            _watch_changes: watch_changes,
        }
    }

    /// Lets other processes on the host (i.e. Zed) know the databases changed.
    fn announce_write(this: &Entity<Self>, cx: &AsyncApp) {
        if this.read_with(cx, |this, _| this.in_memory) {
            return;
        }
        if let Err(error) = paths::touch_agent_threads_changed_marker() {
            log::warn!("failed to touch agent threads change marker: {error}");
        }
    }

    fn with_threads_db<R: Send + 'static>(
        this: &Entity<Self>,
        cx: &AsyncApp,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Task<Result<R>> {
        let (db, in_memory) =
            this.read_with(cx, |this, _| (this.threads_db.clone(), this.in_memory));
        cx.background_spawn(async move {
            let mut db = db
                .lock()
                .map_err(|_| anyhow!("threads database lock poisoned"))?;
            if db.is_none() {
                *db = Some(open_threads_db(in_memory)?);
            }
            f(db.as_ref().context("threads database not open")?)
        })
    }

    fn with_metadata_db<R: Send + 'static>(
        this: &Entity<Self>,
        cx: &AsyncApp,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Task<Result<R>> {
        let (db, in_memory) =
            this.read_with(cx, |this, _| (this.metadata_db.clone(), this.in_memory));
        cx.background_spawn(async move {
            let mut db = db
                .lock()
                .map_err(|_| anyhow!("thread metadata database lock poisoned"))?;
            if db.is_none() {
                *db = Some(open_metadata_db(in_memory)?);
            }
            f(db.as_ref().context("thread metadata database not open")?)
        })
    }

    async fn handle_list_threads(
        this: Entity<Self>,
        _: TypedEnvelope<proto::ListAgentThreads>,
        cx: AsyncApp,
    ) -> Result<proto::ListAgentThreadsResponse> {
        let threads = Self::with_threads_db(&this, &cx, |db| {
            let rows = db.select::<ThreadSummaryRow>(
                "SELECT id, parent_id, folder_paths, folder_paths_order, summary, updated_at, created_at \
                 FROM threads ORDER BY updated_at DESC, created_at DESC",
            )?()?;
            Ok(rows.into_iter().map(|row| row.0).collect())
        })
        .await?;
        Ok(proto::ListAgentThreadsResponse { threads })
    }

    async fn handle_load_thread(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::LoadAgentThread>,
        cx: AsyncApp,
    ) -> Result<proto::LoadAgentThreadResponse> {
        let id = envelope.payload.id;
        let row = Self::with_threads_db(&this, &cx, move |db| {
            let row = db.select_row_bound::<String, (String, Vec<u8>)>(
                "SELECT data_type, data FROM threads WHERE id = ? LIMIT 1",
            )?(id)?;
            Ok(row)
        })
        .await?;
        let (data_type, data) = row.unzip();
        Ok(proto::LoadAgentThreadResponse { data_type, data })
    }

    async fn handle_save_thread(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SaveAgentThread>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let proto::SaveAgentThread {
            summary,
            data_type,
            data,
        } = envelope.payload;
        let summary = summary.context("missing thread summary")?;
        Self::with_threads_db(&this, &cx, move |db| {
            let mut statement = Statement::prepare(
                db,
                "INSERT INTO threads (id, parent_id, folder_paths, folder_paths_order, summary, updated_at, data_type, data, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(id) DO UPDATE SET \
                     parent_id = excluded.parent_id, \
                     folder_paths = excluded.folder_paths, \
                     folder_paths_order = excluded.folder_paths_order, \
                     summary = excluded.summary, \
                     updated_at = excluded.updated_at, \
                     data_type = excluded.data_type, \
                     data = excluded.data",
            )?;
            let created_at = summary
                .created_at
                .clone()
                .unwrap_or_else(|| summary.updated_at.clone());
            let mut i = statement.bind(&summary.id, 1)?;
            i = statement.bind(&summary.parent_id, i)?;
            i = statement.bind(&summary.folder_paths, i)?;
            i = statement.bind(&summary.folder_paths_order, i)?;
            i = statement.bind(&summary.summary, i)?;
            i = statement.bind(&summary.updated_at, i)?;
            i = statement.bind(&data_type, i)?;
            i = statement.bind(&data, i)?;
            statement.bind(&created_at, i)?;
            statement.exec()?;
            Ok(())
        })
        .await?;
        Self::announce_write(&this, &cx);
        Ok(proto::Ack {})
    }

    async fn handle_delete_thread(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::DeleteAgentThread>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let id = envelope.payload.id;
        Self::with_threads_db(&this, &cx, move |db| {
            let mut select_children =
                db.select_bound::<String, String>("SELECT id FROM threads WHERE parent_id = ?")?;
            // Delete the thread together with all of its transitive subagent threads.
            let mut ids_to_delete = vec![id.clone()];
            let mut frontier = vec![id];
            while let Some(parent) = frontier.pop() {
                for child in select_children(parent)? {
                    ids_to_delete.push(child.clone());
                    frontier.push(child);
                }
            }
            let mut delete = db.exec_bound::<String>("DELETE FROM threads WHERE id = ?")?;
            for id in ids_to_delete {
                delete(id)?;
            }
            Ok(())
        })
        .await?;
        Self::announce_write(&this, &cx);
        Ok(proto::Ack {})
    }

    async fn handle_list_metadata(
        this: Entity<Self>,
        _: TypedEnvelope<proto::ListAgentThreadMetadata>,
        cx: AsyncApp,
    ) -> Result<proto::ListAgentThreadMetadataResponse> {
        let threads = Self::with_metadata_db(&this, &cx, |db| {
            // Threads the host itself opened over a remote connection belong to
            // other machines, so only the host's local threads are shared.
            let rows = db.select::<ThreadMetadataRow>(
                "SELECT thread_id, session_id, agent_id, title, updated_at, created_at, interacted_at, \
                 folder_paths, folder_paths_order, archived, main_worktree_paths, \
                 main_worktree_paths_order, title_override \
                 FROM sidebar_threads \
                 WHERE remote_connection IS NULL AND thread_id IS NOT NULL \
                 ORDER BY updated_at DESC",
            )?()?;
            Ok(rows.into_iter().map(|row| row.0).collect())
        })
        .await?;
        Ok(proto::ListAgentThreadMetadataResponse { threads })
    }

    async fn handle_save_metadata(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SaveAgentThreadMetadata>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let row = envelope
            .payload
            .metadata
            .context("missing thread metadata")?;
        anyhow::ensure!(row.thread_id.len() == 16, "invalid thread id");
        Self::with_metadata_db(&this, &cx, move |db| {
            let mut statement = Statement::prepare(
                db,
                "INSERT INTO sidebar_threads(thread_id, session_id, agent_id, title, updated_at, created_at, interacted_at, folder_paths, folder_paths_order, archived, main_worktree_paths, main_worktree_paths_order, remote_connection, title_override) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13) \
                 ON CONFLICT(thread_id) DO UPDATE SET \
                     session_id = excluded.session_id, \
                     agent_id = excluded.agent_id, \
                     title = excluded.title, \
                     updated_at = excluded.updated_at, \
                     created_at = excluded.created_at, \
                     interacted_at = excluded.interacted_at, \
                     folder_paths = excluded.folder_paths, \
                     folder_paths_order = excluded.folder_paths_order, \
                     archived = excluded.archived, \
                     main_worktree_paths = excluded.main_worktree_paths, \
                     main_worktree_paths_order = excluded.main_worktree_paths_order, \
                     remote_connection = NULL, \
                     title_override = excluded.title_override",
            )?;
            let mut i = statement.bind(&row.thread_id.as_slice(), 1)?;
            i = statement.bind(&row.session_id, i)?;
            i = statement.bind(&row.agent_id, i)?;
            i = statement.bind(&row.title, i)?;
            i = statement.bind(&row.updated_at, i)?;
            i = statement.bind(&row.created_at, i)?;
            i = statement.bind(&row.interacted_at, i)?;
            i = statement.bind(&row.folder_paths, i)?;
            i = statement.bind(&row.folder_paths_order, i)?;
            i = statement.bind(&row.archived, i)?;
            i = statement.bind(&row.main_worktree_paths, i)?;
            i = statement.bind(&row.main_worktree_paths_order, i)?;
            statement.bind(&row.title_override, i)?;
            statement.exec()?;
            Ok(())
        })
        .await?;
        Self::announce_write(&this, &cx);
        Ok(proto::Ack {})
    }

    async fn handle_delete_metadata(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::DeleteAgentThreadMetadata>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let thread_id = envelope.payload.thread_id;
        anyhow::ensure!(thread_id.len() == 16, "invalid thread id");
        Self::with_metadata_db(&this, &cx, move |db| {
            let mut statement =
                Statement::prepare(db, "DELETE FROM sidebar_threads WHERE thread_id = ?")?;
            statement.bind(&thread_id.as_slice(), 1)?;
            statement.exec()?;
            Ok(())
        })
        .await?;
        Self::announce_write(&this, &cx);
        Ok(proto::Ack {})
    }
}

fn open_existing(path: &PathBuf) -> Result<Connection> {
    let connection = Connection::open_file(&path.to_string_lossy());
    anyhow::ensure!(
        connection.persistent(),
        "failed to open database at {}",
        path.display()
    );
    Ok(connection)
}

/// Opens the host's native agent threads database, creating it the same way
/// Zed does if it doesn't exist yet.
fn open_threads_db(in_memory: bool) -> Result<Connection> {
    let connection = if in_memory {
        Connection::open_memory(Some(&format!("agent_threads_{}", uuid::Uuid::new_v4())))
    } else {
        let threads_dir = paths::data_dir().join("threads");
        std::fs::create_dir_all(&threads_dir)?;
        open_existing(&threads_dir.join("threads.db"))?
    };

    connection.exec(
        "CREATE TABLE IF NOT EXISTS threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            data_type TEXT NOT NULL,
            data BLOB NOT NULL
        )",
    )?()?;
    // These columns were added after the table was first introduced. Adding a
    // column that already exists fails, which is fine.
    for column in [
        "parent_id TEXT",
        "folder_paths TEXT",
        "folder_paths_order TEXT",
        "created_at TEXT",
    ] {
        if let Ok(mut add_column) =
            connection.exec(&format!("ALTER TABLE threads ADD COLUMN {column}"))
        {
            add_column().ok();
        }
    }

    Ok(connection)
}

/// Opens the database the host's Zed keeps its sidebar thread metadata in.
///
/// That database belongs to Zed, which owns its schema and migrations, so it
/// is never created or migrated here.
fn open_metadata_db(in_memory: bool) -> Result<Connection> {
    anyhow::ensure!(
        !in_memory,
        "thread metadata is not available under a fake file system"
    );
    let path = paths::database_dir()
        .join(format!("0-{}", RELEASE_CHANNEL.dev_name()))
        .join("db.sqlite");
    anyhow::ensure!(
        path.exists(),
        "Zed ({}) has not been run on this host yet",
        RELEASE_CHANNEL.display_name()
    );
    let connection = open_existing(&path)?;
    let has_table = connection.select_row::<String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'sidebar_threads'",
    )?()?
    .is_some();
    anyhow::ensure!(
        has_table,
        "the Zed database on this host has no thread metadata yet"
    );
    Ok(connection)
}

struct ThreadSummaryRow(proto::AgentThreadSummary);

impl Column for ThreadSummaryRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next) = Column::column(statement, start_index)?;
        let (parent_id, next) = Column::column(statement, next)?;
        let (folder_paths, next) = Column::column(statement, next)?;
        let (folder_paths_order, next) = Column::column(statement, next)?;
        let (summary, next) = Column::column(statement, next)?;
        let (updated_at, next) = Column::column(statement, next)?;
        let (created_at, next) = Column::column(statement, next)?;
        Ok((
            Self(proto::AgentThreadSummary {
                id,
                parent_id,
                folder_paths,
                folder_paths_order,
                summary,
                updated_at,
                created_at,
            }),
            next,
        ))
    }
}

struct ThreadMetadataRow(proto::AgentThreadMetadata);

impl Column for ThreadMetadataRow {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (thread_id, next) = Column::column(statement, start_index)?;
        let (session_id, next) = Column::column(statement, next)?;
        let (agent_id, next) = Column::column(statement, next)?;
        let (title, next) = Column::column(statement, next)?;
        let (updated_at, next) = Column::column(statement, next)?;
        let (created_at, next) = Column::column(statement, next)?;
        let (interacted_at, next) = Column::column(statement, next)?;
        let (folder_paths, next) = Column::column(statement, next)?;
        let (folder_paths_order, next) = Column::column(statement, next)?;
        let (archived, next) = Column::column(statement, next)?;
        let (main_worktree_paths, next) = Column::column(statement, next)?;
        let (main_worktree_paths_order, next) = Column::column(statement, next)?;
        let (title_override, next) = Column::column(statement, next)?;
        Ok((
            Self(proto::AgentThreadMetadata {
                thread_id,
                session_id,
                agent_id,
                title,
                updated_at,
                created_at,
                interacted_at,
                folder_paths,
                folder_paths_order,
                archived,
                main_worktree_paths,
                main_worktree_paths_order,
                title_override,
            }),
            next,
        ))
    }
}
