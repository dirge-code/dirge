//! `dirge.harness` output in the TUI: `notify` as a chat-area line.

use crate::ui::notifications::{Notification, notify_send};

use super::port::{HarnessSink, Level};

pub struct TuiSink;

impl HarnessSink for TuiSink {
    fn notify(&self, level: Level, message: &str) {
        let line = format!("[addon] {message}");
        notify_send(match level {
            Level::Info => Notification::Info(line),
            Level::Warn => Notification::Warn(line),
            Level::Error => Notification::Error(line),
        });
    }
}
