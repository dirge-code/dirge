//! Terminal input reader. Split out of `ui/mod.rs::run_interactive`
//! (dirge-4y4l stage 12a): a dedicated OS thread that polls crossterm for
//! key/mouse/paste/resize events and forwards them to the UI loop as
//! [`UserEvent`]s over an mpsc channel. Kept off the async runtime because
//! `event::read()` is blocking; cooperative shutdown via the terminal
//! module's `EVENT_READER_SHUTDOWN` / `EVENT_READER_EXITED` flags.

use crossterm::event;
use crossterm::event::{MouseButton, MouseEventKind};

use crate::event::UserEvent;

/// Spawn the blocking crossterm reader thread. `user_tx` is consumed (pass
/// a clone — the caller keeps its own sender for other event sources). The
/// `JoinHandle` is stored in `READER_HANDLE` so the sandbox attach path
/// can fully join the thread before draining stdin.
pub(crate) fn spawn_input_reader(user_tx: tokio::sync::mpsc::UnboundedSender<UserEvent>) {
    let handle = std::thread::spawn(move || {
        // ── CFS priority boost for the input reader ──────────────
        // nice -20 gives ~5900x scheduling weight over KVM (nice 19)
        // threads. Works without CAP_SYS_NICE on kernels with
        // default RLIMIT_NICE (allows 0 to -20 for unprivileged).
        #[cfg(unix)]
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, -20);
        }

        // ── Dead-tty guard (dirge-jiiv) ─────────────────────────
        // Mirror crossterm's internal fd selection: if stdin is a
        // tty use fd 0; otherwise open /dev/tty. Probe this fd
        // before each call to event::poll because crossterm's
        // internal read loop never returns on EOF/EIO — once control
        // enters poll() it may never come back.
        #[cfg(unix)]
        let probe_fd: Option<(std::os::unix::io::RawFd, Option<std::fs::File>)> = {
            if unsafe { libc::isatty(0) } == 1 {
                Some((0, None))
            } else {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")
                {
                    Ok(f) => {
                        use std::os::unix::io::AsRawFd;
                        let raw = f.as_raw_fd();
                        Some((raw, Some(f)))
                    }
                    Err(_) => None,
                }
            }
        };

        // ── Dead-tty watchdog (dirge-jiiv) ─────────────────────
        // crossterm 0.29's event::poll may never return once the
        // terminal dies (upstream bug crossterm-rs/crossterm#793).
        // The reader loop probes tty_is_dead before each poll call,
        // but if the terminal dies DURING poll, the thread is trapped
        // forever — the probe never runs again. This watchdog supplies
        // the SIGHUP that an orphaned background process never receives:
        // when the tty goes away, it performs the same emergency
        // teardown as src/signal.rs. Skipped in headless modes
        // (--print, MCP server) where there is no controlling terminal
        // to lose and we must never self-exit.
        #[cfg(unix)]
        {
            use std::sync::Once;
            static WATCHDOG_STARTED: Once = Once::new();
            if probe_fd.is_some() {
                WATCHDOG_STARTED.call_once(|| {
                    // Open an independent handle — the watchdog must not
                    // share or close the reader's descriptor.
                    let watchdog_tty = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/tty")
                        .ok();
                    if let Some(tty) = watchdog_tty {
                        std::thread::spawn(move || {
                            use std::os::unix::io::AsRawFd;
                            // `tty` must be MOVED into the thread and kept
                            // alive here. Binding it outside and passing only
                            // the RawFd would drop the File when this scope
                            // ends, closing the descriptor; the ioctl probe in
                            // tty_is_dead would then fail EBADF and the
                            // watchdog would exit the process moments after
                            // startup.
                            let fd = tty.as_raw_fd();
                            loop {
                                std::thread::sleep(std::time::Duration::from_millis(250));
                                if tty_is_dead(fd).unwrap_or(false) {
                                    // Mirror signal.rs SIGHUP teardown exactly.
                                    crate::child_guard::reap_all_groups();
                                    crate::ui::terminal::emergency_restore();
                                    std::process::exit(128 + libc::SIGHUP);
                                }
                            }
                        });
                    }
                });
            }
        }

        // Poll-based loop so `TerminalGuard::drop` can signal a
        // cooperative shutdown via `EVENT_READER_SHUTDOWN`. Previously
        // this thread blocked in `event::read()` indefinitely; on
        // teardown the guard's drain pass and this `read()` both held
        // crossterm's internal mutex, racing for terminal-response
        // bytes (OSC 11, primary DA, CPR). With the flag + 50ms
        // poll-tick, the reader exits within ~50ms of the guard
        // signalling, the mutex is released, and the drain runs
        // uncontended.
        loop {
            // Probe for a dead tty before calling event::poll.
            // crossterm's internal read loop never returns on
            // EOF/EIO, so once we enter poll() we may never come
            // back. A dead fd reports POLLHUP|POLLERR|POLLNVAL.
            #[cfg(unix)]
            if let Some((fd, _guard)) = &probe_fd
                && tty_is_dead(*fd).unwrap_or(false)
            {
                break;
            }
            if crate::ui::terminal::EVENT_READER_SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            // Poll with zero timeout — we own the 1ms wait ourselves
            // so crossterm only holds the thread for a few microseconds.
            // This shrinks the window where a dying tty can trap us
            // inside crossterm's internal read loop by ~1000×.
            match event::poll(std::time::Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                Err(_) => break,
            }
            // Re-check the shutdown flag between poll and read.
            // poll() returning true means there are bytes on fd 0;
            // if shutdown was signalled during poll, we must not
            // consume those bytes — they belong to the drain pass.
            if crate::ui::terminal::EVENT_READER_SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            // `clippy::collapsible_match` suggests moving the `is_err()` check into
            // a match guard, but doing so tries to move bound values (e.g. `text`
            // in `Event::Paste(text)`) inside the guard, which is rejected with
            // E0507. Keep the nested `if`s.
            #[allow(clippy::collapsible_match)]
            match event::read() {
                Ok(event::Event::Key(key)) => {
                    // Filter Release / Repeat events. Modern terminals
                    // (kitty keyboard protocol, Windows 10+ ConPTY,
                    // some iTerm2 modes) emit BOTH Press and Release
                    // for every keystroke — without this filter every
                    // typed char inserts twice ("ssuubb..." bug).
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }

                    // With unbounded channel, sends never block — the only
                    // failure is a closed channel (UI loop exited).
                    if let Err(tokio::sync::mpsc::error::SendError(_)) =
                        user_tx.send(UserEvent::Key(key))
                    {
                        break;
                    }
                }
                Ok(event::Event::Mouse(m)) => {
                    // Wheel → scroll the output pane. Left button
                    // down/drag/up → app-level text selection
                    // (`ui::selection::handle`). Other buttons are
                    // ignored. Right/middle clicks fall through with
                    // no app action and the terminal's own handling
                    // for them takes over (paste, menu, etc.).
                    let ev = match m.kind {
                        MouseEventKind::ScrollUp => Some(UserEvent::ScrollUp {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::ScrollDown => Some(UserEvent::ScrollDown {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Down(MouseButton::Left) => Some(UserEvent::MouseDown {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Drag(MouseButton::Left) => Some(UserEvent::MouseDrag {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Up(MouseButton::Left) => Some(UserEvent::MouseUp {
                            row: m.row,
                            col: m.column,
                        }),
                        _ => None,
                    };
                    if let Some(ev) = ev
                        && let Err(tokio::sync::mpsc::error::SendError(_)) = user_tx.send(ev)
                    {
                        break;
                    }
                }
                Ok(event::Event::Paste(text)) => {
                    if let Err(tokio::sync::mpsc::error::SendError(_)) =
                        user_tx.send(UserEvent::Paste(text))
                    {
                        break;
                    }
                }
                Ok(event::Event::Resize(cols, rows)) => {
                    if let Err(tokio::sync::mpsc::error::SendError(_)) =
                        user_tx.send(UserEvent::Resize(cols, rows))
                    {
                        break;
                    }
                }
                // dirge-ph60: window regained focus. Requires focus
                // reporting (`?1004h`) enabled at startup. The loop treats
                // this as a cue to re-assert the terminal modes — refocusing
                // is the common moment the alt screen gets dropped. FocusLost
                // needs no action, so it falls through to the catch-all.
                Ok(event::Event::FocusGained) => {
                    if let Err(tokio::sync::mpsc::error::SendError(_)) =
                        user_tx.send(UserEvent::FocusGained)
                    {
                        break;
                    }
                }
                Err(_) => break,
                _ => {}
            }
        }
        // Tell `TerminalGuard::drop` we've actually exited so it can
        // proceed past the wait barrier without sleeping on a
        // timeout. Release-store paired with the guard's
        // Acquire-load gives a clean happens-before relationship —
        // by the time the guard observes `true`, every byte this
        // thread consumed from crossterm's internal buffer is
        // visible to subsequent reads.
        crate::ui::terminal::EVENT_READER_EXITED.store(true, std::sync::atomic::Ordering::Release);
    });
    // Store the handle so `join_reader` can wait for the thread to
    // actually exit — critical for the sandbox attach path where we
    // need to guarantee the reader is gone before draining stdin.
    if let Ok(mut guard) = crate::ui::terminal::READER_HANDLE.lock() {
        *guard = Some(handle);
    }
}

/// Death probe for a tty fd. Two independent checks:
///
/// 1. `poll(2)` for POLLHUP/POLLERR — catches a dead pty slave (the
///    primary side closed). POLLNVAL is deliberately NOT treated as
///    death: on macOS `/dev/tty` is the controlling-terminal redirect
///    device and ALWAYS reports POLLNVAL to poll(2), even on a healthy
///    terminal — treating it as fatal made the dead-tty watchdog kill
///    the process ~250ms after every startup (exit 128+SIGHUP).
/// 2. `ioctl(TIOCGWINSZ)` — on a hung-up terminal the line discipline
///    is gone and the ioctl fails with EIO. This covers the /dev/tty
///    case where poll can't see the hangup, and costs one syscall.
#[cfg(unix)]
pub(crate) fn tty_is_dead(fd: std::os::unix::io::RawFd) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        return Ok(true);
    }
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    Ok(rc < 0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    fn open_pty_pair() -> (std::fs::File, std::fs::File) {
        let primary_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(primary_fd >= 0, "posix_openpt failed");
        assert_eq!(unsafe { libc::grantpt(primary_fd) }, 0, "grantpt failed");
        assert_eq!(unsafe { libc::unlockpt(primary_fd) }, 0, "unlockpt failed");
        let secondary_name = unsafe { libc::ptsname(primary_fd) };
        assert!(!secondary_name.is_null(), "ptsname returned null");
        let secondary_fd = unsafe { libc::open(secondary_name, libc::O_RDWR | libc::O_NOCTTY) };
        assert!(secondary_fd >= 0, "open secondary failed");
        let primary = unsafe { std::fs::File::from_raw_fd(primary_fd) };
        let secondary = unsafe { std::fs::File::from_raw_fd(secondary_fd) };
        (primary, secondary)
    }

    #[test]
    fn tty_is_dead_false_for_live_pty() {
        let (primary, secondary) = open_pty_pair();
        let secondary_fd = secondary.as_raw_fd();
        assert!(
            !tty_is_dead(secondary_fd).expect("poll failed"),
            "secondary should be alive while primary is open"
        );
        drop(primary);
        drop(secondary);
    }

    #[test]
    fn tty_is_dead_true_after_peer_close() {
        let (primary, secondary) = open_pty_pair();
        let secondary_fd = secondary.as_raw_fd();
        drop(primary);
        assert!(
            tty_is_dead(secondary_fd).expect("poll failed"),
            "secondary should be dead after primary closes"
        );
        drop(secondary);
    }

    /// Regression: on macOS, poll(2) on /dev/tty (the controlling-
    /// terminal redirect device) ALWAYS reports POLLNVAL, even on a
    /// healthy tty — the watchdog read that as death and killed the
    /// process ~250ms after startup (exit 128+SIGHUP). A live /dev/tty
    /// must report NOT dead.
    ///
    /// dirge-u35k: this used to open `/dev/tty` directly and `return` when
    /// that failed, which is exactly what happens on a CI runner — so the
    /// one test guarding a bug that shipped in TWO releases was a silent
    /// no-op everywhere it mattered. It cannot be converted to the pty
    /// helper above either: the bug is specific to `/dev/tty`, the
    /// controlling-terminal REDIRECT device, and a pty secondary is a
    /// different device that polls normally.
    ///
    /// So build the missing precondition instead. A forked child calls
    /// `setsid` to leave the test's session, claims the pty secondary as
    /// its controlling terminal with `TIOCSCTTY`, and only then does
    /// `/dev/tty` resolve — to our pty. The child reports through its exit
    /// status because that is the only channel that needs no allocation.
    ///
    /// The child touches nothing but raw syscalls before `_exit`, which is
    /// the rule for a forked child in a process that may have threads.
    #[test]
    fn a_live_controlling_tty_is_not_dead() {
        const DEAD: i32 = 1;
        const NO_SETSID: i32 = 10;
        const NO_CTTY: i32 = 11;
        const NO_DEV_TTY: i32 = 12;
        const PROBE_ERR: i32 = 13;

        let (primary, secondary) = open_pty_pair();
        let secondary_fd = secondary.as_raw_fd();

        match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => unsafe {
                // CHILD. New session, so we hold no controlling terminal…
                if libc::setsid() < 0 {
                    libc::_exit(NO_SETSID);
                }
                // …then adopt the pty secondary as one.
                if libc::ioctl(secondary_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    libc::_exit(NO_CTTY);
                }
                let tty = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR);
                if tty < 0 {
                    libc::_exit(NO_DEV_TTY);
                }
                match tty_is_dead(tty) {
                    Ok(false) => libc::_exit(0),
                    Ok(true) => libc::_exit(DEAD),
                    Err(_) => libc::_exit(PROBE_ERR),
                }
            },
            child => {
                let mut status: libc::c_int = 0;
                let waited = unsafe { libc::waitpid(child, &mut status, 0) };
                assert_eq!(waited, child, "waitpid failed");
                assert!(
                    libc::WIFEXITED(status),
                    "child did not exit normally (status {status})"
                );
                let code = libc::WEXITSTATUS(status);
                let explain = match code {
                    DEAD => {
                        "a LIVE /dev/tty reported dead — this is the macOS \
                             POLLNVAL false positive that killed every session \
                             ~250ms after startup in 0.19.24 and 0.19.25"
                    }
                    NO_SETSID => "setsid failed in the child",
                    NO_CTTY => "could not claim the pty as a controlling terminal",
                    NO_DEV_TTY => "/dev/tty did not open even with a controlling terminal",
                    PROBE_ERR => "tty_is_dead returned an error",
                    _ => "unexpected child exit",
                };
                assert_eq!(code, 0, "{explain} (exit {code})");
            }
        }
        drop(primary);
        drop(secondary);
    }
}
