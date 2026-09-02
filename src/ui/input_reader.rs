//! Terminal input reader. Split out of `ui/mod.rs::run_interactive`
//! (dirge-4y4l stage 12a): a dedicated OS thread that polls crossterm for
//! key/mouse/paste/resize events and forwards them to the UI loop as
//! [`UserEvent`]s over an mpsc channel. Kept off the async runtime because
//! `event::read()` is blocking; cooperative shutdown via the terminal
//! module's `EVENT_READER_SHUTDOWN` / `EVENT_READER_EXITED` flags and the
//! `READER_GENERATION` counter.
//!
//! The thread is also where unsolicited terminal reports are stopped
//! ([`ReportFilter`]): crossterm cannot parse them, so left alone they
//! arrive as ordinary text and get typed into the compose box.

use std::time::{Duration, Instant};

use crossterm::event;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use smallvec::SmallVec;

use crate::event::UserEvent;

/// How long a gap between two key events ends a suspected terminal report
/// (dirge-v4xf). A real report arrives inside a single `read()` of the tty,
/// so its events land microseconds apart; the fastest human typing leaves
/// ~100ms between keys. 25ms sits clear of both, with slack for a report
/// split across two reads on a laggy link.
const REPORT_BURST_GAP: Duration = Duration::from_millis(25);

/// How many events one suspected report may swallow before the filter gives
/// up and releases them as ordinary keys. An OSC 52 clipboard reply is the
/// longest report seen in practice; this covers it while keeping a
/// mis-detected introducer from eating an unbounded amount of typing.
const REPORT_MAX_EVENTS: usize = 4096;

/// How many consecutive `event::poll` / `event::read` errors the reader
/// tolerates before giving up (dirge-sp1x). A single transient error used to
/// end the thread for the rest of the session, which reads to the user as a
/// dead keyboard on a UI that still paints.
const MAX_CONSECUTIVE_ERRORS: usize = 8;

/// Pause between retries after a poll/read error, so a persistently failing
/// source is abandoned in tens of milliseconds rather than spun on.
const ERROR_RETRY_PAUSE: Duration = Duration::from_millis(5);

/// Does `key` open an OSC / DCS / APC / SOS / PM sequence?
///
/// crossterm 0.29 has no parser for any of them. `parse_event` handles only
/// `ESC O`, `ESC [` and `ESC ESC`; every other byte after ESC falls through
/// to a recursive call that reports the introducer as an Alt-modified
/// character, and `Parser::advance` then clears its buffer — so the whole
/// payload is parsed byte-by-byte as ordinary text
/// (`src/event/sys/unix/parse.rs`, `src/event/source/unix/tty.rs`).
///
/// SHIFT is ignored because crossterm sets it for the uppercase introducers
/// (`P`, `X`). CONTROL must NOT be set: Windows reports AltGr as
/// Ctrl+Alt, so an AltGr-composed `]` on the Italian/German/Spanish layouts
/// arrives with both modifiers and is real typing, not an introducer
/// (`ui::input::normalize_altgr`, GH #659).
fn is_report_introducer(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']' | 'P' | '_' | 'X' | '^'))
}

/// BEL (`\x07`), which crossterm reports as Ctrl+G — one of the two
/// terminators a terminal may use for an OSC report.
fn is_report_bel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// ST (`ESC \`), the other terminator, which crossterm reports as Alt+`\`
/// when both bytes land in the same parse.
fn is_report_st(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('\\') && key.modifiers.contains(KeyModifiers::ALT)
}

/// Swallows unsolicited terminal reports before they reach the compose
/// editor as text (dirge-v4xf).
///
/// A terminal emits these on its own: OSC 10/11/12 colour reports on a theme
/// change or an alt-screen transition (kitty, ghostty, foot, iTerm2), an
/// OSC 52 clipboard reply, DCS XTGETTCAP / DECRQSS replies, kitty's OSC 99
/// notification reports — and in a multiplexer, a reply to a query some other
/// pane's program made. dirge drains that chatter at startup, at teardown and
/// around a suspended subprocess (`ui::terminal::sync_and_drain_via_sentinel`),
/// but nothing stood between it and the editor mid-session, so a report typed
/// its payload into the input box one character at a time.
///
/// The filter is deliberately conservative: a suspected report it cannot
/// close — no terminator, a human-scale gap, more events than
/// [`REPORT_MAX_EVENTS`] — is released as ordinary keys rather than dropped,
/// so the worst case for a real Alt+`]` keystroke is a delay of
/// [`REPORT_BURST_GAP`].
#[derive(Default)]
pub(crate) struct ReportFilter {
    /// The introducer plus every payload event captured so far. Empty when
    /// not inside a suspected report.
    pending: Vec<KeyEvent>,
    /// When the last event joined `pending` (or closed a report).
    last: Option<Instant>,
    /// A bare `Esc` closed a report: the `\` of an ST split across two
    /// parses may still be in flight and must not leak into the editor.
    expect_st_backslash: bool,
}

