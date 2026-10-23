use anyhow::{Context as _, Result, anyhow, bail, ensure};
use buffer_diff::{BufferDiff, BufferDiffEvent};
use collections::HashMap;
use editor::Editor;
use futures::future::join_all;
use git::Oid;
use git::repository::{CommitFileStatus, LogOrder, LogSource, RepoPath};
use git::status::{FileStatus as GitFileStatus, StageStatus, StatusCode};
use git_ui::{commit_view::CommitView, project_diff::ProjectDiff};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Task};
use language::Buffer;
use mobile_protocol::ClientId;
use project::git_store::{
    CommitDataState, Repository, RepositoryEvent, RepositoryId as GitRepositoryId, StatusEntry,
};
use project::{Project, ProjectPath, WorktreeId as ProjectWorktreeId};
use std::str::FromStr as _;
use std::time::{Duration, Instant};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::{Pane, SaveIntent, Workspace, item::ItemHandle};

use crate::api::{
    BufferHandle, CommitFile, CommitSummary, DiffBaseChangedEvent, DiffFile, DiffFilesChangedEvent,
    DirectoryEntry, DirectoryListing, EditorTab, EditorTabKind, FileStatus, GitLog, ItemContent,
    ItemId, OpenedItem, RepositoryId, TerminalTab, WorkspaceId, WorkspaceItems,
    WorkspaceItemsChangedEvent, WorktreeId, events,
};
use crate::{MobileBridge, Subscribers, active_workspace, find_workspace, to_json};

const ITEMS_CHANGED_DEBOUNCE: Duration = Duration::from_millis(100);
const DIFF_FILES_CHANGED_DEBOUNCE: Duration = Duration::from_millis(100);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const GIT_LOG_TIMEOUT: Duration = Duration::from_secs(30);
const OPEN_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);

type ItemViewKey = (ClientId, WorkspaceId, ItemId);

#[derive(Default)]
pub(crate) struct WorkspaceItemsState {
    workspaces: HashMap<WorkspaceId, WatchedWorkspace>,
    item_views: HashMap<ItemViewKey, ItemView>,
    /// The latest `item.open` of each view, so that a slow open that was
    /// closed or superseded in the meantime doesn't install its view.
    pending_opens: HashMap<ItemViewKey, u64>,
    next_open_id: u64,
}

struct WatchedWorkspace {
    subscribers: Subscribers,
    /// Set when panes or panels may have been added, so the pane
    /// subscriptions must be rebuilt.
    needs_resubscribe: bool,
    pending_notification: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

/// What a client's full-screen view of an item holds on to.
#[derive(Default)]
struct ItemView {
    buffers: Vec<BufferHandle>,
    /// For diff views, the repository's changed files when the view was
    /// opened, to tell when the client must re-open it.
    status_signature: Vec<(RepoPath, GitFileStatus)>,
    pending_files_changed: Option<Task<()>>,
    _diffs: Vec<Entity<BufferDiff>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl WorkspaceItemsState {
    pub fn remove_client(&mut self, client_id: ClientId) {
        self.workspaces.retain(|_, workspace| {
            workspace.subscribers.remove(&client_id);
            !workspace.subscribers.is_empty()
        });
        self.item_views
            .retain(|(view_client_id, _, _), _| *view_client_id != client_id);
        self.pending_opens
            .retain(|(view_client_id, _, _), _| *view_client_id != client_id);
    }
}

impl MobileBridge {
    pub(crate) fn workspace_items(
        &mut self,
        client_id: ClientId,
        workspace_id: Option<WorkspaceId>,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let workspace = match workspace_id {
            Some(workspace_id) => find_workspace(workspace_id, cx),
            None => active_workspace(cx),
        };
        let workspace = match workspace {
            Ok((_, workspace)) => workspace,
            Err(error) => return Task::ready(Err(error)),
        };
        self.watch_workspace(client_id, &workspace, cx);
        Task::ready(to_json(workspace_items(&workspace, cx)))
    }

