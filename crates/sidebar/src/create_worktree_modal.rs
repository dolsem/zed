use agent_ui::thread_metadata_store::{ThreadFlairId, ThreadMetadataStore};
use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, Focusable, SharedString};
use ui::{ContextMenu, ContextMenuEntry, DropdownMenu, DropdownStyle, prelude::*};
use workspace::{ModalView, Workspace};
use git_ui_core::worktree_service::{RemoteBranchName, worktree_create_targets};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget, OpenCreateWorktreeDialog};

/// One ref the new worktree can be based on.
pub struct WorktreeBase {
    pub label: SharedString,
    pub target: NewWorktreeBranchTarget,
}

/// Creates a worktree: the user picks what it is based on, can name it, and picks
/// the flair its threads start with. A flair that is lazy opens the worktree
/// running only what the agent needs.
pub struct CreateWorktreeModal {
    workspace: Entity<Workspace>,
    bases: Vec<WorktreeBase>,
    selected_base: usize,
    name_editor: Entity<Editor>,
    flair: Option<ThreadFlairId>,
}

impl EventEmitter<DismissEvent> for CreateWorktreeModal {}
impl ModalView for CreateWorktreeModal {}

impl Focusable for CreateWorktreeModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl CreateWorktreeModal {
    pub fn new(
        workspace: Entity<Workspace>,
        bases: Vec<WorktreeBase>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name (leave empty for a random one)", window, cx);
            editor
        });
        Self {
            workspace,
            bases,
            selected_base: 0,
            name_editor,
            flair: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.bases.get(self.selected_base) else {
            return;
        };
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        let flair = self.flair.clone();
        let lightweight = flair
            .as_ref()
            .and_then(|id| ThreadMetadataStore::global(cx).read(cx).flair(id))
            .is_some_and(|flair| flair.is_lazy());
        let action = CreateWorktree {
            worktree_name: (!name.is_empty()).then_some(name),
            branch_target: base.target.clone(),
            lightweight,
        };

        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window
            .spawn(cx, async move |cx| {
                let creation = workspace.update_in(cx, |workspace, window, cx| {
                    let focused_dock = workspace.focused_dock_position(window, cx);
                    git_ui_core::worktree_service::create_worktree_workspace_foreground(
                        workspace,
                        &action,
                        window,
                        focused_dock,
                        cx,
                    )
                })?;
                let created = creation.await?;

                if let Some(flair) = flair {
                    cx.update(|_window, cx| {
                        let folder_paths = created
                            .workspace
                            .read(cx)
                            .project()
                            .read(cx)
                            .worktree_paths(cx)
                            .folder_path_list()
                            .clone();
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            store.set_default_flair(folder_paths, Some(flair), cx);
                        });
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    fn render_base_dropdown(&self, window: &mut Window, cx: &mut Context<Self>) -> DropdownMenu {
        let this = cx.weak_entity();
        let selected_base = self.selected_base;
        let labels = self
            .bases
            .iter()
            .map(|base| base.label.clone())
            .collect::<Vec<_>>();
        let selected_label = labels.get(selected_base).cloned().unwrap_or_default();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (index, label) in labels.into_iter().enumerate() {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    label,
                    index == selected_base,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.selected_base = index;
                            cx.notify();
                        })
                        .ok();
                    },
                );
            }
            menu
        });
        DropdownMenu::new("worktree-base", selected_label, menu)
            .style(DropdownStyle::Outlined)
            .full_width(true)
    }

    fn render_flair_dropdown(&self, window: &mut Window, cx: &mut Context<Self>) -> DropdownMenu {
        let this = cx.weak_entity();
        let flairs = ThreadMetadataStore::global(cx).read(cx).flairs().to_vec();
        let selected_flair = self
            .flair
            .as_ref()
            .and_then(|id| flairs.iter().find(|flair| &flair.id == id))
            .cloned();
        let current = self.flair.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let select = |flair: Option<ThreadFlairId>| {
                let this = this.clone();
                move |_window: &mut Window, cx: &mut App| {
                    this.update(cx, |this, cx| {
                        this.flair = flair.clone();
                        cx.notify();
                    })
                    .ok();
                }
            };
            menu = menu.toggleable_entry(
                "None",
                current.is_none(),
                IconPosition::Start,
                None,
                select(None),
            );
            for flair in flairs {
                let mut entry = ContextMenuEntry::new(flair.name.clone())
                    .toggle(IconPosition::Start, current.as_ref() == Some(&flair.id))
                    .handler(select(Some(flair.id.clone())));
                if let Some(color) = flair.hsla() {
                    entry = entry
                        .icon(IconName::Circle)
                        .icon_position(IconPosition::End)
                        .icon_color(Color::Custom(color));
                }
                menu = menu.item(entry);
            }
            menu
        });
        let label = h_flex()
            .gap_1p5()
            .when_some(
                selected_flair.as_ref().and_then(|flair| flair.hsla()),
                |this, color| this.child(div().size_2().rounded_full().bg(color)),
            )
            .child(Label::new(
                selected_flair
                    .map(|flair| flair.name)
                    .unwrap_or_else(|| "None".into()),
            ))
            .into_any_element();
        DropdownMenu::new_with_element("worktree-flair", label, menu)
            .style(DropdownStyle::Outlined)
            .full_width(true)
    }
}