/// What one call to [`ReportFilter::feed`] hands back: nothing while a report
/// is being swallowed, one key for ordinary typing, and occasionally a run
/// being released. Inline capacity keeps the common path allocation-free —
/// this is the program's hottest input path.
type Forward = SmallVec<[KeyEvent; 4]>;

impl ReportFilter {
    /// Feed one key event; returns the events to forward, in order.
    pub(crate) fn feed(&mut self, key: KeyEvent, now: Instant) -> Forward {
        // The `\` right after the `Esc` that closed a report is the tail of a
        // split ST, not typing.
        if self.expect_st_backslash {
            self.expect_st_backslash = false;
            if self.within_burst(now) && key.code == KeyCode::Char('\\') {
                self.last = Some(now);
                return Forward::new();
            }
        }

        if self.pending.is_empty() {
            if is_report_introducer(&key) {
                self.pending.push(key);
                self.last = Some(now);
                return Forward::new();
            }
            let mut out = Forward::new();
            out.push(key);
            return out;
        }

        // Inside a suspected report.
        if !self.within_burst(now) {
            // Too slow to be one: release what we held, then treat this key as
            // fresh (it may open a report of its own). `release` empties
            // `pending`, so the recursion is one level deep.
            let mut out = self.release();
            out.extend(self.feed(key, now));
            return out;
        }
        if is_report_bel(&key) || is_report_st(&key) {
            tracing::debug!(
                events = self.pending.len() + 1,
                "swallowed an unsolicited terminal report"
            );
            self.pending.clear();
            self.last = Some(now);
            return Forward::new();
        }
        if key.code == KeyCode::Esc {
            // Possibly the first half of an ST that straddles two parses.
            tracing::debug!(
                events = self.pending.len() + 1,
                "swallowed an unsolicited terminal report (bare ESC terminator)"
            );
            self.pending.clear();
            self.last = Some(now);
            self.expect_st_backslash = true;
            return Forward::new();
        }
        if self.pending.len() >= REPORT_MAX_EVENTS {
            let mut out = self.release();
            out.push(key);
            return out;
        }
        self.pending.push(key);
        self.last = Some(now);
        Forward::new()
    }

    /// Release a suspected report that has gone quiet. Called from the
    /// reader's idle tick, so a real Alt+`]` reaches the editor after
    /// [`REPORT_BURST_GAP`] instead of waiting for the next keystroke.
    pub(crate) fn flush_stale(&mut self, now: Instant) -> Forward {
        if self.within_burst(now) {
            return Forward::new();
        }
        self.expect_st_backslash = false;
        self.release()
    }

    fn within_burst(&self, now: Instant) -> bool {
        self.last
            .is_some_and(|t| now.saturating_duration_since(t) < REPORT_BURST_GAP)
    }

    fn release(&mut self) -> Forward {
        self.last = None;
        if self.pending.is_empty() {
            return Forward::new();
        }
        tracing::debug!(
            events = self.pending.len(),
            "releasing a suspected terminal report as keystrokes"
        );
        Forward::from_vec(std::mem::take(&mut self.pending))
    }
}