    fn watch_workspace(
        &mut self,
        client_id: ClientId,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let workspace_id = workspace.entity_id().as_u64();
        let watched = self
            .workspace_items
            .workspaces
            .entry(workspace_id)
            .or_insert_with(|| WatchedWorkspace {
                subscribers: Subscribers::default(),
                needs_resubscribe: true,
                pending_notification: None,
                _subscriptions: Vec::new(),
            });
        watched.subscribers.insert(client_id);
        if watched.needs_resubscribe {
            self.resubscribe_workspace(workspace, cx);
        }
    }

    fn resubscribe_workspace(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let workspace_id = workspace.entity_id().as_u64();
        let mut subscriptions = vec![cx.subscribe(
            workspace,
            move |this, _, event: &workspace::Event, cx| match event {
                workspace::Event::PaneAdded(_)
                | workspace::Event::PaneRemoved
                | workspace::Event::PanelAdded(_) => {
                    this.schedule_items_changed(workspace_id, true, cx)
                }
                workspace::Event::ItemAdded { .. }
                | workspace::Event::ItemRemoved { .. }
                | workspace::Event::ActiveItemChanged => {
                    this.schedule_items_changed(workspace_id, false, cx)
                }
                _ => {}
            },
        )];
        if let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) {
            subscriptions.push(cx.observe(&terminal_panel, move |this, _, cx| {
                this.schedule_items_changed(workspace_id, true, cx);
            }));
        }
        for pane in all_panes(workspace, cx) {
            subscriptions.push(cx.subscribe(
                &pane,
                move |this, _, event: &workspace::pane::Event, cx| {
                    if matches!(
                        event,
                        workspace::pane::Event::AddItem { .. }
                            | workspace::pane::Event::RemovedItem { .. }
                            | workspace::pane::Event::ActivateItem { .. }
                            | workspace::pane::Event::ChangeItemTitle
                            | workspace::pane::Event::Remove { .. }
                    ) {
                        this.schedule_items_changed(workspace_id, false, cx);
                    }
                },
            ));
        }
        if let Some(watched) = self.workspace_items.workspaces.get_mut(&workspace_id) {
            watched._subscriptions = subscriptions;
            watched.needs_resubscribe = false;
        }
    }

