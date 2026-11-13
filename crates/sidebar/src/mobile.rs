//! The threads list and thread activation used by the mobile web app
//! (`mobile_bridge`). Unlike the sidebar's own list, this one ignores collapsed
//! groups and the search filter, so every active thread is listed.

use std::time::Duration;

use anyhow::Context as _;
use gpui::AsyncApp;

use super::*;

pub struct MobileProject {
    pub key: ProjectGroupKey,
    pub name: SharedString,
    pub is_active: bool,
    pub threads: Vec<MobileThread>,
}

pub struct MobileThread {
    pub id: MobileThreadId,
    pub title: SharedString,
    pub status: AgentThreadStatus,
    pub agent_id: Option<AgentId>,
    pub worktrees: Vec<SharedString>,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub has_notification: bool,
    pub is_active: bool,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    /// Always `None` for terminals.
    pub flair: Option<ThreadFlairId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MobileThreadId {
    Agent(ThreadId),
    Terminal(TerminalId),
}

/// The window's project groups, plus a group for each open workspace that
/// isn't part of one yet (workspaces only join a group once pinned).
pub fn project_groups(multi_workspace: &MultiWorkspace, cx: &App) -> Vec<workspace::ProjectGroup> {
    let mut groups = multi_workspace.project_groups(cx);
    for workspace in multi_workspace.workspaces() {
        let key = workspace.read(cx).project_group_key(cx);
        if key.path_list().paths().is_empty() {
            continue;
        }
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => {
                if !group.workspaces.contains(workspace) {
                    group.workspaces.push(workspace.clone());
                }
            }
            None => groups.push(workspace::ProjectGroup {
                key,
                workspaces: vec![workspace.clone()],
                expanded: true,
            }),
        }
    }
    groups
}

