//! A window listing what each open worktree is running, with the means to turn
//! everything but what the agent needs off, or to let go of a worktree entirely.

use std::collections::HashMap;
use std::time::Duration;

use agent_ui::thread_metadata_store::ThreadMetadataStore;
use gpui::{
    App, Context, Entity, SharedString, TitlebarOptions, WeakEntity, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use project::ResourceMode;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{MultiWorkspace, RemovalIntent, Workspace};
use zed_actions::OpenResourceManager;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &OpenResourceManager, _window, cx| {
            open_resource_manager(cx);
        });
    })
    .detach();
}

fn open_resource_manager(cx: &mut App) {
    let existing_window = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<ResourceManagerWindow>());
    if let Some(existing_window) = existing_window {
        existing_window
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        return;
    }

    let window_background = cx.theme().window_background_appearance();
    let bounds = size(px(900.), px(560.));
    cx.defer(move |cx| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Resource Manager".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                is_movable: true,
                kind: gpui::WindowKind::Normal,
                window_background,
                window_decorations: None,
                window_min_size: Some(size(px(600.), px(320.))),
                window_bounds: Some(WindowBounds::centered(bounds, cx)),
                ..Default::default()
            },
            |_window, cx| cx.new(ResourceManagerWindow::new),
        )
        .log_err();
    });
}

struct WorktreeRow {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    title: SharedString,
    mode: ResourceMode,
    is_local: bool,
    is_displayed: bool,
    thread_count: usize,
    language_servers: usize,
    repositories: usize,
    memory_bytes: u64,
    cpu_percent: f32,
}

pub struct ResourceManagerWindow {
    system: System,
    rows: Vec<WorktreeRow>,
    _refresh_task: gpui::Task<()>,
}

impl ResourceManagerWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        let refresh_task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            loop {
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            system: System::new(),
            rows: Vec::new(),
            _refresh_task: refresh_task,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // The CPU figure is the usage since the previous refresh, so the system has
        // to be kept around between refreshes rather than created for each one.
        self.system.refresh_specifics(
            RefreshKind::nothing().with_processes(
                ProcessRefreshKind::nothing()
                    .without_tasks()
                    .with_memory()
                    .with_cpu(),
            ),
        );
        let parents: HashMap<Pid, Pid> = self
            .system
            .processes()
            .iter()
            .filter_map(|(&pid, process)| Some((pid, process.parent()?)))
            .collect();

