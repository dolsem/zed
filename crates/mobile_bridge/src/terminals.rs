use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{App, Context, Entity, Subscription, Task, WeakEntity};
use mobile_protocol::ClientId;
use std::time::{Duration, Instant};
use terminal::Terminal;
use terminal_view::{TerminalView, terminal_element::convert_color};
use theme::ActiveTheme as _;
use workspace::item::Item as _;

use crate::api::{
    ItemId, TerminalClosedEvent, TerminalCursor, TerminalCursorShape, TerminalRun, TerminalScreen,
    WorkspaceId, events,
};
use crate::workspace_items::find_item;
use crate::{MobileBridge, Subscribers, css_color, find_workspace, to_json};

const SCREEN_THROTTLE: Duration = Duration::from_millis(50);

type TerminalKey = (WorkspaceId, ItemId);

#[derive(Default)]
pub(crate) struct TerminalsState {
    terminals: HashMap<TerminalKey, WatchedTerminal>,
}

struct WatchedTerminal {
    view: WeakEntity<TerminalView>,
    subscribers: Subscribers,
    last_sent: Option<Instant>,
    pending_screen: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalsState {
    pub fn remove_client(&mut self, client_id: ClientId) {
        self.terminals.retain(|_, terminal| {
            terminal.subscribers.remove(&client_id);
            !terminal.subscribers.is_empty()
        });
    }

    pub fn unsubscribe(&mut self, client_id: ClientId, workspace_id: WorkspaceId, item_id: ItemId) {
        let key = (workspace_id, item_id);
        if let Some(terminal) = self.terminals.get_mut(&key) {
            terminal.subscribers.remove(&client_id);
            if terminal.subscribers.is_empty() {
                self.terminals.remove(&key);
            }
        }
    }
}

impl MobileBridge {
    pub(crate) fn open_terminal(
        &mut self,
        client_id: ClientId,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        Task::ready(self.subscribe_terminal(client_id, workspace_id, item_id, cx))
    }

    fn subscribe_terminal(
        &mut self,
        client_id: ClientId,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value> {
        let view = find_terminal_view(workspace_id, item_id, cx)?;
        let key = (workspace_id, item_id);
        let watched = self.terminals.terminals.entry(key).or_insert_with(|| {
            let terminal = view.read(cx).terminal().clone();
            let subscriptions = vec![
                cx.subscribe(
                    &terminal,
                    move |this, _, event: &terminal::Event, cx| match event {
                        terminal::Event::Wakeup
                        | terminal::Event::TitleChanged
                        | terminal::Event::Bell => this.schedule_terminal_screen(key, cx),
                        terminal::Event::CloseTerminal => this.close_terminal(key),
                        _ => {}
                    },
                ),
                cx.observe_release(&terminal, move |this, _, _| this.close_terminal(key)),
                cx.observe_release(&view, move |this, _, _| this.close_terminal(key)),
            ];
            WatchedTerminal {
                view: view.downgrade(),
                subscribers: Subscribers::default(),
                last_sent: None,
                pending_screen: None,
                _subscriptions: subscriptions,
            }
        });
        watched.subscribers.insert(client_id);
        to_json(terminal_screen(item_id, &view, cx))
    }

    fn close_terminal(&mut self, key: TerminalKey) {
        let Some(watched) = self.terminals.terminals.remove(&key) else {
            return;
        };
        let (workspace_id, item_id) = key;
        let event = TerminalClosedEvent {
            workspace_id,
            item_id,
        };
        for client_id in watched.subscribers {
            self.send_event(client_id, events::TERMINAL_CLOSED, &event);
        }
    }

    fn schedule_terminal_screen(&mut self, key: TerminalKey, cx: &mut Context<Self>) {
        let Some(watched) = self.terminals.terminals.get_mut(&key) else {
            return;
        };
        if watched.pending_screen.is_some() {
            return;
        }
        let delay = watched.last_sent.map_or(Duration::ZERO, |last_sent| {
            SCREEN_THROTTLE.saturating_sub(last_sent.elapsed())
        });
        watched.pending_screen = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |this, cx| this.send_terminal_screen(key, cx))
                .ok();
        }));
    }

    fn send_terminal_screen(&mut self, key: TerminalKey, cx: &mut Context<Self>) {
        let Some(watched) = self.terminals.terminals.get_mut(&key) else {
            return;
        };
        watched.pending_screen = None;
        watched.last_sent = Some(Instant::now());
        let view = watched.view.upgrade();
        let subscribers = watched.subscribers.iter().copied().collect::<Vec<_>>();
        let Some(view) = view else {
            self.close_terminal(key);
            return;
        };
        let screen = terminal_screen(key.1, &view, cx);
        for client_id in subscribers {
            self.send_event(client_id, events::TERMINAL_SCREEN, &screen);
        }
    }

    pub(crate) fn terminal_input(
        &mut self,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        data: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let result = find_terminal(workspace_id, item_id, cx).map(|terminal| {
            terminal.update(cx, |terminal, cx| {
                terminal.input(data.into_bytes());
                terminal.scroll_viewport_to_bottom(cx);
            });
            serde_json::Value::Null
        });
        Task::ready(result)
    }

    pub(crate) fn terminal_scroll(
        &mut self,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        lines: i32,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        // Scrolling wakes the terminal, which sends the subscribers a fresh screen.
        let result = find_terminal(workspace_id, item_id, cx).map(|terminal| {
            terminal.update(cx, |terminal, cx| terminal.scroll_viewport_by(lines, cx));
            serde_json::Value::Null
        });
        Task::ready(result)
    }
}