/// Lists the threads of every project group in every window, newest first.
pub fn threads_snapshot(cx: &App) -> Vec<MobileProject> {
    let mut projects = Vec::new();
    let mut seen_thread_ids = HashSet::new();
    let mut seen_terminal_ids = HashSet::new();
    for window in cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
    {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        let sidebar = multi_workspace
            .sidebar()
            .and_then(|sidebar| sidebar.to_any().downcast::<Sidebar>().ok());
        let sidebar = sidebar.as_ref().map(|sidebar| sidebar.read(cx));
        let workspaces = multi_workspace.workspaces().cloned().collect::<Vec<_>>();
        let active_workspace = multi_workspace.workspace().clone();

        let mut branch_by_path = HashMap::new();
        for workspace in &workspaces {
            let project = workspace.read(cx).project().read(cx);
            for repository in project.repositories(cx).values() {
                let snapshot = repository.read(cx).snapshot();
                if let Some(branch) = &snapshot.branch {
                    branch_by_path.insert(
                        snapshot.work_directory_abs_path.to_path_buf(),
                        SharedString::from(Arc::<str>::from(branch.name())),
                    );
                }
                for linked_worktree in snapshot.linked_worktrees() {
                    if let Some(branch) = linked_worktree.branch_name() {
                        branch_by_path.insert(
                            linked_worktree.path.clone(),
                            SharedString::from(Arc::<str>::from(branch)),
                        );
                    }
                }
            }
        }

        let groups = project_groups(multi_workspace, cx);
        let mut all_paths: Vec<PathBuf> = groups
            .iter()
            .flat_map(|group| group.key.path_list().paths().iter().cloned())
            .collect();
        all_paths.sort_unstable();
        all_paths.dedup();
        let path_details =
            util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
                project::path_suffix(path, detail)
            });
        let path_detail_map: HashMap<PathBuf, usize> =
            all_paths.into_iter().zip(path_details).collect();

        let thread_store = ThreadMetadataStore::global(cx).read(cx);
        let terminal_store = TerminalThreadMetadataStore::global(cx).read(cx);
        for group in &groups {
            let key = &group.key;
            if key.path_list().paths().is_empty() {
                continue;
            }
            let host = key.host();
            let mut path_lists = vec![key.path_list().clone()];
            path_lists.extend(
                group
                    .workspaces
                    .iter()
                    .map(|workspace| workspace_path_list(workspace, cx))
                    .filter(|path_list| !path_list.paths().is_empty()),
            );
            path_lists.extend(linked_worktree_path_lists_for_workspaces(
                &group.workspaces,
                cx,
            ));

            let live_infos: HashMap<acp::SessionId, ActiveThreadInfo> = group
                .workspaces
                .iter()
                .flat_map(|workspace| all_thread_infos_for_workspace(workspace, cx))
                .map(|info| (info.session_id.clone(), info))
                .collect();

            let mut threads = Vec::new();
            let agent_rows = thread_store
                .entries_for_main_worktree_path(key.path_list(), host.as_ref())
                .chain(
                    path_lists
                        .iter()
                        .flat_map(|path_list| thread_store.entries_for_path(path_list, host.as_ref())),
                );
            for metadata in agent_rows {
                // Drafts have no messages yet, so there's nothing to show remotely.
                let Some(session_id) = metadata.session_id.as_ref() else {
                    continue;
                };
                if !seen_thread_ids.insert(metadata.thread_id) {
                    continue;
                }
                let live_info = live_infos.get(session_id);
                let worktrees = worktree_labels(&worktree_info_from_thread_paths(
                    &metadata.worktree_paths,
                    &branch_by_path,
                ));
                let (title, status, diff_stats) = match live_info {
                    Some(info) => (info.title.clone(), info.status, info.diff_stats),
                    None => (
                        metadata.display_title(),
                        AgentThreadStatus::Completed,
                        DiffStats::default(),
                    ),
                };
                threads.push(MobileThread {
                    id: MobileThreadId::Agent(metadata.thread_id),
                    title,
                    status,
                    agent_id: Some(metadata.agent_id.clone()),
                    worktrees,
                    lines_added: diff_stats.lines_added,
                    lines_removed: diff_stats.lines_removed,
                    has_notification: sidebar
                        .is_some_and(|sidebar| sidebar.contents.is_thread_notified(&metadata.thread_id)),
                    is_active: sidebar.is_some_and(|sidebar| {
                        sidebar.active_entry.as_ref().is_some_and(|entry| {
                            entry.is_active_thread(&metadata.thread_id)
                                && *entry.workspace() == active_workspace
                        })
                    }),
                    updated_at: Sidebar::thread_display_time(metadata),
                    archived: metadata.archived,
                    flair: metadata.flair.clone(),
                });
            }

            let terminal_rows = terminal_store
                .entries_for_main_worktree_path(key.path_list(), host.as_ref())
                .chain(path_lists.iter().flat_map(|path_list| {
                    terminal_store.entries_for_path(path_list, host.as_ref())
                }));
            for metadata in terminal_rows {
                if !seen_terminal_ids.insert(metadata.terminal_id) {
                    continue;
                }
                threads.push(MobileThread {
                    id: MobileThreadId::Terminal(metadata.terminal_id),
                    title: metadata.display_title(),
                    status: AgentThreadStatus::Completed,
                    agent_id: None,
                    worktrees: worktree_labels(&worktree_info_from_thread_paths(
                        &metadata.worktree_paths,
                        &branch_by_path,
                    )),
                    lines_added: 0,
                    lines_removed: 0,
                    has_notification: sidebar.is_some_and(|sidebar| {
                        sidebar
                            .contents
                            .is_terminal_notified(metadata.terminal_id)
                    }),
                    is_active: sidebar.is_some_and(|sidebar| {
                        sidebar.active_entry.as_ref().is_some_and(|entry| {
                            entry.is_active_terminal(metadata.terminal_id)
                                && *entry.workspace() == active_workspace
                        })
                    }),
                    updated_at: metadata.created_at,
                    archived: metadata.archived,
                    flair: None,
                });
            }

            threads.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
            projects.push(MobileProject {
                key: key.clone(),
                name: key.display_name(&path_detail_map),
                is_active: group.workspaces.contains(&active_workspace),
                threads,
            });
        }
    }
    projects
}