/// Spawn the blocking crossterm reader thread. `user_tx` is consumed (pass
/// a clone — the caller keeps its own sender for other event sources). The
/// `JoinHandle` is stored in `READER_HANDLE` so the sandbox attach path
/// can fully join the thread before draining stdin.
///
/// Bumping `READER_GENERATION` here is what retires a previous reader
/// (dirge-xxo9): the shutdown flag alone could not, because it is cleared
/// again on resume and the loop never latched it, so a reader the suspend
/// path failed to join woke up to a `false` flag and kept reading fd 0
/// alongside its replacement — and alongside the stdin drains, which is how
/// a split escape sequence ends up typed into the compose box.
pub(crate) fn spawn_input_reader(user_tx: tokio::sync::mpsc::UnboundedSender<UserEvent>) {
    // Claim a generation before the thread starts, so any older reader sees
    // the new value on its very next tick.
    let generation = crate::ui::terminal::READER_GENERATION
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
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
        let mut filter = ReportFilter::default();
        let mut consecutive_errors = 0usize;
        'reader: loop {
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
                || crate::ui::terminal::READER_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
                    != generation
            {
                break;
            }
            // Poll with zero timeout — we own the 1ms wait ourselves
            // so crossterm only holds the thread for a few microseconds.
            // This shrinks the window where a dying tty can trap us
            // inside crossterm's internal read loop by ~1000×.
            match event::poll(std::time::Duration::ZERO) {
                Ok(true) => consecutive_errors = 0,
                Ok(false) => {
                    consecutive_errors = 0;
                    // Idle: a suspected report that has gone quiet is released
                    // here, so a real Alt+`]` reaches the editor after
                    // REPORT_BURST_GAP rather than on the next keystroke.
                    for key in filter.flush_stale(Instant::now()) {
                        if user_tx.send(UserEvent::Key(key)).is_err() {
                            break 'reader;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => {
                    // dirge-sp1x: a transient error must not end input for the
                    // session. The dead-tty probe above owns the case that
                    // genuinely cannot recover.
                    consecutive_errors += 1;
                    tracing::warn!(
                        error = %e,
                        consecutive = consecutive_errors,
                        "input reader: event::poll failed"
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        tracing::error!("input reader: giving up after repeated poll failures");
                        break;
                    }
                    std::thread::sleep(ERROR_RETRY_PAUSE);
                    continue;
                }
            }
            // Re-check the shutdown flag between poll and read.
            // poll() returning true means there are bytes on fd 0;
            // if shutdown was signalled during poll, we must not
            // consume those bytes — they belong to the drain pass.
            if crate::ui::terminal::EVENT_READER_SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
                || crate::ui::terminal::READER_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
                    != generation
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

                    // Unsolicited terminal reports (OSC / DCS / APC) reach us
                    // as an Alt-modified introducer plus their payload as plain
                    // text; the filter swallows those and passes everything else
                    // straight through (dirge-v4xf).
                    //
                    // With unbounded channel, sends never block — the only
                    // failure is a closed channel (UI loop exited).
                    for key in filter.feed(key, Instant::now()) {
                        if let Err(tokio::sync::mpsc::error::SendError(_)) =
                            user_tx.send(UserEvent::Key(key))
                        {
                            break 'reader;
                        }
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
                Err(e) => {
                    // dirge-sp1x: same retry policy as the poll error above.
                    consecutive_errors += 1;
                    tracing::warn!(
                        error = %e,
                        consecutive = consecutive_errors,
                        "input reader: event::read failed"
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        tracing::error!("input reader: giving up after repeated read failures");
                        break;
                    }
                    std::thread::sleep(ERROR_RETRY_PAUSE);
                    continue;
                }
                _ => {}
            }
            consecutive_errors = 0;
        }
        // Tell `TerminalGuard::drop` we've actually exited so it can
        // proceed past the wait barrier without sleeping on a
        // timeout. Release-store paired with the guard's
        // Acquire-load gives a clean happens-before relationship —
        // by the time the guard observes `true`, every byte this
        // thread consumed from crossterm's internal buffer is
        // visible to subsequent reads.
        //
        // Only the live reader may set it: a stale generation exiting because
        // it was retired must not tell the next suspend that the reader it
        // cares about is gone (dirge-xxo9).
        if crate::ui::terminal::READER_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
            == generation
        {
            crate::ui::terminal::EVENT_READER_EXITED
                .store(true, std::sync::atomic::Ordering::Release);
        }
    });
    // Store the handle so `join_reader` can wait for the thread to
    // actually exit — critical for the sandbox attach path where we
    // need to guarantee the reader is gone before draining stdin.
    if let Ok(mut guard) = crate::ui::terminal::READER_HANDLE.lock() {
        // Dropping a retired handle detaches that thread; the generation
        // check above has already told it to exit, so it is gone within a
        // tick and cleans itself up.
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

    // ── ReportFilter (dirge-v4xf) ───────────────────────────────────
    //
    // The events these feed are exactly what crossterm 0.29 produces for
    // the byte sequences named in each test — see `is_report_introducer`
    // for why, and `relay_tests::terminal_reports` for the same sequences
    // driven through a real PTY.

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn alt(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Feed a whole burst at one instant, returning everything forwarded.
    fn burst(f: &mut ReportFilter, at: Instant, keys: &[KeyEvent]) -> Vec<KeyEvent> {
        let mut out: Vec<KeyEvent> = Vec::new();
        for k in keys {
            out.extend(f.feed(*k, at));
        }
        out
    }

    /// `\x1b]11;rgb:2e2e/3434/3636\x07` — a background-colour report, the
    /// chatter kitty / ghostty / foot / iTerm2 emit unprompted.
    fn osc_11_report() -> Vec<KeyEvent> {
        let mut keys = vec![alt(']')];
        keys.extend("11;rgb:2e2e/3434/3636".chars().map(ch));
        keys.push(ctrl('g')); // BEL
        keys
    }

    #[test]
    fn osc_report_is_swallowed_whole() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        assert!(
            burst(&mut f, t0, &osc_11_report()).is_empty(),
            "an OSC report must not reach the editor"
        );
        // Typing right behind it is unaffected.
        assert_eq!(
            f.feed(ch('x'), t0 + Duration::from_millis(1)).as_slice(),
            [ch('x')]
        );
    }

    #[test]
    fn dcs_report_closed_by_st_is_swallowed() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        // `\x1bP1+r5463=787465726d\x1b\` — an XTGETTCAP reply.
        let mut keys = vec![key(
            KeyCode::Char('P'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        )];
        keys.extend("1+r5463=787465726d".chars().map(ch));
        keys.push(alt('\\')); // ST
        assert!(burst(&mut f, t0, &keys).is_empty());
    }

    #[test]
    fn st_split_across_two_parses_leaks_no_backslash() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        // The ESC of the ST landed at the end of a read, so crossterm
        // reported it as a bare Esc and the `\` came in the next parse.
        let mut keys = vec![alt(']')];
        keys.extend("52;c;SGVsbG8=".chars().map(ch));
        keys.push(key(KeyCode::Esc, KeyModifiers::NONE));
        keys.push(ch('\\'));
        assert!(burst(&mut f, t0, &keys).is_empty());
        assert_eq!(
            f.feed(ch('q'), t0 + Duration::from_millis(1)).as_slice(),
            [ch('q')]
        );
    }

    #[test]
    fn ordinary_typing_passes_through_untouched() {
        let mut f = ReportFilter::default();
        let mut t = Instant::now();
        for c in "hello".chars() {
            assert_eq!(f.feed(ch(c), t).as_slice(), [ch(c)]);
            t += Duration::from_millis(80);
        }
    }

    #[test]
    fn altgr_composed_bracket_is_not_an_introducer() {
        // Windows reports AltGr as Ctrl+Alt, and `]` needs AltGr on the
        // Italian / German / Spanish layouts (GH #659). It is typing.
        let mut f = ReportFilter::default();
        let k = key(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(f.feed(k, Instant::now()).as_slice(), [k]);
    }

    #[test]
    fn a_human_scale_gap_releases_the_introducer_and_the_next_key() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        assert!(f.feed(alt(']'), t0).is_empty());
        assert_eq!(
            f.feed(ch('a'), t0 + Duration::from_millis(100)).as_slice(),
            [alt(']'), ch('a')],
            "a deliberate Alt+] is delayed, never dropped"
        );
    }

    #[test]
    fn idle_flush_releases_a_lone_introducer() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        assert!(f.feed(alt(']'), t0).is_empty());
        assert!(
            f.flush_stale(t0 + Duration::from_millis(1)).is_empty(),
            "still inside the burst window"
        );
        assert_eq!(
            f.flush_stale(t0 + REPORT_BURST_GAP).as_slice(),
            [alt(']')],
            "the reader's idle tick releases it"
        );
        assert!(f.flush_stale(t0 + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn a_report_past_the_cap_is_released_not_dropped() {
        let mut f = ReportFilter::default();
        let t0 = Instant::now();
        assert!(f.feed(alt(']'), t0).is_empty());
        let mut released = Forward::new();
        for i in 0..=REPORT_MAX_EVENTS {
            released = f.feed(ch('z'), t0 + Duration::from_millis(1));
            if !released.is_empty() {
                // The introducer + everything held + the event that hit the
                // cap, in order.
                assert_eq!(released.len(), i + 2, "released at event {i}");
                break;
            }
        }
        assert!(
            !released.is_empty(),
            "the cap must release the run, not swallow forever"
        );
        assert_eq!(released[0], alt(']'));
    }
}