impl Render for CreateWorktreeModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let field = |label: &'static str, dropdown: DropdownMenu| {
            v_flex()
                .gap_1()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(dropdown)
        };
        let base_dropdown = self.render_base_dropdown(window, cx);
        let flair_dropdown = self.render_flair_dropdown(window, cx);

        v_flex()
            .key_context("CreateWorktreeModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(26.))
            .overflow_hidden()
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new("New Worktree"))
                    .child(self.name_editor.clone())
                    .child(field("Based on", base_dropdown))
                    .child(field("Flair for new threads", flair_dropdown)),
            )
            .child(
                h_flex()
                    .bg(colors.editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .gap_1()
                    .justify_between()
                    .child(
                        Label::new("Enter to create, Esc to cancel")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("create-worktree", "Create")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Opens the dialog on the workspace, offering the bases the project can be
/// branched from. `selected` picks the base the dialog starts on, the first when
/// it is `None` or not on offer, and `name` fills in the name field.
pub fn open_for_workspace(
    workspace: &Entity<Workspace>,
    default_branch: Option<RemoteBranchName>,
    selected: Option<NewWorktreeBranchTarget>,
    name: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        open_in_workspace(workspace, default_branch, selected, name, window, cx);
    });
}

fn open_in_workspace(
    workspace: &mut Workspace,
    default_branch: Option<RemoteBranchName>,
    selected: Option<NewWorktreeBranchTarget>,
    name: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let project_ref = project.read(cx);
    let has_multiple_repositories = project_ref.repositories(cx).len() > 1;
    let current_branch = project_ref.active_repository(cx).and_then(|repo| {
        repo.read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string())
    });
    let bases: Vec<WorktreeBase> = worktree_create_targets(
        has_multiple_repositories,
        default_branch,
        current_branch.as_deref(),
    )
    .into_iter()
    .map(|target| WorktreeBase {
        label: target
            .branch_label(has_multiple_repositories, current_branch.as_deref())
            .into(),
        target: target.branch_target(),
    })
    .collect();
    let selected_base = selected
        .and_then(|selected| bases.iter().position(|base| base.target == selected))
        .unwrap_or(0);

    let base_workspace = cx.entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        let mut modal = CreateWorktreeModal::new(base_workspace, bases, window, cx);
        modal.selected_base = selected_base;
        if let Some(name) = name {
            modal
                .name_editor
                .update(cx, |editor, cx| editor.set_text(name, window, cx));
        }
        modal
    });
}

/// Handles [`OpenCreateWorktreeDialog`], dispatched by the worktree picker.
pub fn handle_open_create_worktree_dialog(
    workspace: &mut Workspace,
    action: &OpenCreateWorktreeDialog,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let default_branch = action
        .default_branch
        .as_deref()
        .and_then(RemoteBranchName::parse);
    open_in_workspace(
        workspace,
        default_branch,
        Some(action.branch_target.clone()),
        action.worktree_name.clone(),
        window,
        cx,
    );
}