/// Lists every thread Zed knows of, archived ones included, whether or not its
/// project is open or the thread is loaded, grouped by project: the open
/// project groups first, as in [`threads_snapshot`], then the others, most
/// recently used first. Threads that aren't loaded are reported idle.
pub fn all_threads_snapshot(cx: &App) -> Vec<MobileProject> {
    let mut projects = threads_snapshot(cx);
    let mut listed_thread_ids = HashSet::new();
    let mut listed_terminal_ids = HashSet::new();
    for thread in projects.iter().flat_map(|project| &project.threads) {
        match thread.id {
            MobileThreadId::Agent(thread_id) => listed_thread_ids.insert(thread_id),
            MobileThreadId::Terminal(terminal_id) => listed_terminal_ids.insert(terminal_id),
        };
    }

    let mut live_infos: HashMap<acp::SessionId, ActiveThreadInfo> = HashMap::new();
    let mut branch_by_path = HashMap::new();
    for window in cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
    {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            live_infos.extend(
                all_thread_infos_for_workspace(workspace, cx)
                    .map(|info| (info.session_id.clone(), info)),
            );
            for repository in workspace.read(cx).project().read(cx).repositories(cx).values() {
                let snapshot = repository.read(cx).snapshot();
                if let Some(branch) = &snapshot.branch {
                    branch_by_path.insert(
                        snapshot.work_directory_abs_path.to_path_buf(),
                        SharedString::from(Arc::<str>::from(branch.name())),
                    );
                }
                for linked_worktree in snapshot.linked_worktrees() {
                    if let Some(branch) = linked_worktree.branch_name() {
                        branch_by_path.insert(
                            linked_worktree.path.clone(),
                            SharedString::from(Arc::<str>::from(branch)),
                        );
                    }
                }
            }
        }
    }

    // Threads not listed yet, by the project they belong to.
    let mut unlisted: Vec<(ProjectGroupKey, MobileThread)> = Vec::new();
    let thread_store = ThreadMetadataStore::global(cx).read(cx);
    for metadata in thread_store.entries() {
        // Drafts have no messages yet, so there's nothing to show remotely.
        let Some(session_id) = metadata.session_id.as_ref() else {
            continue;
        };
        if listed_thread_ids.contains(&metadata.thread_id) {
            continue;
        }
        let (title, status, diff_stats) = match live_infos.get(session_id) {
            Some(info) => (info.title.clone(), info.status, info.diff_stats),
            None => (
                metadata.display_title(),
                AgentThreadStatus::Completed,
                DiffStats::default(),
            ),
        };
        unlisted.push((
            ProjectGroupKey::from_worktree_paths(
                &metadata.worktree_paths,
                metadata.remote_connection.clone(),
            ),
            MobileThread {
                id: MobileThreadId::Agent(metadata.thread_id),
                title,
                status,
                agent_id: Some(metadata.agent_id.clone()),
                worktrees: worktree_labels(&worktree_info_from_thread_paths(
                    &metadata.worktree_paths,
                    &branch_by_path,
                )),
                lines_added: diff_stats.lines_added,
                lines_removed: diff_stats.lines_removed,
                has_notification: false,
                is_active: false,
                updated_at: Sidebar::thread_display_time(metadata),
                archived: metadata.archived,
                flair: metadata.flair.clone(),
            },
        ));
    }
    let terminal_store = TerminalThreadMetadataStore::global(cx).read(cx);
    for metadata in terminal_store.entries() {
        if listed_terminal_ids.contains(&metadata.terminal_id) {
            continue;
        }
        unlisted.push((
            ProjectGroupKey::from_worktree_paths(
                &metadata.worktree_paths,
                metadata.remote_connection.clone(),
            ),
            MobileThread {
                id: MobileThreadId::Terminal(metadata.terminal_id),
                title: metadata.display_title(),
                status: AgentThreadStatus::Completed,
                agent_id: None,
                worktrees: worktree_labels(&worktree_info_from_thread_paths(
                    &metadata.worktree_paths,
                    &branch_by_path,
                )),
                lines_added: 0,
                lines_removed: 0,
                has_notification: false,
                is_active: false,
                updated_at: metadata.created_at,
                archived: metadata.archived,
                flair: None,
            },
        ));
    }

    let open_project_count = projects.len();
    for (key, thread) in unlisted {
        if key.path_list().paths().is_empty() {
            continue;
        }
        match projects
            .iter_mut()
            .find(|project| project.key.matches(&key))
        {
            Some(project) => project.threads.push(thread),
            None => projects.push(MobileProject {
                key,
                name: SharedString::default(),
                is_active: false,
                threads: vec![thread],
            }),
        }
    }
    for project in &mut projects {
        project
            .threads
            .sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
    }
    projects[open_project_count..].sort_by_key(|project| {
        std::cmp::Reverse(project.threads.first().map(|thread| thread.updated_at))
    });

    // Names are disambiguated against every project listed, open or not.
    let mut all_paths: Vec<PathBuf> = projects
        .iter()
        .flat_map(|project| project.key.path_list().paths().iter().cloned())
        .collect();
    all_paths.sort_unstable();
    all_paths.dedup();
    let path_details = util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
        project::path_suffix(path, detail)
    });
    let path_detail_map: HashMap<PathBuf, usize> = all_paths.into_iter().zip(path_details).collect();
    for project in &mut projects {
        project.name = project.key.display_name(&path_detail_map);
    }
    projects
}

