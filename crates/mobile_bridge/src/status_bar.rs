use fs::Fs;
use gpui::{App, Context, Render, Subscription, Window};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

use crate::MobileServerSettings;

/// Status bar toggle for `mobile_server.always_notify`. Only shown while the
/// mobile server is enabled.
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
        let always_notify = !MobileServerSettings::get_global(cx).always_notify;
        update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
            settings
                .mobile_server
                .get_or_insert_default()
                .always_notify = Some(always_notify);
        });
    }
}

impl Render for MobileNotificationsIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = MobileServerSettings::get_global(cx);
        if !settings.enabled {
            return div().hidden().into_any_element();
        }
        let (icon, color, tooltip) = if settings.always_notify {
            (
                IconName::BellRing,
                Color::Accent,
                "Phone notifications: always. Click to only notify after using the mobile app.",
            )
        } else {
            (
                IconName::Bell,
                Color::Muted,
                "Phone notifications: only after using the mobile app. Click to always notify.",
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
