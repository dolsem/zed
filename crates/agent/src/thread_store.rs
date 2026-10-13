use crate::{DbThread, DbThreadMetadata, RemoteThreadStorage, ThreadsDatabase};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, anyhow};
use client::proto;
use collections::{HashMap, HashSet};
use fs::Fs;
use futures::{FutureExt, StreamExt as _, future::Shared};
use gpui::{App, Context, Entity, EntityId, EventEmitter, Global, Subscription, Task, prelude::*};
use project::Project;
use remote::{RemoteClient, RemoteClientEvent};
use std::{sync::Arc, time::Duration};
use util::path_list::PathList;

const CHANGES_WATCH_LATENCY: Duration = Duration::from_millis(300);

struct GlobalThreadStore(Entity<ThreadStore>);

impl Global for GlobalThreadStore {}

pub struct ThreadStore {
    threads: Vec<DbThreadMetadata>,
    reload_task: Shared<Task<()>>,
    /// Remote hosts whose threads are stored in the host's own databases,
    /// keyed by the entity id of their `RemoteClient`.
    remote_hosts: HashMap<EntityId, RemoteThreadHost>,
    _watch_local_changes: Option<Task<()>>,
}

struct RemoteThreadHost {
    storage: RemoteThreadStorage,
    threads: Vec<DbThreadMetadata>,
    reload_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug)]