fn worktree_labels(worktrees: &[ThreadItemWorktreeInfo]) -> Vec<SharedString> {
    worktrees
        .iter()
        .filter(|worktree| worktree.kind == ui::WorktreeKind::Linked)
        .filter_map(|worktree| {
            worktree
                .branch_name
                .clone()
                .or_else(|| worktree.worktree_name.clone())
        })
        .collect()
}

/// Finds or opens (in the background, connecting to its remote host if
/// needed) the workspace for `folder_paths` in `window`'s project group `key`.
pub fn find_or_open_workspace(
    window: WindowHandle<MultiWorkspace>,
    key: ProjectGroupKey,
    folder_paths: PathList,
    cx: &mut App,
) -> Task<anyhow::Result<Entity<Workspace>>> {
    let result = window.update(cx, |multi_workspace, window, cx| {
        let active_workspace = multi_workspace.workspace().clone();
        multi_workspace.find_or_create_workspace(
            folder_paths,
            key.host(),
            Some(key),
            |options, window, cx| connect_remote(active_workspace, options, window, cx),
            None,
            OpenMode::Add,
            None,
            window,
            cx,
        )
    });
    match result {
        Ok(task) => task,
        Err(error) => Task::ready(Err(error)),
    }
}

/// Unarchives the thread if it's archived, restoring the worktrees archiving
/// removed from disk, like [`Sidebar`]'s `open_thread_from_archive` does.
async fn unarchive_thread(thread_id: ThreadId, cx: &mut AsyncApp) -> anyhow::Result<()> {
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    let Some((remote_connection, archived_worktrees)) = cx.update(|cx| {
        let metadata = store.read(cx).entry(thread_id)?;
        metadata.archived.then(|| {
            (
                metadata.remote_connection.clone(),
                store.read(cx).get_archived_worktrees_for_thread(thread_id, cx),
            )
        })
    }) else {
        return Ok(());
    };

    let mut path_replacements = Vec::new();
    for row in &archived_worktrees.await? {
        let restored_path = thread_worktree_archive::restore_worktree_via_git(
            row,
            remote_connection.as_ref(),
            cx,
        )
        .await
        .with_context(|| {
            format!(
                "failed to restore the thread's worktree {}",
                row.worktree_path.display()
            )
        })?;
        thread_worktree_archive::cleanup_archived_worktree_record(
            row,
            remote_connection.as_ref(),
            cx,
        )
        .await;
        path_replacements.push((row.worktree_path.clone(), restored_path));
    }

    store.update(cx, |store, cx| {
        if !path_replacements.is_empty() {
            store.update_restored_worktree_paths(thread_id, &path_replacements, cx);
        }
        store.unarchive(thread_id, cx);
    });
    Ok(())
}

