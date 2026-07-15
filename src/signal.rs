//! Signal-driven emergency teardown of detached child processes.
//!
//! dirge spawns its long-lived children — LSP servers (rust-analyzer &c.),
//! MCP servers, DAP adapters, bash subtrees — into their own sessions via
//! `setsid` ([`crate::child_guard::detach_session`]). That gives them no
//! controlling terminal, which protects the TUI but also means a signal the
//! terminal delivers (SIGHUP when the window/tab closes, SIGINT/SIGQUIT to
//! the foreground process group) never reaches them. The only thing that
//! reaps such a child is an explicit `kill(-pgid)`, and dirge issues that
//! from each guard's `Drop`.
//!
//! `Drop` runs on a normal return or an unwinding panic — but NOT when the
//! process is itself killed by a signal. So a plain `kill <pid>` (SIGTERM),
//! a closed terminal (SIGHUP), or a `kill -INT` left rust-analyzer orphaned,
//! still indexing the workspace and holding a gigabyte of RAM (dirge-6klk).
//!
//! This module installs an async task that awaits those signals, reaps every
//! registered child group via [`crate::child_guard::reap_all_groups`],
//! restores the terminal, and exits with the conventional `128 + signum`
//! status. It complements — does not replace — the per-guard `Drop`, which
//! still handles every normal exit and sends the graceful LSP shutdown.

/// Spawn the signal reaper. Call once, early in `main`, inside the tokio
/// runtime and before any child is spawned. No-op off Unix, where children
/// aren't `setsid`-detached and `kill_on_drop` is the whole story.
#[cfg(unix)]
pub fn install_reaper() {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        // Registering a stream for each signal installs a handler that
        // supersedes the default "terminate immediately" disposition, so we
        // get a chance to clean up. If any registration fails we bail and
        // leave the defaults in place rather than half-installing.
        let (mut sigint, mut sigterm, mut sighup) = match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(i), Ok(t), Ok(h)) => (i, t, h),
            _ => return,
        };

        let signum = tokio::select! {
            _ = sigint.recv() => libc::SIGINT,
            _ = sigterm.recv() => libc::SIGTERM,
            _ = sighup.recv() => libc::SIGHUP,
        };

        // Reap the detached child groups first — their guards' Drop won't
        // run under a signal exit, so this is the only thing that stops
        // rust-analyzer & friends from being orphaned.
        crate::child_guard::reap_all_groups();
        // Then leave the terminal in a usable state (best effort).
        crate::ui::terminal::emergency_restore();
        // 128 + signum is the shell convention for a signal-terminated
        // process (SIGINT → 130, SIGTERM → 143, SIGHUP → 129).
        std::process::exit(128 + signum);
    });
}

#[cfg(not(unix))]
pub fn install_reaper() {}
