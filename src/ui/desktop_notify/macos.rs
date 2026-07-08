use std::io;
use std::sync::Once;

use notify_rust::{Notification, set_application};

use super::NotificationSpec;

static SET_APPLICATION: Once = Once::new();
const NOTIFICATION_SENDER_BUNDLE: &str = "com.apple.Terminal";

pub(super) fn notify(spec: &NotificationSpec<'_>) -> io::Result<()> {
    prime_notification_sender();

    Notification::new()
        .summary(spec.title)
        .body(spec.message)
        .show()
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}

fn prime_notification_sender() {
    SET_APPLICATION.call_once(|| {
        // The default mac-notification-sys app lookup can resolve to Finder and
        // fail for CLI runs. Terminal is the backend's stable fallback sender.
        let _ = set_application(NOTIFICATION_SENDER_BUNDLE);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_terminal_as_macos_sender() {
        assert_eq!(NOTIFICATION_SENDER_BUNDLE, "com.apple.Terminal");
    }
}