        let mut rows = Vec::new();
        for window in cx.windows() {
            let Some(window) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let Ok(multi_workspace) = window.read(cx) else {
                continue;
            };
            let displayed = multi_workspace.workspace().clone();
            for workspace in multi_workspace.workspaces() {
                rows.push(self.row_for(window, workspace, &displayed, &parents, cx));
            }
        }
        rows.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
        self.rows = rows;
        cx.notify();
    }

    fn row_for(
        &self,
        window: WindowHandle<MultiWorkspace>,
        workspace: &Entity<Workspace>,
        displayed: &Entity<Workspace>,
        parents: &HashMap<Pid, Pid>,
        cx: &App,
    ) -> WorktreeRow {
        let project = workspace.read(cx).project().read(cx);
        let title = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).root_name_str().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let folder_paths = project.worktree_paths(cx).folder_path_list().clone();
        let thread_count = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries_for_path(&folder_paths, None)
            .count();

        let mut language_servers = 0;
        let mut memory_bytes = 0;
        let mut cpu_percent = 0.0;
        for (_, status) in project.language_server_statuses(cx) {
            language_servers += 1;
            if let Some(process_id) = status.process_id {
                let (memory, cpu) = self.process_tree_usage(Pid::from_u32(process_id), parents);
                memory_bytes += memory;
                cpu_percent += cpu;
            }
        }

        WorktreeRow {
            window,
            workspace: workspace.clone(),
            title: if title.is_empty() {
                "Empty workspace".into()
            } else {
                title.into()
            },
            mode: project.resource_mode(cx),
            is_local: project.is_local(),
            is_displayed: workspace == displayed,
            thread_count,
            language_servers,
            repositories: project.repositories(cx).len(),
            memory_bytes,
            cpu_percent,
        }
    }

    /// Memory and CPU of a process and everything it started.
    fn process_tree_usage(&self, root: Pid, parents: &HashMap<Pid, Pid>) -> (u64, f32) {
        let mut memory = 0;
        let mut cpu = 0.0;
        for (&pid, process) in self.system.processes() {
            let mut current = pid;
            let mut steps = 0;
            let in_tree = loop {
                if current == root {
                    break true;
                }
                steps += 1;
                match parents.get(&current) {
                    // The step limit guards against a cycle in the parent links.
                    Some(&parent) if steps < 64 => current = parent,
                    _ => break false,
                }
            };
            if in_tree {
                memory += process.memory();
                cpu += process.cpu_usage();
            }
        }
        (memory, cpu)
    }

    fn set_mode(&mut self, workspace: &Entity<Workspace>, mode: ResourceMode, cx: &mut Context<Self>) {
        workspace
            .read(cx)
            .project()
            .clone()
            .update(cx, |project, cx| project.set_resource_mode(mode, cx));
        self.refresh(cx);
    }

    /// Lets go of a worktree's workspace. What the user is looking at stays.
    fn suspend(&mut self, row_index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(row_index) else {
            return;
        };
        let workspace = row.workspace.clone();
        row.window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace
                    .remove([workspace], RemovalIntent::KeepProject, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn render_row(&self, index: usize, row: &WorktreeRow, cx: &mut Context<Self>) -> AnyElement {
        let mode_label = match row.mode {
            ResourceMode::Full => "Full",
            ResourceMode::AgentOnly => "Agent only",
            ResourceMode::Git => "Git only",
        };
        let mut details = vec![
            format!("{} threads", row.thread_count),
            format!("{} language servers", row.language_servers),
            format!("{} repositories", row.repositories),
        ];
        if row.language_servers > 0 {
            details.push(format!(
                "{:.0} MiB, {:.0}% CPU",
                row.memory_bytes as f64 / (1024.0 * 1024.0),
                row.cpu_percent
            ));
        }

        let workspace = row.workspace.clone();
        let agent_only_workspace = row.workspace.clone();
        let git_workspace = row.workspace.clone();
        h_flex()
            .id(("worktree-row", index))
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(row.title.clone()))
                            .child(
                                Label::new(mode_label)
                                    .size(LabelSize::Small)
                                    .color(match row.mode {
                                        ResourceMode::Full => Color::Warning,
                                        ResourceMode::AgentOnly => Color::Success,
                                        ResourceMode::Git => Color::Info,
                                    }),
                            )
                            .when(row.is_displayed, |this| {
                                this.child(
                                    Label::new("open")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        Label::new(details.join("  ·  "))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(("agent-only", index), "Agent only")
                            .disabled(!row.is_local || row.mode == ResourceMode::AgentOnly)
                            .tooltip(Tooltip::text(
                                "Stop indexing, git and language servers; keep what the agent needs",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_mode(&agent_only_workspace, ResourceMode::AgentOnly, cx);
                            })),
                    )
                    .child(
                        Button::new(("git", index), "Git only")
                            .disabled(!row.is_local || row.mode == ResourceMode::Git)
                            .tooltip(Tooltip::text("Track git, but stop language servers"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_mode(&git_workspace, ResourceMode::Git, cx);
                            })),
                    )
                    .child(
                        Button::new(("full", index), "Full")
                            .disabled(!row.is_local || row.mode == ResourceMode::Full)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_mode(&workspace, ResourceMode::Full, cx);
                            })),
                    )
                    .child(
                        Button::new(("suspend", index), "Suspend")
                            .disabled(row.is_displayed)
                            .tooltip(Tooltip::text(
                                "Close this worktree's workspace. Its threads stay in the sidebar.",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.suspend(index, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ResourceManagerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total_memory: u64 = self.rows.iter().map(|row| row.memory_bytes).sum();
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| self.render_row(index, row, cx))
            .collect::<Vec<_>>();

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Worktrees").size(LabelSize::Large))
                    .child(
                        Label::new(format!(
                            "Language servers: {:.0} MiB",
                            total_memory as f64 / (1024.0 * 1024.0)
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .id("worktree-rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}