/// Where a thread lives: the window to open it in, and its workspace if
/// that's already open.
struct ThreadTarget {
    metadata: ThreadMetadata,
    window: WindowHandle<MultiWorkspace>,
    open_workspace: Option<Entity<Workspace>>,
    key: ProjectGroupKey,
}

fn thread_target(thread_id: ThreadId, cx: &App) -> anyhow::Result<ThreadTarget> {
    let metadata = ThreadMetadataStore::global(cx)
        .read(cx)
        .entry(thread_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let windows = cx
        .window_stack()
        .unwrap_or_else(|| cx.windows())
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect::<Vec<_>>();

    let open_workspace = windows.iter().find_map(|window| {
        let multi_workspace = window.read(cx).ok()?;
        let workspace = multi_workspace.workspace_for_paths(
            metadata.folder_paths(),
            metadata.remote_connection.as_ref(),
            cx,
        )?;
        Some((*window, workspace))
    });
    let key = ProjectGroupKey::from_worktree_paths(
        &metadata.worktree_paths,
        metadata.remote_connection.clone(),
    );
    let window = match &open_workspace {
        Some((window, _)) => *window,
        None => windows
            .iter()
            .find(|window| {
                window.read(cx).is_ok_and(|multi_workspace| {
                    project_groups(multi_workspace, cx)
                        .iter()
                        .any(|group| group.key.matches(&key))
                })
            })
            .or_else(|| windows.first())
            .copied()
            .ok_or_else(|| anyhow::anyhow!("no Zed window is open"))?,
    };
    Ok(ThreadTarget {
        metadata,
        window,
        open_workspace: open_workspace.map(|(_, workspace)| workspace),
        key,
    })
}

/// Loads an agent thread in its workspace's agent panel without showing it,
/// opening the workspace in the background first if needed, so that the
/// user's view stays as it is.
///
/// An archived thread is unarchived first, as restoring it from Thread
/// History does: the worktrees archiving removed are restored through git.
pub fn load_thread_in_background(thread_id: ThreadId, cx: &mut App) -> Task<anyhow::Result<()>> {
    const PANEL_TIMEOUT: Duration = Duration::from_secs(90);
    const PANEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

    cx.spawn(async move |cx| {
        unarchive_thread(thread_id, cx).await?;
        let ThreadTarget {
            metadata,
            window,
            open_workspace,
            key,
        } = cx.update(|cx| thread_target(thread_id, cx))?;
        let workspace = match open_workspace {
            Some(workspace) => workspace,
            None => {
                cx.update(|cx| {
                    find_or_open_workspace(window, key, metadata.folder_paths().clone(), cx)
                })
                .await?
            }
        };
        // A newly opened workspace registers its panels asynchronously.
        let deadline = cx.background_executor().now() + PANEL_TIMEOUT;
        let panel = loop {
            if let Some(panel) = cx.update(|cx| workspace.read(cx).panel::<AgentPanel>(cx)) {
                break panel;
            }
            if cx.background_executor().now() >= deadline {
                anyhow::bail!("the thread's workspace didn't finish loading");
            }
            cx.background_executor().timer(PANEL_POLL_INTERVAL).await;
        };
        window.update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.load_agent_thread_in_background(
                    Agent::from(metadata.agent_id.clone()),
                    metadata.thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                    AgentThreadSource::Sidebar,
                    window,
                    cx,
                );
            })
        })
    })
}
