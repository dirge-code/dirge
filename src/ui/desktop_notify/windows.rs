use std::io;

use notify_rust::Notification;

use super::NotificationSpec;

pub(super) fn notify(spec: &NotificationSpec<'_>) -> io::Result<()> {
    // notify-rust's Windows backend raises a WinRT toast. Without a registered
    // AppUserModelID it is attributed to PowerShell (the crate's default sender),
    // mirroring the macOS "shows as Terminal" limitation; the toast still shows.
    Notification::new()
        .summary(spec.title)
        .body(spec.message)
        .show()
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}
