use std::io;

use super::NotificationSpec;

pub(super) fn notify(_spec: &NotificationSpec<'_>) -> io::Result<()> {
    // Backend reserved for a follow-up implementation, likely via PowerShell toast.
    Ok(())
}