    fn schedule_items_changed(
        &mut self,
        workspace_id: WorkspaceId,
        resubscribe: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(watched) = self.workspace_items.workspaces.get_mut(&workspace_id) else {
            return;
        };
        watched.needs_resubscribe |= resubscribe;
        if watched.pending_notification.is_some() {
            return;
        }
        watched.pending_notification = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ITEMS_CHANGED_DEBOUNCE).await;
            this.update(cx, |this, cx| this.send_items_changed(workspace_id, cx))
                .ok();
        }));
    }

    fn send_items_changed(&mut self, workspace_id: WorkspaceId, cx: &mut Context<Self>) {
        let Some(watched) = self.workspace_items.workspaces.get_mut(&workspace_id) else {
            return;
        };
        watched.pending_notification = None;
        let needs_resubscribe = watched.needs_resubscribe;
        let subscribers = watched.subscribers.iter().copied().collect::<Vec<_>>();
        if needs_resubscribe {
            match find_workspace(workspace_id, cx) {
                Ok((_, workspace)) => self.resubscribe_workspace(&workspace, cx),
                Err(_) => {
                    self.workspace_items.workspaces.remove(&workspace_id);
                    return;
                }
            }
        }
        let event = WorkspaceItemsChangedEvent { workspace_id };
        for client_id in subscribers {
            self.send_event(client_id, events::WORKSPACE_ITEMS_CHANGED, &event);
        }
    }

    pub(crate) fn close_item(
        &mut self,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let close = (|| {
            let (window, workspace) = find_workspace(workspace_id, cx)?;
            let (item, pane) =
                find_item_and_pane(&workspace, item_id, cx).context("the tab is no longer open")?;
            // Terminals are closed without asking about running processes, since
            // the phone asked for it explicitly.
            let save_intent = if item.downcast::<TerminalView>().is_some() {
                SaveIntent::Skip
            } else {
                // A save prompt would show on the desktop, where nobody might
                // be around to answer it.
                ensure!(!item.is_dirty(cx), "Save or discard changes in Zed first");
                SaveIntent::Close
            };
            window.update(cx, |_, window, cx| {
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(item.item_id(), save_intent, window, cx)
                })
            })
        })();
        match close {
            Ok(close) => cx.background_spawn(async move {
                close.await?;
                Ok(serde_json::Value::Null)
            }),
            Err(error) => Task::ready(Err(error)),
        }
    }

    pub(crate) fn new_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let add_terminal = (|| {
            let (window, workspace) = find_workspace(workspace_id, cx)?;
            let terminal_panel = workspace
                .read(cx)
                .panel::<TerminalPanel>(cx)
                .context("the terminal panel isn't available")?;
            let add_terminal = window.update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_default_terminal(window, cx)
                })
            })?;
            anyhow::Ok((workspace, add_terminal))
        })();
        let (workspace, add_terminal) = match add_terminal {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |_, cx| {
            let terminal = add_terminal.await?;
            let item_id = cx.update(|cx| {
                let terminal = terminal.upgrade()?;
                all_panes(&workspace, cx).iter().find_map(|pane| {
                    pane.read(cx).items().find_map(|item| {
                        let view = item.downcast::<TerminalView>()?;
                        (view.read(cx).terminal() == &terminal).then(|| item.item_id().as_u64())
                    })
                })
            });
            let item_id = item_id.context("the new terminal closed")?;
            Ok(serde_json::json!({ "item_id": item_id }))
        })
    }

    pub(crate) fn list_directory(
        &mut self,
        workspace_id: WorkspaceId,
        worktree_id: Option<WorktreeId>,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        Task::ready(
            list_directory(workspace_id, worktree_id, path, cx)
                .and_then(|entries| to_json(DirectoryListing { entries })),
        )
    }

    pub(crate) fn open_file(
        &mut self,
        workspace_id: WorkspaceId,
        worktree_id: WorktreeId,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let open = (|| {
            let (window, workspace) = find_workspace(workspace_id, cx)?;
            let project_path = ProjectPath {
                worktree_id: ProjectWorktreeId::from_proto(worktree_id),
                path: RelPath::from_unix_str(path)?.into_arc(),
            };
            window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
            })
        })();
        let open = match open {
            Ok(open) => open,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |_, cx| {
            let item = open.await?;
            let kind = cx.update(|cx| item_kind(item.as_ref(), cx));
            to_json(OpenedItem {
                item_id: item.item_id().as_u64(),
                kind,
                offset: None,
            })
        })
    }

    pub(crate) fn git_log(
        &mut self,
        workspace_id: WorkspaceId,
        offset: usize,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let repository = match find_workspace(workspace_id, cx) {
            Ok((_, workspace)) => workspace.read(cx).project().read(cx).active_repository(cx),
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(repository) = repository else {
            return Task::ready(to_json(GitLog {
                repository_id: None,
                branch: None,
                commits: Vec::new(),
                has_more: false,
            }));
        };
        let (repository_id, branch, head) = {
            let repository = repository.read(cx);
            (
                repository.id.0,
                repository
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string()),
                repository
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string()),
            )
        };
        let Some(head) = head else {
            return Task::ready(to_json(GitLog {
                repository_id: Some(repository_id),
                branch,
                commits: Vec::new(),
                has_more: false,
            }));
        };
        let head = match Oid::from_str(&head) {
            Ok(head) => head,
            Err(error) => return Task::ready(Err(error)),
        };

        cx.spawn(async move |_, cx| {
            let source = LogSource::Sha(head);
            // One commit past the page tells whether there are more.
            let end = offset.saturating_add(limit).saturating_add(1);
            let deadline = Instant::now() + GIT_LOG_TIMEOUT;
            let commits = loop {
                let (commits, is_loading, error) = repository.update(cx, |repository, cx| {
                    let response =
                        repository.graph_data(source.clone(), LogOrder::default(), 0..end, cx);
                    (
                        response.commits.to_vec(),
                        response.is_loading,
                        response.error,
                    )
                });
                if let Some(error) = error {
                    bail!("failed to load the git log: {error}");
                }
                if !is_loading || commits.len() >= end {
                    break commits;
                }
                ensure!(Instant::now() < deadline, "timed out loading the git log");
                cx.background_executor().timer(POLL_INTERVAL).await;
            };
            let has_more = commits.len() > offset.saturating_add(limit);
            let page = commits
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();

            let commit_data = repository.update(cx, |repository, cx| {
                page.iter()
                    .map(|commit| repository.fetch_commit_data(commit.sha, true, cx).clone())
                    .collect::<Vec<_>>()
            });
            let mut summaries = Vec::with_capacity(page.len());
            for (commit, data) in page.iter().zip(commit_data) {
                let data = match data {
                    CommitDataState::Loaded(data) => data,
                    CommitDataState::Loading(Some(receiver)) => receiver.await?,
                    CommitDataState::Loading(None) => {
                        bail!("couldn't load commit {}", commit.sha)
                    }
                };
                summaries.push(CommitSummary {
                    sha: commit.sha.to_string(),
                    subject: data.subject.to_string(),
                    author: data.author_name.to_string(),
                    timestamp: data.commit_timestamp,
                    refs: commit
                        .ref_names
                        .iter()
                        .map(|ref_name| ref_name.to_string())
                        .collect(),
                });
            }
            to_json(GitLog {
                repository_id: Some(repository_id),
                branch,
                commits: summaries,
                has_more,
            })
        })
    }

    pub(crate) fn open_uncommitted_changes(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = (|| {
            let (window, workspace) = find_workspace(workspace_id, cx)?;
            let item_id = window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    ProjectDiff::deploy_at(workspace, None, window, cx);
                    workspace
                        .items_of_type::<ProjectDiff>(cx)
                        .next()
                        .map(|project_diff| project_diff.entity_id().as_u64())
                })
            })?;
            let item_id = item_id.context("couldn't open the uncommitted changes")?;
            to_json(OpenedItem {
                item_id,
                kind: EditorTabKind::Diff,
                offset: None,
            })
        })();
        Task::ready(result)
    }

    pub(crate) fn open_commit(
        &mut self,
        workspace_id: WorkspaceId,
        sha: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let open = (|| {
            let (window, workspace) = find_workspace(workspace_id, cx)?;
            let repository = workspace
                .read(cx)
                .project()
                .read(cx)
                .active_repository(cx)
                .context("the project has no git repository")?;
            let existing_views = commit_views(&workspace, &sha, cx);
            window.update(cx, |_, window, cx| {
                CommitView::open(
                    sha.clone(),
                    repository.downgrade(),
                    workspace.downgrade(),
                    None,
                    None,
                    window,
                    cx,
                );
            })?;
            anyhow::Ok((workspace, existing_views))
        })();
        let (workspace, existing_views) = match open {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        // `CommitView::open` loads the commit in the background and doesn't
        // report the tab it adds, so look for it.
        cx.spawn(async move |_, cx| {
            let deadline = Instant::now() + OPEN_COMMIT_TIMEOUT;
            loop {
                let item_id = cx.update(|cx| {
                    commit_views(&workspace, &sha, cx)
                        .into_iter()
                        .find(|item_id| !existing_views.contains(item_id))
                });
                if let Some(item_id) = item_id {
                    return to_json(OpenedItem {
                        item_id,
                        kind: EditorTabKind::Commit,
                        offset: None,
                    });
                }
                ensure!(Instant::now() < deadline, "couldn't open the commit");
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
    }

    pub(crate) fn set_paths_staged(
        &mut self,
        workspace_id: WorkspaceId,
        repository_id: RepositoryId,
        paths: Vec<String>,
        staged: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let update = (|| {
            let (_, workspace) = find_workspace(workspace_id, cx)?;
            let repository = workspace
                .read(cx)
                .project()
                .read(cx)
                .repositories(cx)
                .get(&GitRepositoryId(repository_id))
                .cloned()
                .context("the repository is no longer open")?;
            let repo_paths = paths
                .iter()
                .map(|path| RepoPath::new(path))
                .collect::<Result<Vec<_>>>()?;
            Ok(repository.update(cx, |repository, cx| {
                if staged {
                    repository.stage_entries(repo_paths, cx)
                } else {
                    repository.unstage_entries(repo_paths, cx)
                }
            }))
        })();
        match update {
            Ok(update) => cx.background_spawn(async move {
                update.await?;
                Ok(serde_json::Value::Null)
            }),
            Err(error) => Task::ready(Err(error)),
        }
    }

    pub(crate) fn open_item(
        &mut self,
        client_id: ClientId,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        self.close_item_view(client_id, workspace_id, item_id, cx);
        let key = (client_id, workspace_id, item_id);
        let open_id = self.workspace_items.next_open_id;
        self.workspace_items.next_open_id += 1;
        self.workspace_items.pending_opens.insert(key, open_id);
        match self.start_opening_item(key, open_id, cx) {
            Ok(task) => task,
            Err(error) => {
                self.workspace_items.pending_opens.remove(&key);
                Task::ready(Err(error))
            }
        }
    }

    fn start_opening_item(
        &mut self,
        key: ItemViewKey,
        open_id: u64,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<serde_json::Value>>> {
        let (client_id, workspace_id, item_id) = key;
        let (_, workspace) = find_workspace(workspace_id, cx)?;
        let item = find_item(&workspace, item_id, cx).context("the tab is no longer open")?;
        let project = workspace.read(cx).project().clone();
        let title = item.tab_content_text(0, cx).to_string();

        if let Some(editor) = item.downcast::<Editor>()
            && let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton()
        {
            let buffer = self.subscribe_buffer(client_id, buffer, project.downgrade(), cx)?;
            self.install_item_view(
                key,
                open_id,
                ItemView {
                    buffers: vec![buffer.buffer],
                    ..ItemView::default()
                },
            );
            return Ok(Task::ready(to_json(ItemContent::File { title, buffer })));
        }
        if let Some(project_diff) = item.downcast::<ProjectDiff>() {
            return Ok(self.open_diff_view(key, open_id, title, &project_diff, project, cx));
        }
        if let Some(commit_view) = item.downcast::<CommitView>() {
            self.install_item_view(key, open_id, ItemView::default());
            return Ok(open_commit_view(title, &commit_view, cx));
        }
        bail!("this tab can't be shown on mobile")
    }

    /// Installs a view unless it was closed or opened again while loading.
    fn install_item_view(&mut self, key: ItemViewKey, open_id: u64, view: ItemView) {
        if self.workspace_items.pending_opens.get(&key) != Some(&open_id) {
            for handle in view.buffers {
                self.release_buffer(key.0, handle);
            }
            return;
        }
        self.workspace_items.pending_opens.remove(&key);
        if let Some(previous) = self.workspace_items.item_views.insert(key, view) {
            for handle in previous.buffers {
                self.release_buffer(key.0, handle);
            }
        }
    }

    fn open_diff_view(
        &mut self,
        key: ItemViewKey,
        open_id: u64,
        title: String,
        project_diff: &Entity<ProjectDiff>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let Some(repository) = project_diff
            .read(cx)
            .repo(cx)
            .or_else(|| project.read(cx).active_repository(cx))
        else {
            return Task::ready(Err(anyhow!("the project has no git repository")));
        };
        let entries = changed_files(&repository, cx);
        let open_buffers = entries
            .iter()
            .map(|entry| {
                if entry.status.is_deleted() {
                    return None;
                }
                let project_path = repository
                    .read(cx)
                    .repo_path_to_project_path(&entry.repo_path, cx)?;
                Some(project.update(cx, |project, cx| project.open_buffer(project_path, cx)))
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |this, cx| {
            let buffers = join_all(
                open_buffers
                    .into_iter()
                    .map(|open_buffer| async move { open_buffer?.await.log_err() }),
            )
            .await;
            let open_diffs = project.update(cx, |project, cx| {
                buffers
                    .iter()
                    .map(|buffer| {
                        let buffer = buffer.clone()?;
                        Some(project.open_uncommitted_diff(buffer, cx))
                    })
                    .collect::<Vec<_>>()
            });
            let diffs = join_all(
                open_diffs
                    .into_iter()
                    .map(|open_diff| async move { open_diff?.await.log_err() }),
            )
            .await;
            this.update(cx, |this, cx| {
                this.install_diff_view(
                    DiffViewContents {
                        key,
                        open_id,
                        title,
                        repository,
                        entries,
                        buffers,
                        diffs,
                        project,
                    },
                    cx,
                )
            })?
        })
    }

    fn install_diff_view(
        &mut self,
        contents: DiffViewContents,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value> {
        let DiffViewContents {
            key,
            open_id,
            title,
            repository,
            entries,
            buffers,
            diffs,
            project,
        } = contents;
        ensure!(
            self.workspace_items.pending_opens.get(&key) == Some(&open_id),
            "the item was closed"
        );
        let (client_id, _, _) = key;
        let status_signature = status_signature(&entries);
        let mut view = ItemView::default();
        let mut files = Vec::with_capacity(entries.len());
        for ((entry, buffer), diff) in entries.into_iter().zip(buffers).zip(diffs) {
            let Some(status) = file_status(entry.status) else {
                continue;
            };
            let buffer = buffer.and_then(|buffer| {
                self.subscribe_buffer(client_id, buffer, project.downgrade(), cx)
                    .log_err()
            });
            let base_text = diff
                .as_ref()
                .and_then(|diff| diff.read(cx).base_text_string(cx));
            if let Some(buffer) = &buffer {
                view.buffers.push(buffer.buffer);
                if let Some(diff) = diff {
                    let handle = buffer.buffer;
                    view._subscriptions.push(cx.subscribe(
                        &diff,
                        move |this, diff, event: &BufferDiffEvent, cx| {
                            if matches!(event, BufferDiffEvent::BaseTextChanged) {
                                let event = DiffBaseChangedEvent {
                                    buffer: handle,
                                    base_text: diff.read(cx).base_text_string(cx),
                                };
                                this.send_event(client_id, events::DIFF_BASE_CHANGED, &event);
                            }
                        },
                    ));
                    view._diffs.push(diff);
                }
            }
            let staging = entry.status.staging();
            files.push(DiffFile {
                path: entry.repo_path.as_unix_str().to_string(),
                status,
                is_staged: staging.is_fully_staged(),
                is_partially_staged: staging == StageStatus::PartiallyStaged,
                buffer,
                base_text,
            });
        }
        view.status_signature = status_signature;
        view._subscriptions.push(cx.subscribe(
            &repository,
            move |this, repository, event: &RepositoryEvent, cx| {
                if matches!(event, RepositoryEvent::StatusesChanged) {
                    this.check_diff_files(key, &repository, cx);
                }
            },
        ));
        let repository_id = repository.read(cx).id.0;
        self.install_item_view(key, open_id, view);
        to_json(ItemContent::Diff {
            title,
            repository_id,
            files,
        })
    }

    fn check_diff_files(
        &mut self,
        key: ItemViewKey,
        repository: &Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let signature = status_signature(&changed_files(repository, cx));
        let Some(view) = self.workspace_items.item_views.get_mut(&key) else {
            return;
        };
        if view.status_signature == signature || view.pending_files_changed.is_some() {
            return;
        }
        view.status_signature = signature;
        view.pending_files_changed = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(DIFF_FILES_CHANGED_DEBOUNCE)
                .await;
            this.update(cx, |this, _| {
                if let Some(view) = this.workspace_items.item_views.get_mut(&key) {
                    view.pending_files_changed = None;
                }
                let (client_id, workspace_id, item_id) = key;
                let event = DiffFilesChangedEvent {
                    workspace_id,
                    item_id,
                };
                this.send_event(client_id, events::DIFF_FILES_CHANGED, &event);
            })
            .ok();
        }));
    }

    pub(crate) fn close_item_view(
        &mut self,
        client_id: ClientId,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        _cx: &mut Context<Self>,
    ) {
        let key = (client_id, workspace_id, item_id);
        self.workspace_items.pending_opens.remove(&key);
        if let Some(view) = self.workspace_items.item_views.remove(&key) {
            for handle in view.buffers {
                self.release_buffer(client_id, handle);
            }
        }
    }
}

struct DiffViewContents {
    key: ItemViewKey,
    open_id: u64,
    title: String,
    repository: Entity<Repository>,
    entries: Vec<StatusEntry>,
    buffers: Vec<Option<Entity<Buffer>>>,
    diffs: Vec<Option<Entity<BufferDiff>>>,
    project: Entity<Project>,
}

fn open_commit_view(
    title: String,
    commit_view: &Entity<CommitView>,
    cx: &mut Context<MobileBridge>,
) -> Task<Result<serde_json::Value>> {
    let commit_view = commit_view.read(cx);
    let commit = commit_view.commit().clone();
    let load_diff =
        commit_view
            .repository()
            .read(cx)
            .load_commit_diff(commit.sha.to_string(), false, cx);
    cx.background_spawn(async move {
        let diff = load_diff.await?;
        let files = diff
            .files
            .into_iter()
            .map(|file| CommitFile {
                path: file.path.as_unix_str().to_string(),
                status: match file.status() {
                    CommitFileStatus::Added => FileStatus::Added,
                    CommitFileStatus::Deleted => FileStatus::Deleted,
                    _ => FileStatus::Modified,
                },
                old_text: file.old_text,
                new_text: file.new_text,
                is_binary: file.is_binary,
            })
            .collect();
        to_json(ItemContent::Commit {
            title,
            sha: commit.sha.to_string(),
            message: commit.message.to_string(),
            author: commit.author_name.to_string(),
            timestamp: commit.commit_timestamp,
            files,
        })
    })
}

/// The workspace's center panes followed by the terminal panel's panes.
fn all_panes(workspace: &Entity<Workspace>, cx: &App) -> Vec<Entity<Pane>> {
    let workspace = workspace.read(cx);
    let mut panes = workspace.panes().to_vec();
    if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) {
        panes.extend(terminal_panel.read(cx).panes().into_iter().cloned());
    }
    panes
}

fn find_item_and_pane(
    workspace: &Entity<Workspace>,
    item_id: ItemId,
    cx: &App,
) -> Option<(Box<dyn ItemHandle>, Entity<Pane>)> {
    let item_id = EntityId::from(item_id);
    all_panes(workspace, cx).into_iter().find_map(|pane| {
        let item = pane
            .read(cx)
            .items()
            .find(|item| item.item_id() == item_id)?
            .boxed_clone();
        Some((item, pane))
    })
}

/// Finds an item in the workspace's center panes or terminal panel.
pub(crate) fn find_item(
    workspace: &Entity<Workspace>,
    item_id: ItemId,
    cx: &App,
) -> Option<Box<dyn ItemHandle>> {
    find_item_and_pane(workspace, item_id, cx).map(|(item, _)| item)
}

pub(crate) fn item_kind(item: &dyn ItemHandle, cx: &App) -> EditorTabKind {
    if item
        .downcast::<Editor>()
        .is_some_and(|editor| editor.read(cx).buffer().read(cx).is_singleton())
    {
        EditorTabKind::File
    } else if item.downcast::<ProjectDiff>().is_some() {
        EditorTabKind::Diff
    } else if item.downcast::<CommitView>().is_some() {
        EditorTabKind::Commit
    } else {
        EditorTabKind::Other
    }
}

fn workspace_items(workspace: &Entity<Workspace>, cx: &App) -> WorkspaceItems {
    let workspace_ref = workspace.read(cx);
    let name = workspace_ref
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).root_name_str().to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut editors = Vec::new();
    let mut terminals = Vec::new();
    for pane in all_panes(workspace, cx) {
        for item in pane.read(cx).items() {
            if let Some(terminal_view) = item.downcast::<TerminalView>() {
                terminals.push(TerminalTab {
                    item_id: item.item_id().as_u64(),
                    title: item.tab_content_text(0, cx).to_string(),
                    has_bell: terminal_view.read(cx).has_bell(),
                });
            } else if workspace_ref.panes().contains(&pane) {
                let kind = item_kind(item.as_ref(), cx);
                editors.push(EditorTab {
                    item_id: item.item_id().as_u64(),
                    kind,
                    title: item.tab_content_text(0, cx).to_string(),
                    path: (kind == EditorTabKind::File)
                        .then(|| item.project_path(cx))
                        .flatten()
                        .map(|project_path| project_path.path.as_unix_str().to_string()),
                    is_dirty: item.is_dirty(cx),
                });
            }
        }
    }

    WorkspaceItems {
        workspace_id: workspace.entity_id().as_u64(),
        name,
        editors,
        terminals,
        active_item_id: workspace_ref
            .active_item(cx)
            .map(|item| item.item_id().as_u64()),
    }
}

fn commit_views(workspace: &Entity<Workspace>, sha: &str, cx: &App) -> Vec<ItemId> {
    workspace
        .read(cx)
        .items_of_type::<CommitView>(cx)
        .filter(|commit_view| {
            let commit_sha = &commit_view.read(cx).commit().sha;
            commit_sha.starts_with(sha) || sha.starts_with(commit_sha.as_ref())
        })
        .map(|commit_view| commit_view.entity_id().as_u64())
        .collect()
}

fn list_directory(
    workspace_id: WorkspaceId,
    worktree_id: Option<WorktreeId>,
    path: &str,
    cx: &App,
) -> Result<Vec<DirectoryEntry>> {
    let (_, workspace) = find_workspace(workspace_id, cx)?;
    let project = workspace.read(cx).project().read(cx);

    let Some(worktree_id) = worktree_id else {
        ensure!(path.is_empty(), "a worktree_id is required to list a path");
        let mut roots = project
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                DirectoryEntry {
                    worktree_id: worktree.id().to_proto(),
                    path: String::new(),
                    name: worktree.root_name_str().to_string(),
                    is_dir: worktree.root_entry().is_some_and(|entry| entry.is_dir()),
                    is_ignored: false,
                    git_status: None,
                }
            })
            .collect::<Vec<_>>();
        sort_directory_entries(&mut roots);
        return Ok(roots);
    };

    let project_worktree_id = ProjectWorktreeId::from_proto(worktree_id);
    let worktree = project
        .worktree_for_id(project_worktree_id, cx)
        .context("the folder is no longer open")?;
    let snapshot = worktree.read(cx).snapshot();
    let directory = RelPath::from_unix_str(path)?;
    let git_store = project.git_store().read(cx);
    let mut entries = snapshot
        .child_entries(directory)
        .map(|entry| {
            let git_status = if entry.is_dir() {
                None
            } else {
                let project_path = ProjectPath {
                    worktree_id: project_worktree_id,
                    path: entry.path.clone(),
                };
                git_store
                    .repository_and_path_for_project_path(&project_path, cx)
                    .and_then(|(repository, repo_path)| {
                        repository.read(cx).status_for_path(&repo_path)
                    })
                    .and_then(|status_entry| file_status(status_entry.status))
            };
            DirectoryEntry {
                worktree_id,
                path: entry.path.as_unix_str().to_string(),
                name: entry.path.file_name().unwrap_or_default().to_string(),
                is_dir: entry.is_dir(),
                is_ignored: entry.is_ignored,
                git_status,
            }
        })
        .collect::<Vec<_>>();
    sort_directory_entries(&mut entries);
    Ok(entries)
}

