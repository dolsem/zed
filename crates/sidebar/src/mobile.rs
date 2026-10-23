//! The threads list and thread activation used by the mobile web app
//! (`mobile_bridge`). Unlike the sidebar's own list, this one ignores collapsed
//! groups and the search filter, so every active thread is listed.

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

/// Shows an agent thread in Zed, opening its workspace (and connecting to its
/// remote host) if needed, exactly as clicking it in the sidebar would.
pub fn activate_thread(thread_id: ThreadId, cx: &mut App) -> anyhow::Result<()> {
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
        let workspace = multi_workspace
            .workspace_for_paths(metadata.folder_paths(), metadata.remote_connection.as_ref(), cx)?;
        Some((*window, workspace))
    });
    let key = ProjectGroupKey::from_worktree_paths(
        &metadata.worktree_paths,
        metadata.remote_connection.clone(),
    );
    let target_window = match &open_workspace {
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

    let sidebar = target_window
        .read(cx)?
        .sidebar()
        .and_then(|sidebar| sidebar.to_any().downcast::<Sidebar>().ok())
        .ok_or_else(|| anyhow::anyhow!("the window has no threads sidebar"))?;
    // Activation updates the multi-workspace, so it must not be leased by
    // `WindowHandle::update` while the sidebar runs.
    let window_handle: gpui::AnyWindowHandle = target_window.into();
    window_handle.update(cx, |_, window, cx| {
        sidebar.update(cx, |sidebar, cx| match open_workspace {
            Some((_, workspace)) => {
                sidebar.activate_thread(metadata.clone(), &workspace, false, window, cx);
            }
            None => {
                let folder_paths = metadata.folder_paths().clone();
                sidebar.open_workspace_and_activate_thread(
                    metadata.clone(),
                    folder_paths,
                    &key,
                    window,
                    cx,
                );
            }
        });
    })
}
