use fs::Fs;
use gpui::{App, Context, Render, Subscription, Window};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

use crate::{HttpApiSettings, WebUiSettings};

/// Status bar toggle for `web_ui.always_notify`. Only shown while the web UI
/// is enabled.
pub struct MobileNotificationsIndicator {
    _settings_subscription: Subscription,
}

impl MobileNotificationsIndicator {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    fn toggle(cx: &mut App) {
        let always_notify = !WebUiSettings::get_global(cx).always_notify;
        update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
            settings.web_ui.get_or_insert_default().always_notify = Some(always_notify);
        });
    }
}

impl Render for MobileNotificationsIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = WebUiSettings::get_global(cx);
        if !settings.enabled || !HttpApiSettings::get_global(cx).enabled {
            return div().hidden().into_any_element();
        }
        let (icon, color, tooltip) = if settings.always_notify {
            (
                IconName::BellRing,
                Color::Accent,
                "Phone notifications: always. Click to only notify after using the web UI.",
            )
        } else {
            (
                IconName::Bell,
                Color::Muted,
                "Phone notifications: only after using the web UI. Click to always notify.",
            )
        };
        IconButton::new("mobile-notifications", icon)
            .icon_size(IconSize::Small)
            .icon_color(color)
            .tooltip(Tooltip::text(tooltip))
            .on_click(|_, _, cx| Self::toggle(cx))
            .into_any_element()
    }
}

impl StatusItemView for MobileNotificationsIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