fn sort_directory_entries(entries: &mut [DirectoryEntry]) {
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// The repository's changed files, including untracked ones even when Zed's
/// desktop diff hides them.
fn changed_files(repository: &Entity<Repository>, cx: &App) -> Vec<StatusEntry> {
    repository
        .read(cx)
        .cached_status()
        .filter(|entry| file_status(entry.status).is_some())
        .collect()
}

fn status_signature(entries: &[StatusEntry]) -> Vec<(RepoPath, GitFileStatus)> {
    entries
        .iter()
        .map(|entry| (entry.repo_path.clone(), entry.status))
        .collect()
}

fn file_status(status: GitFileStatus) -> Option<FileStatus> {
    match status {
        GitFileStatus::Ignored => None,
        GitFileStatus::Untracked => Some(FileStatus::Untracked),
        GitFileStatus::Unmerged(_) => Some(FileStatus::Conflict),
        GitFileStatus::Tracked(tracked) => {
            let codes = [tracked.index_status, tracked.worktree_status];
            if status.is_deleted() {
                Some(FileStatus::Deleted)
            } else if status.is_created() {
                Some(FileStatus::Added)
            } else if codes.contains(&StatusCode::Renamed) {
                Some(FileStatus::Renamed)
            } else if codes.iter().any(|code| *code != StatusCode::Unmodified) {
                Some(FileStatus::Modified)
            } else {
                None
            }
        }
    }
}