pub enum ThreadStoreEvent {
    /// The threads stored on a remote host may have changed: the host was
    /// added or reconnected, or a process on the host changed its threads.
    RemoteHostChanged(EntityId),
    RemoteHostRemoved(EntityId),
    /// Another process on this machine (e.g. a remote server serving a client
    /// connected to this machine) changed the local thread databases.
    LocalThreadsChangedExternally,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

impl ThreadStore {
    pub fn init_global(cx: &mut App) {
        let thread_store = cx.new(|cx| Self::new(cx));
        cx.set_global(GlobalThreadStore(thread_store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalThreadStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalThreadStore>().map(|g| g.0.clone())
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let reload_task = Self::spawn_reload(cx);
        Self {
            threads: Vec::new(),
            reload_task,
            remote_hosts: HashMap::default(),
            _watch_local_changes: None,
        }
    }

    /// Keeps the store in sync with threads stored elsewhere: threads of
    /// remote projects live on their remote host, and other processes on this
    /// machine may write to the local databases.
    pub fn start_syncing(this: &Entity<Self>, fs: Arc<dyn Fs>, cx: &mut App) {
        let weak_store = this.downgrade();
        cx.observe_new(move |project: &mut Project, _window, cx| {
            let Some(remote_client) = project.remote_client() else {
                return;
            };
            if project.is_via_collab() {
                return;
            }
            weak_store
                .update(cx, |store, cx| {
                    store.register_remote_host(&remote_client, cx)
                })
                .ok();
        })
        .detach();

        this.update(cx, |this, cx| {
            this._watch_local_changes = Some(cx.spawn(async move |this, cx| {
                let changes_dir = paths::agent_threads_changes_dir();
                if let Err(error) = fs.create_dir(changes_dir).await {
                    log::warn!("failed to create agent threads changes directory: {error}");
                    return;
                }
                let own_marker = changes_dir.join(std::process::id().to_string());
                let (mut events, _watcher) = fs.watch(changes_dir, CHANGES_WATCH_LATENCY).await;
                while let Some(events) = events.next().await {
                    if events.iter().all(|event| event.path == own_marker) {
                        continue;
                    }
                    let updated = this.update(cx, |this, cx| {
                        this.reload(cx);
                        cx.emit(ThreadStoreEvent::LocalThreadsChangedExternally);
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            }));
        });
    }

    /// Returns where threads of the given project are stored, if the project
    /// is remote. Threads of local projects are stored in the local database.
    pub fn remote_storage_for_project(
        &mut self,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Option<RemoteThreadStorage> {
        let project = project.read(cx);
        if project.is_via_collab() {
            return None;
        }
        let remote_client = project.remote_client()?;
        self.register_remote_host(&remote_client, cx);
        Some(
            self.remote_hosts
                .get(&remote_client.entity_id())?
                .storage
                .clone(),
        )
    }

    pub fn remote_storages(&self) -> impl Iterator<Item = &RemoteThreadStorage> + '_ {
        self.remote_hosts.values().map(|host| &host.storage)
    }

    pub fn remote_storage(&self, host_id: EntityId) -> Option<&RemoteThreadStorage> {
        self.remote_hosts.get(&host_id).map(|host| &host.storage)
    }

    fn register_remote_host(
        &mut self,
        remote_client: &Entity<RemoteClient>,
        cx: &mut Context<Self>,
    ) {
        let host_id = remote_client.entity_id();
        if self.remote_hosts.contains_key(&host_id) {
            return;
        }

        let (client, connection_options) = {
            let remote_client = remote_client.read(cx);
            (
                remote_client.proto_client(),
                remote_client.connection_options(),
            )
        };
        client.add_request_handler(
            cx.weak_entity(),
            move |this, _: client::TypedEnvelope<proto::AgentThreadsChanged>, mut cx| async move {
                this.update(&mut cx, |this, cx| this.reload_remote_host(host_id, cx));
                Ok(proto::Ack {})
            },
        );

        let subscriptions = vec![
            cx.subscribe(remote_client, move |this, _, event, cx| {
                if let RemoteClientEvent::Reconnected = event {
                    this.reload_remote_host(host_id, cx);
                }
            }),
            cx.observe_release(remote_client, move |this, _, cx| {
                if this.remote_hosts.remove(&host_id).is_some() {
                    cx.emit(ThreadStoreEvent::RemoteHostRemoved(host_id));
                    cx.notify();
                }
            }),
        ];

        self.remote_hosts.insert(
            host_id,
            RemoteThreadHost {
                storage: RemoteThreadStorage::new(host_id, client, connection_options),
                threads: Vec::new(),
                reload_task: None,
                _subscriptions: subscriptions,
            },
        );
        self.reload_remote_host(host_id, cx);
    }

    fn reload_remote_host(&mut self, host_id: EntityId, cx: &mut Context<Self>) {
        let Some(host) = self.remote_hosts.get_mut(&host_id) else {
            return;
        };
        let list = host.storage.list_threads();
        host.reload_task = Some(cx.spawn(async move |this, cx| {
            let threads = list.await;
            this.update(cx, |this, cx| {
                let Some(host) = this.remote_hosts.get_mut(&host_id) else {
                    return;
                };
                match threads {
                    Ok(threads) => {
                        host.threads = threads
                            .into_iter()
                            .filter(|thread| thread.parent_session_id.is_none())
                            .collect();
                        cx.notify();
                    }
                    Err(error) => {
                        log::error!("failed to list threads of remote host: {error:#}");
                    }
                }
                cx.emit(ThreadStoreEvent::RemoteHostChanged(host_id));
            })
            .ok();
        }));
    }

    /// Records a thread that was just saved on a remote host, without
    /// re-fetching the host's thread list.
    pub(crate) fn record_remote_save(
        &mut self,
        host_id: EntityId,
        metadata: DbThreadMetadata,
        cx: &mut Context<Self>,
    ) {
        if metadata.parent_session_id.is_some() {
            return;
        }
        let Some(host) = self.remote_hosts.get_mut(&host_id) else {
            return;
        };
        host.threads.retain(|thread| thread.id != metadata.id);
        let ix = host
            .threads
            .iter()
            .position(|thread| thread.updated_at <= metadata.updated_at)
            .unwrap_or(host.threads.len());
        host.threads.insert(ix, metadata);
        cx.notify();
    }

    /// Resolves when the most recently initiated reload has completed.
    /// Callers that need to read `entries()` and can't tolerate the initial
    /// empty state must await this before reading.
    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task.clone()
    }

    pub fn thread_from_session_id(&self, session_id: &acp::SessionId) -> Option<&DbThreadMetadata> {
        self.threads
            .iter()
            .chain(self.remote_hosts.values().flat_map(|host| &host.threads))
            .find(|thread| &thread.id == session_id)
    }

    fn remote_host_for_session(&self, session_id: &acp::SessionId) -> Option<&RemoteThreadHost> {
        if self.threads.iter().any(|thread| &thread.id == session_id) {
            return None;
        }
        self.remote_hosts
            .values()
            .find(|host| host.threads.iter().any(|thread| &thread.id == session_id))
    }

    pub fn load_thread(
        &mut self,
        id: acp::SessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<DbThread>>> {
        if let Some(host) = self.remote_host_for_session(&id) {
            return host.storage.load_thread(id, cx.background_executor());
        }
        let database_future = ThreadsDatabase::connect(cx);
        cx.background_spawn(async move {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.load_thread(id).await
        })
    }

    pub fn save_thread(
        &mut self,
        id: acp::SessionId,
        thread: crate::DbThread,
        folder_paths: PathList,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.save_thread(id, thread, folder_paths).await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn delete_thread(
        &mut self,
        id: acp::SessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Some(host) = self.remote_host_for_session(&id) {
            let host_id = host.storage.host_id();
            let delete = host.storage.delete_thread(id);
            return cx.spawn(async move |this, cx| {
                delete.await?;
                this.update(cx, |this, cx| this.reload_remote_host(host_id, cx))
            });
        }
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.delete_thread(id.clone()).await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn delete_threads(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.delete_threads().await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_task = Self::spawn_reload(cx);
    }

    fn spawn_reload(cx: &mut Context<Self>) -> Shared<Task<()>> {
        let database_connection = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let Ok(database) = database_connection.await.map_err(|err| anyhow!(err)) else {
                return;
            };
            let Ok(all_threads) = database.list_threads().await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.threads.clear();
                for thread in all_threads {
                    if thread.parent_session_id.is_some() {
                        continue;
                    }
                    this.threads.push(thread);
                }
                cx.notify();
            })
            .ok();
        })
        .shared()
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
            && self
                .remote_hosts
                .values()
                .all(|host| host.threads.is_empty())
    }

    /// Returns the threads stored locally and on all connected remote hosts,
    /// most recently updated first.
    pub fn entries(&self) -> impl Iterator<Item = DbThreadMetadata> + '_ {
        let mut seen = HashSet::default();
        let mut entries = self
            .threads
            .iter()
            .chain(self.remote_hosts.values().flat_map(|host| &host.threads))
            .filter(|thread| seen.insert(thread.id.clone()))
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries.into_iter()
    }

    /// Returns the threads stored in the local database.
    pub fn local_entries(&self) -> impl Iterator<Item = DbThreadMetadata> + '_ {
        self.threads.iter().cloned()
    }

    pub fn entry_ids(&self) -> impl Iterator<Item = acp::SessionId> + '_ {
        self.entries().map(|t| t.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use collections::HashMap;
    use gpui::TestAppContext;
    use std::sync::Arc;

    fn session_id(value: &str) -> acp::SessionId {
        acp::SessionId::new(Arc::<str>::from(value))
    }

    fn make_thread(title: &str, updated_at: DateTime<Utc>) -> DbThread {
        DbThread {
            title: title.to_string().into(),
            messages: Vec::new(),
            updated_at,
            detailed_summary: None,
            initial_project_snapshot: None,
            cumulative_token_usage: Default::default(),
            request_token_usage: HashMap::default(),
            model: None,
            profile: None,
            subagent_context: None,
            speed: None,
            thinking_enabled: false,
            thinking_effort: None,
            draft_prompt: None,
            ui_scroll_position: None,
            sandboxed_terminal_temp_dir: None,
            sandbox_grants: Default::default(),
        }
    }

    #[gpui::test]
    async fn test_entries_are_sorted_by_updated_at(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let older_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let newer_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_older = thread_store.update(cx, |store, cx| {
            store.save_thread(older_id.clone(), older_thread, PathList::default(), cx)
        });
        save_older.await.unwrap();

        let save_newer = thread_store.update(cx, |store, cx| {
            store.save_thread(newer_id.clone(), newer_thread, PathList::default(), cx)
        });
        save_newer.await.unwrap();

        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, newer_id);
        assert_eq!(entries[1].id, older_id);
    }

    #[gpui::test]
    async fn test_delete_threads_clears_entries(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let thread_id = session_id("thread-a");
        let thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );

        let save_task = thread_store.update(cx, |store, cx| {
            store.save_thread(thread_id, thread, PathList::default(), cx)
        });
        save_task.await.unwrap();

        cx.run_until_parked();
        assert!(!thread_store.read_with(cx, |store, _cx| store.is_empty()));

        let delete_task = thread_store.update(cx, |store, cx| store.delete_threads(cx));
        delete_task.await.unwrap();
        cx.run_until_parked();

        assert!(thread_store.read_with(cx, |store, _cx| store.is_empty()));
    }

    #[gpui::test]
    async fn test_delete_thread_removes_only_target(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let first_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let second_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_first = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), first_thread, PathList::default(), cx)
        });
        save_first.await.unwrap();
        let save_second = thread_store.update(cx, |store, cx| {
            store.save_thread(second_id.clone(), second_thread, PathList::default(), cx)
        });
        save_second.await.unwrap();
        cx.run_until_parked();

        let delete_task =
            thread_store.update(cx, |store, cx| store.delete_thread(first_id.clone(), cx));
        delete_task.await.unwrap();
        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, second_id);
    }

    #[gpui::test]
    async fn test_save_thread_refreshes_ordering(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let first_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let second_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_first = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), first_thread, PathList::default(), cx)
        });
        save_first.await.unwrap();
        let save_second = thread_store.update(cx, |store, cx| {
            store.save_thread(second_id.clone(), second_thread, PathList::default(), cx)
        });
        save_second.await.unwrap();
        cx.run_until_parked();

        let updated_first = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 3, 0, 0, 0).unwrap(),
        );
        let update_task = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), updated_first, PathList::default(), cx)
        });
        update_task.await.unwrap();
        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first_id);
        assert_eq!(entries[1].id, second_id);
    }
}
