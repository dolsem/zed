use agent_ui::ThreadId;
use agent_ui::thread_metadata_store::{MCP_FLAIR_ID, ThreadFlairId, ThreadMetadataStore};
use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, Focusable, Rgba};
use ui::prelude::*;
use ui::{Checkbox, ToggleState};
use workspace::ModalView;

/// Colors a flair can take. Stored as `#rrggbb`.
const PALETTE: &[&str] = &[
    "#ef4444", "#f97316", "#f59e0b", "#22c55e", "#14b8a6", "#3b82f6", "#6366f1", "#a855f7",
    "#ec4899", "#9ca3af",
];

pub enum FlairModalTarget {
    /// Creates a flair and applies it to the thread.
    Create { thread_id: ThreadId },
    Edit { flair_id: ThreadFlairId },
}

pub struct FlairModal {
    target: FlairModalTarget,
    name_editor: Entity<Editor>,
    colors: Vec<SharedString>,
    selected_color: usize,
    lazy: bool,
    error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for FlairModal {}
impl ModalView for FlairModal {}

impl Focusable for FlairModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl FlairModal {
    pub fn new(target: FlairModalTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut colors = PALETTE
            .iter()
            .map(|color| SharedString::from(*color))
            .collect::<Vec<_>>();
        let mut selected_color = 0;
        let mut initial_name = SharedString::default();
        let mut lazy = false;
        if let FlairModalTarget::Edit { flair_id } = &target
            && let Some(flair) = ThreadMetadataStore::global(cx).read(cx).flair(flair_id)
        {
            initial_name = flair.name.clone();
            lazy = flair.is_lazy();
            selected_color = match colors.iter().position(|color| *color == flair.color) {
                Some(index) => index,
                None => {
                    colors.push(flair.color.clone());
                    colors.len() - 1
                }
            };
        }
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Flair name", window, cx);
            if !initial_name.is_empty() {
                editor.set_text(initial_name.to_string(), window, cx);
            }
            editor
        });
        Self {
            target,
            name_editor,
            colors,
            selected_color,
            lazy,
            error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.error = Some("Give the flair a name.".into());
            cx.notify();
            return;
        }
        let Some(color) = self.colors.get(self.selected_color).cloned() else {
            return;
        };
        let lazy = self.lazy;
        ThreadMetadataStore::global(cx).update(cx, |store, cx| match &self.target {
            FlairModalTarget::Create { thread_id } => {
                let flair_id = store.create_flair(name.into(), color, lazy, cx);
                store.set_flair(*thread_id, Some(flair_id), cx);
            }
            FlairModalTarget::Edit { flair_id } => {
                store.update_flair(flair_id, name.into(), color, lazy, cx);
            }
        });
        cx.emit(DismissEvent);
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if let FlairModalTarget::Edit { flair_id } = &self.target {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete_flair(flair_id, cx));
        }
        cx.emit(DismissEvent);
    }

    fn render_swatch(&self, index: usize, color: &str, cx: &mut Context<Self>) -> AnyElement {
        let Some(hsla) = Rgba::try_from(color).ok().map(gpui::Hsla::from) else {
            return gpui::Empty.into_any_element();
        };
        let selected = index == self.selected_color;
        let border_color = cx.theme().colors().border_focused;
        div()
            .id(("flair-color", index))
            .size_5()
            .rounded_full()
            .cursor_pointer()
            .bg(hsla)
            .border_2()
            .border_color(gpui::transparent_black())
            .when(selected, |this| this.border_color(border_color))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.selected_color = index;
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for FlairModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let swatches = self
            .colors
            .iter()
            .enumerate()
            .map(|(index, color)| self.render_swatch(index, color, cx))
            .collect::<Vec<_>>();
        let (title, confirm_label) = match &self.target {
            FlairModalTarget::Create { .. } => ("New Flair", "Create"),
            FlairModalTarget::Edit { .. } => ("Edit Flair", "Save"),
        };
        let can_delete = matches!(
            &self.target,
            FlairModalTarget::Edit { flair_id } if flair_id.0.as_ref() != MCP_FLAIR_ID
        );

        v_flex()
            .key_context("FlairModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_80()
            .overflow_hidden()
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new(title))
                    .child(self.name_editor.clone())
                    .child(h_flex().gap_1p5().children(swatches))
                    .child(
                        Checkbox::new("flair-lazy", self.lazy.into())
                            .label("Lazy: only run what the agent needs")
                            .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                                this.lazy = state.selected();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .bg(colors.editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .gap_1()
                    .justify_between()
                    .child(match &self.error {
                        Some(error) => Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                        None => Label::new("Enter to confirm, Esc to cancel")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .when(can_delete, |this| {
                                this.child(
                                    Button::new("delete-flair", "Delete")
                                        .color(Color::Error)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.delete(cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("confirm-flair", confirm_label)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(&menu::Confirm, window, cx);
                                    })),
                            ),
                    ),
            )
    }
}
