use std::io;

use notify_rust::Notification;

use super::NotificationSpec;

pub(super) fn notify(spec: &NotificationSpec<'_>) -> io::Result<()> {
    Notification::new()
        .summary(spec.title)
        .body(spec.message)
        .show()
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}