fn find_terminal_view(
    workspace_id: WorkspaceId,
    item_id: ItemId,
    cx: &App,
) -> Result<Entity<TerminalView>> {
    let (_, workspace) = find_workspace(workspace_id, cx)?;
    find_item(&workspace, item_id, cx)
        .context("the terminal is no longer open")?
        .downcast::<TerminalView>()
        .context("the tab isn't a terminal")
}

fn find_terminal(workspace_id: WorkspaceId, item_id: ItemId, cx: &App) -> Result<Entity<Terminal>> {
    Ok(find_terminal_view(workspace_id, item_id, cx)?
        .read(cx)
        .terminal()
        .clone())
}

#[derive(Clone, PartialEq)]
struct RunStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    italic: bool,
    underline: bool,
    dim: bool,
    strikethrough: bool,
}

fn terminal_screen(item_id: ItemId, view: &Entity<TerminalView>, cx: &App) -> TerminalScreen {
    let view = view.read(cx);
    let title = view.tab_content_text(0, cx).to_string();
    let content = view.terminal().read(cx).current_content();
    let theme = cx.theme();
    let default_foreground = css_color(theme.colors().terminal_foreground);
    let default_background = css_color(theme.colors().terminal_background);
    let css = |color: &terminal::Color, default: &str| {
        let color = css_color(convert_color(color, theme));
        (color != default).then_some(color)
    };

    let rows = content.screen_lines;
    let display_offset = content.display_offset;
    let viewport_row = |line: i32| {
        usize::try_from(i64::from(line) + display_offset as i64)
            .ok()
            .filter(|row| *row < rows)
    };

    let mut lines: Vec<Vec<TerminalRun>> = vec![Vec::new(); rows];
    for cell in &content.cells {
        if cell.is_wide_char_spacer() {
            continue;
        }
        let Some(line) = viewport_row(cell.point.line).and_then(|row| lines.get_mut(row)) else {
            continue;
        };
        let (foreground, background) = if cell.is_inverse() {
            (cell.background(), cell.foreground())
        } else {
            (cell.foreground(), cell.background())
        };
        let style = RunStyle {
            fg: css(&foreground, &default_foreground),
            bg: css(&background, &default_background),
            bold: cell.is_bold(),
            italic: cell.is_italic(),
            underline: cell.has_underline() || cell.has_undercurl(),
            dim: cell.is_dim(),
            strikethrough: cell.has_strikeout(),
        };
        let mut text = String::new();
        text.push(cell.character());
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth);
        }
        push_run(line, text, style);
    }
    for line in &mut lines {
        trim_trailing_blank_runs(line);
    }

    let cursor_shape = match content.cursor.shape {
        terminal::CursorShape::Block => Some(TerminalCursorShape::Block),
        terminal::CursorShape::Underline => Some(TerminalCursorShape::Underline),
        terminal::CursorShape::Bar => Some(TerminalCursorShape::Bar),
        terminal::CursorShape::HollowBlock => Some(TerminalCursorShape::Hollow),
        terminal::CursorShape::Hidden => None,
    };
    let cursor = cursor_shape
        .filter(|_| content.mode.contains(terminal::Modes::SHOW_CURSOR))
        .and_then(|shape| {
            Some(TerminalCursor {
                row: viewport_row(content.cursor.point.line)?,
                column: content.cursor.point.column,
                shape,
            })
        });

    TerminalScreen {
        item_id,
        title,
        columns: content.columns,
        rows,
        lines,
        cursor,
        display_offset,
        history_size: content.total_lines.saturating_sub(content.screen_lines),
        bracketed_paste: content.mode.contains(terminal::Modes::BRACKETED_PASTE),
        application_cursor: content.mode.contains(terminal::Modes::APP_CURSOR),
    }
}

fn push_run(line: &mut Vec<TerminalRun>, text: String, style: RunStyle) {
    if let Some(last) = line.last_mut()
        && last.fg == style.fg
        && last.bg == style.bg
        && last.bold == style.bold
        && last.italic == style.italic
        && last.underline == style.underline
        && last.dim == style.dim
        && last.strikethrough == style.strikethrough
    {
        last.text.push_str(&text);
        return;
    }
    line.push(TerminalRun {
        text,
        fg: style.fg,
        bg: style.bg,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        dim: style.dim,
        strikethrough: style.strikethrough,
    });
}

/// Drops trailing whitespace that renders the same as the empty background,
/// which keeps screens of mostly empty lines small.
fn trim_trailing_blank_runs(line: &mut Vec<TerminalRun>) {
    while let Some(last) = line.last_mut() {
        if last.bg.is_some() || last.underline || last.strikethrough {
            return;
        }
        let trimmed_length = last.text.trim_end_matches(' ').len();
        if trimmed_length > 0 {
            last.text.truncate(trimmed_length);
            return;
        }
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, bg: Option<&str>) -> TerminalRun {
        TerminalRun {
            text: text.to_string(),
            fg: None,
            bg: bg.map(str::to_string),
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            strikethrough: false,
        }
    }

    #[test]
    fn test_trim_trailing_blank_runs() {
        let mut line = vec![run("$ ls  ", None), run("   ", None)];
        trim_trailing_blank_runs(&mut line);
        assert_eq!(line, vec![run("$ ls", None)]);

        let mut line = vec![run("a", None), run("  ", Some("#ff0000ff"))];
        trim_trailing_blank_runs(&mut line);
        assert_eq!(line.len(), 2);

        let mut line = vec![run("    ", None)];
        trim_trailing_blank_runs(&mut line);
        assert!(line.is_empty());
    }
}
