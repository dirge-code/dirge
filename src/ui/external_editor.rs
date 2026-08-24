//! Hand a block of text to `$EDITOR` and read back what the user saved.
//!
//! Extracted from `Input::open_in_external_editor` so `/edit` and the memory
//! review share one implementation. The tricky parts are all here:
//!
//! * The temp file is created with `create_new` (`O_EXCL`), so a symlink
//!   pre-planted at the predictable path fails the open instead of being
//!   written through.
//! * dirge points fds 1/2 at its log file for the TUI session. They are
//!   redirected to `/dev/tty` for the child's lifetime and restored after,
//!   or the editor draws into the log.
//! * The path is passed as a positional arg via `"$@"` rather than
//!   interpolated, so spaces and metacharacters in it cannot break out.
//!   `$EDITOR` itself still word-splits, which is what makes
//!   `EDITOR="code --wait"` work.
//!
//! The caller MUST suspend the TUI first — see
//! [`suspend_tui_for_subprocess`](crate::ui::terminal::suspend_tui_for_subprocess).

#[cfg(unix)]
use std::io::Write;

/// Open `$EDITOR` on `seed` and return the saved contents.
///
/// `tag` distinguishes concurrent temp files and gives the editor a useful
/// filename to syntax-highlight from (e.g. `"input"`, `"memory-review"`).
///
/// Returns `None` if the editor could not be spawned or exited non-zero —
/// which is the deliberate way to abort (`:cq` in vim). Callers must treat
/// `None` as "change nothing"; a failed edit must never be read as an empty
/// document, or aborting would silently discard the caller's data. Errors
/// are reported to the user before returning.
#[cfg(unix)]
pub(crate) fn edit_text(seed: &str, tag: &str) -> Option<String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let path = std::env::temp_dir().join(format!("dirge-{tag}-{}.md", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(seed.as_bytes()));
    if let Err(e) = write_result {
        crate::ui::notifications::notify_send(crate::ui::notifications::Notification::Error(
            format!("External editor: failed to write temp file: {e}"),
        ));
        return None;
    }

    let saved: Option<(i32, i32, i32)> = unsafe {
        let tty = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR);
        if tty < 0 {
            None
        } else {
            let so = libc::dup(1);
            let se = libc::dup(2);
            libc::dup2(tty, 1);
            libc::dup2(tty, 2);
            Some((tty, so, se))
        }
    };

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor) // $0
        .arg(&path) // $1 → "$@"
        .status();

    if let Some((tty, so, se)) = saved {
        unsafe {
            libc::dup2(so, 1);
            libc::dup2(se, 2);
            libc::close(so);
            libc::close(se);
            libc::close(tty);
        }
    }

    let result = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).ok(),
        Ok(s) => {
            crate::ui::notifications::notify_send(crate::ui::notifications::Notification::Error(
                format!(
                    "External editor exited with: {}",
                    s.code()
                        .map(|c| format!("code {c}"))
                        .unwrap_or_else(|| "signal".into())
                ),
            ));
            None
        }
        Err(e) => {
            crate::ui::notifications::notify_send(crate::ui::notifications::Notification::Error(
                format!("External editor: failed to spawn {editor}: {e}"),
            ));
            None
        }
    };

    let _ = std::fs::remove_file(&path);
    result
}
