//! OS-level desktop notifications.
//!
//! This is deliberately separate from `ui::notifications`, which is the
//! in-TUI chat notification channel. Call sites emit semantic events here and
//! the platform module owns OS-specific mechanics. macOS, Linux, and Windows
//! are all backed by `notify-rust`; any other target falls back to a no-op.

use crate::config::{Config, DesktopNotificationConfig};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use std::io;

    use super::NotificationSpec;

    pub(super) fn notify(_spec: &NotificationSpec<'_>) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesktopNotifyEvent {
    Completion,
    InputRequired(InputRequiredKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputRequiredKind {
    Permission,
    Question,
    PluginDialog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NotificationSpec<'a> {
    pub(super) title: &'a str,
    pub(super) message: &'a str,
}

pub(crate) fn notify(cfg: &Config, event: DesktopNotifyEvent) {
    if !should_notify(cfg.desktop_notifications.as_ref(), event) {
        return;
    }

    let spec = spec_for(event);
    if let Err(err) = platform::notify(&spec) {
        tracing::debug!(
            target: "dirge::ui::desktop_notify",
            error = %err,
            "desktop notification skipped"
        );
    }
}

fn should_notify(settings: Option<&DesktopNotificationConfig>, event: DesktopNotifyEvent) -> bool {
    let Some(settings) = settings else {
        return false;
    };
    if !settings.enabled.unwrap_or(false) {
        return false;
    }
    match event {
        DesktopNotifyEvent::Completion => settings.on_completion.unwrap_or(true),
        DesktopNotifyEvent::InputRequired(_) => settings.on_input_required.unwrap_or(true),
    }
}

fn spec_for(event: DesktopNotifyEvent) -> NotificationSpec<'static> {
    match event {
        DesktopNotifyEvent::Completion => NotificationSpec {
            title: "Dirge",
            message: "Run completed.",
        },
        DesktopNotifyEvent::InputRequired(InputRequiredKind::Permission) => NotificationSpec {
            title: "Dirge needs approval",
            message: "A tool permission prompt is waiting for your input.",
        },
        DesktopNotifyEvent::InputRequired(InputRequiredKind::Question) => NotificationSpec {
            title: "Dirge needs input",
            message: "A question is waiting for your response.",
        },
        DesktopNotifyEvent::InputRequired(InputRequiredKind::PluginDialog) => NotificationSpec {
            title: "Dirge needs input",
            message: "A plugin dialog is waiting for your response.",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> DesktopNotificationConfig {
        DesktopNotificationConfig {
            enabled: Some(true),
            on_completion: None,
            on_input_required: None,
        }
    }

    #[test]
    fn notifications_are_off_when_config_absent_or_disabled() {
        assert!(!should_notify(None, DesktopNotifyEvent::Completion));

        let cfg = DesktopNotificationConfig {
            enabled: Some(false),
            on_completion: Some(true),
            on_input_required: Some(true),
        };
        assert!(!should_notify(Some(&cfg), DesktopNotifyEvent::Completion));
    }

    #[test]
    fn enabled_defaults_to_both_event_classes() {
        let cfg = enabled_cfg();
        assert!(should_notify(Some(&cfg), DesktopNotifyEvent::Completion));
        assert!(should_notify(
            Some(&cfg),
            DesktopNotifyEvent::InputRequired(InputRequiredKind::Question),
        ));
    }

    #[test]
    fn per_event_toggles_are_honored() {
        let cfg = DesktopNotificationConfig {
            enabled: Some(true),
            on_completion: Some(false),
            on_input_required: Some(true),
        };
        assert!(!should_notify(Some(&cfg), DesktopNotifyEvent::Completion));
        assert!(should_notify(
            Some(&cfg),
            DesktopNotifyEvent::InputRequired(InputRequiredKind::Permission),
        ));
    }

    #[test]
    fn event_specs_are_stable() {
        let spec = spec_for(DesktopNotifyEvent::InputRequired(
            InputRequiredKind::PluginDialog,
        ));
        assert_eq!(spec.title, "Dirge needs input");
        assert!(spec.message.contains("plugin dialog"));
    }
}
