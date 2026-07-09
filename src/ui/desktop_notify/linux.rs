use std::io;

use notify_rust::Notification;

use super::NotificationSpec;

pub(super) fn notify(spec: &NotificationSpec<'_>) -> io::Result<()> {
    // notify-rust's `z` (zbus) feature speaks the freedesktop Notifications
    // D-Bus protocol directly, so this reaches any running notification daemon
    // without a system libnotify/libdbus dependency. A missing daemon (e.g. a
    // headless session) surfaces as an `Err`, which the caller logs and drops.
    Notification::new()
        .summary(spec.title)
        .body(spec.message)
        .show()
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}
