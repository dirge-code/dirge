use std::io::Write;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::Hide;
use crossterm::event::{
    EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, supports_keyboard_enhancement,
};

/// A handle to `/dev/tty` opened once by `TerminalGuard::new` and
/// read by `Renderer::new` so ratatui's backend writes directly to
/// the controlling terminal rather than to the process's stdout (fd
/// 1). With stdout redirected to the log file (see
/// `redirect_stdout_stderr_to_log` below), any code that writes to
/// stdout/stderr — Janet `(print …)`, `println!`, panic messages,
/// child-process inherited stdout, anything — lands in the log
/// instead of corrupting the TUI. This is the fd-level isolation
/// the user asked for: ratatui owns the screen, nothing else can
/// reach it.
pub(crate) static TTY_FD_PATH: OnceLock<bool> = OnceLock::new();

/// Optional log file path for the stdout/stderr fd redirect.
/// `None` means redirect to `/dev/null` (default — no log file is
/// created on disk). Set by `main.rs::set_log_path` before
/// `TerminalGuard::new` runs, based on `--verbose`, `RUST_LOG`, or
/// `DIRGE_LOG` opt-ins.
static LOG_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Publish the log destination for the fd redirect. Setting `None`
/// keeps the default (redirect to `/dev/null`); setting `Some(path)`
/// makes the fd target match what the tracing subscriber writes to.
/// First call wins (matches `tracing_subscriber::init` semantics).
pub fn set_log_path(path: Option<std::path::PathBuf>) {
    let _ = LOG_PATH.set(path);
}

/// Terminal reset for the signal reaper's emergency teardown: SGR
/// default, disable mouse + bracketed paste, clear title, leave the
/// alternate screen, show the cursor. Same modes `new` sets, in reverse
/// — matches the suspend path's sequence with a trailing cursor-show.
// `\x1b[<1u` pops any enhanced-keyboard (kitty) flags we may have pushed; a
// pop with an empty stack is a no-op and unsupported terminals ignore the
// unknown CSI, so it's safe to emit unconditionally on this path.
const PANIC_RESET_SEQ: &[u8] = b"\x1b[<1u\x1b[0m\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b]0;\x1b\\\x1b[?1049l\x1b[?25h";

/// Private-mode clear sequence shared across all teardown paths and the
/// entry defensive burst.  Emitted unconditionally on startup (to normalize
/// inherited state) and on every exit path (Drop, panic, signal-reaper,
/// suspend-for-subprocess).
// ponytail: emit unconditionally — pop/decrst on an empty/off mode is a
// no-op; unsupported terminals ignore unknown CSI.
const MODE_CLEAR: &[u8] = b"\x1b[<1u\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?2004l";

/// Where the default hook's panic backtrace actually landed. With fd 1/2
/// redirected to the log for the session, the message the default hook
/// prints to stderr goes to that file, not the screen — so point the
/// user at it.
fn log_path_hint() -> String {
    match LOG_PATH.get().and_then(|opt| opt.clone()) {
        Some(p) => p.display().to_string(),
        None => "stderr (run with --verbose or DIRGE_LOG to capture a log file)".to_string(),
    }
}

/// Build the on-tty panic notice. Pure (no I/O) so it can be tested; the
/// hook writes the returned string to /dev/tty after the terminal reset.
/// Every line carries `\r\n` — raw mode is off by the time this prints,
/// so a bare `\n` would stair-step across the cooked screen.
fn format_panic_notice(payload: &str, location: Option<&str>, log_hint: &str) -> String {
    let at = location.unwrap_or("unknown location");
    format!(
        "\r\n\x1b[1;31mdirge panicked:\x1b[0m {payload}\r\n  at {at}\r\n  full backtrace in the log: {log_hint}\r\n"
    )
}

/// Print the panic notice for a panic nobody survived (dirge-9ny9,
/// dirge-u9xv). Called from `TerminalGuard::drop` after the terminal is
/// restored and fd 1/2 are back, so the notice lands on a cooked screen
/// and on the user's real stderr.
///
/// The default hook writes its message and backtrace to stderr, which is
/// redirected to the log for the whole session, so without this a fatal
/// panic makes the TUI vanish with nothing shown and no hint where to
/// look. `crate::panic_report` holds what the hook saw.
///
/// Nothing prints when the record was already claimed: a panic somebody
/// caught and reported — the agent run's crash error, say — is that
/// party's to explain, and repeating it at exit would describe a
/// disaster the user already read about and recovered from.
fn report_unclaimed_panic() {
    let Some(notice) = pending_notice(crate::panic_report::take(), &log_path_hint()) else {
        return;
    };
    // /dev/tty first so the notice survives `2>/dev/null`; the restored
    // stderr is the fallback for a run with no controlling terminal.
    match open_tty_for_write() {
        Some(mut tty) => {
            let _ = tty.write_all(notice.as_bytes());
            let _ = tty.flush();
        }
        None => eprint!("{notice}"),
    }
}

/// What (if anything) teardown has to say about a panic. Pure, so the
/// "a claimed panic stays quiet" rule is testable without a terminal.
fn pending_notice(
    record: Option<crate::panic_report::PanicRecord>,
    log_hint: &str,
) -> Option<String> {
    record.map(|r| format_panic_notice(&r.message, r.location.as_deref(), log_hint))
}

/// Best-effort terminal restore for the signal reaper ([`crate::signal`]).
/// Write the reset/cursor-restore sequence to the controlling terminal and
/// leave raw mode, so a SIGTERM / SIGHUP / SIGINT teardown doesn't strand the
/// shell in raw mode with the alt screen still up — a signal exit skips
/// `TerminalGuard::drop`, which is what does this on every other path.
/// Harmless when there's no tty (headless `--print` / `--loop`).
///
/// Only the Unix signal reaper calls this today; off Unix there's no
/// signal-driven teardown, so allow it to be unused there.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn emergency_restore() {
    if let Some(mut tty) = open_tty_for_write() {
        let _ = tty.write_all(PANIC_RESET_SEQ);
        let _ = tty.flush();
    }
    let _ = terminal::disable_raw_mode();
}

/// Shared shutdown signal between the input-reader background thread
/// in `ui::mod` and `TerminalGuard::drop`. The reader polls this with
/// each `event::poll` tick; the guard sets it before tearing down so
/// the reader exits its loop cooperatively instead of dying mid-read
/// when the process unwinds. Without this flag the reader stays
/// blocked in `event::read()` while the guard's drain pass is also
/// holding crossterm's internal mutex — the two race for terminal-
/// response bytes (OSC 11, primary DA, CPR). Either path consumes
/// them, but the race is real and the outcome is timing-dependent.
pub(crate) static EVENT_READER_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Set by the input-reader background thread immediately before it
/// exits its loop. `TerminalGuard::drop` polls this so it can
/// proceed to the CPR-sync sentinel the moment the reader is gone,
/// rather than waiting on a hardcoded sleep that under-estimates
/// the worst case (reader stuck in `event::poll`) and over-estimates
/// the common case (reader exits within a few ms).
pub(crate) static EVENT_READER_EXITED: AtomicBool = AtomicBool::new(false);

/// Stored `JoinHandle` of the crossterm input-reader thread.
/// Set by `spawn_input_reader`, consumed by `join_reader`.
pub(crate) static READER_HANDLE: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

pub struct TerminalGuard {
    /// Original stdout (fd 1) saved before we redirected fd 1 to
    /// the log file. Restored on drop so the shell that spawned
    /// dirge gets its stdout back.
    #[cfg(unix)]
    saved_stdout_fd: Option<libc::c_int>,
    /// Original stderr (fd 2), same treatment.
    #[cfg(unix)]
    saved_stderr_fd: Option<libc::c_int>,
    /// True when we pushed the enhanced-keyboard (kitty) protocol flags at
    /// startup, so `drop` pops exactly what it pushed.
    kbd_flags_pushed: bool,
}

impl TerminalGuard {
    /// `keyboard_enhancement` opts into the terminal's enhanced keyboard
    /// (kitty) protocol so distinct chords like Shift+Enter reach the input
    /// editor. It's applied ONLY when the terminal advertises support
    /// (`supports_keyboard_enhancement`), so it's a safe no-op elsewhere.
    pub fn new(keyboard_enhancement: bool) -> std::io::Result<Self> {
        // Reset the flags in case the binary previously held a
        // guard in the same process (test harness, embedded use).
        EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // `main` already installed the recording hook; this is for
        // embedded use and tests, where `TerminalGuard::new` may be the
        // first thing that runs. Idempotent.
        crate::panic_report::install();

        // Open /dev/tty for all subsequent setup writes AND for
        // ratatui's backend to use later. If /dev/tty isn't
        // available (no controlling terminal — CI, pipe), fall back
        // to stdout; ratatui will too.
        let mut tty_writer: Box<dyn std::io::Write> = match open_tty_for_write() {
            Some(f) => Box::new(f),
            None => Box::new(std::io::stdout()),
        };
        tty_writer.write_all(MODE_CLEAR)?;
        tty_writer.execute(EnterAlternateScreen)?;
        tty_writer.execute(Clear(ClearType::All))?;
        // Bracketed paste lets the terminal deliver a multi-line paste as a
        // single Event::Paste, rather than a flood of keystroke events. The
        // input editor relies on this to compress long pastes into a
        // `[N lines pasted]` placeholder.
        tty_writer.execute(EnableBracketedPaste)?;
        // Capture mouse events so wheel scrolls reach the app (and we
        // scroll the output pane) instead of being absorbed by the
        // terminal to scroll its scrollback — which, under the alt
        // screen, would push the TUI off-view. Drag is captured too,
        // so native text selection requires the standard
        // bypass-modifier: Option/Alt+drag on macOS terminals, Shift
        // +drag on most Linux terminals.
        tty_writer.execute(EnableMouseCapture)?;
        // Focus reporting (`?1004h`): the terminal sends `\x1b[I` on
        // focus-in / `\x1b[O` on focus-out, which crossterm delivers as
        // FocusGained / FocusLost. dirge-ph60 uses FocusGained to
        // auto-recover the terminal modes — switching away from and back to
        // the window is the common moment the alt screen gets dropped, and
        // re-asserting on focus-in heals it without the manual Ctrl+L. The
        // teardown/suspend paths already emit `?1004l` to turn it back off.
        tty_writer.execute(EnableFocusChange)?;
        // Hide the hardware cursor by default. While the agent streams output,
        // the renderer issues many MoveTo calls and the visible cursor would
        // flicker across the screen. draw_bottom re-shows it only after
        // positioning it at the input prompt.
        tty_writer.execute(Hide)?;
        terminal::enable_raw_mode()?;
        // dirge: enable the terminal's enhanced keyboard (kitty) protocol so
        // distinct chords like Shift+Enter are reported instead of collapsing
        // onto plain Enter. Only pushed when the terminal actually supports it
        // (a no-op query elsewhere), and only DISAMBIGUATE_ESCAPE_CODES — the
        // mildest flag, which leaves ordinary text keys untouched and just
        // disambiguates the previously-ambiguous ones. The background event
        // reader already filters non-Press events, so no release-event spam.
        // `supports_keyboard_enhancement` needs raw mode (it reads the reply),
        // so it runs here, after `enable_raw_mode`.
        let kbd_flags_pushed = keyboard_enhancement
            && matches!(supports_keyboard_enhancement(), Ok(true))
            && tty_writer
                .execute(PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
                ))
                .is_ok();
        // Flush the setup writes to /dev/tty BEFORE redirecting fd 1.
        let _ = tty_writer.flush();
        drop(tty_writer);

        // === fd isolation ===
        // Redirect stdout (1) and stderr (2) to the dirge log file
        // for the duration of the TUI. Any code path that writes to
        // those fds (Janet code that escaped our :out redirect,
        // child processes inheriting stdout, panic messages, etc.)
        // lands in the log instead of corrupting the screen.
        //
        // ratatui itself writes via a fresh /dev/tty fd that the
        // Renderer opens via `open_tty_for_write` — independent of
        // the process's fd 1.
        #[cfg(unix)]
        let (saved_stdout_fd, saved_stderr_fd) = redirect_stdout_stderr_to_log();
        #[cfg(not(unix))]
        let _ = (); // non-unix builds don't get fd isolation yet

        // Mark that ratatui should use /dev/tty. The Renderer reads
        // this on construction to choose its backend writer.
        let _ = TTY_FD_PATH.set(true);

        #[cfg(unix)]
        return Ok(TerminalGuard {
            saved_stdout_fd,
            saved_stderr_fd,
            kbd_flags_pushed,
        });
        #[cfg(not(unix))]
        return Ok(TerminalGuard { kbd_flags_pushed });
    }
}

/// Open `/dev/tty` for write. Returns `None` when there's no
/// controlling terminal (CI, pipe, headless), in which case callers
/// should fall back to stdout — the user sees nothing useful
/// either way but at least we don't crash.
pub(crate) fn open_tty_for_write() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(false)
        .write(true)
        .open("/dev/tty")
        .ok()
}

/// Query the controlling terminal's size via `ioctl(/dev/tty,
/// TIOCGWINSZ)`. crossterm's own `terminal::size()` ioctls on fd 1,
/// which is now the log file — returns ENOTTY. We open /dev/tty
/// fresh each call (cheap; same fs operation that crossterm does
/// internally for `is_raw_mode_enabled`) and read winsize from it.
/// Falls back to (80, 24) on any error.
pub(crate) fn tty_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let f = match std::fs::OpenOptions::new()
            .read(true)
            .write(false)
            .open("/dev/tty")
        {
            Ok(f) => f,
            Err(_) => return (80, 24),
        };
        let fd = f.as_raw_fd();
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        if rc < 0 || ws.ws_col == 0 || ws.ws_row == 0 {
            return (80, 24);
        }
        (ws.ws_col, ws.ws_row)
    }
    #[cfg(not(unix))]
    {
        crossterm::terminal::size().unwrap_or((80, 24))
    }
}

/// dup2 fd 1 and fd 2 either to the dirge log file (when the user
/// opted in via `--verbose` / `RUST_LOG` / `DIRGE_LOG`) or to
/// `/dev/null` (default — silently discard stdout/stderr without
/// creating a log on disk). The redirect itself is mandatory for
/// TUI correctness; the destination is what's configurable.
/// Returns the saved originals so `Drop` can restore them.
#[cfg(unix)]
fn redirect_stdout_stderr_to_log() -> (Option<libc::c_int>, Option<libc::c_int>) {
    // Try the configured target first (a log file if the user
    // opted in, /dev/null otherwise). If that fails (read-only fs,
    // missing /dev/null on a weird container, etc.), force-fall
    // back to /dev/null — we MUST redirect somewhere, since
    // leaving fd 1/2 attached to the TTY would let stray writes
    // corrupt the ratatui screen.
    let configured = LOG_PATH
        .get()
        .and_then(|opt| opt.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&configured)
        .or_else(|_| std::fs::OpenOptions::new().write(true).open("/dev/null"));
    let file = match file {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    use std::os::fd::AsRawFd;
    let target_fd = file.as_raw_fd();
    // dup the originals so Drop can restore.
    let saved_stdout_fd = unsafe { libc::dup(1) };
    let saved_stderr_fd = unsafe { libc::dup(2) };
    // Redirect fds 1 and 2 to the chosen target.
    unsafe {
        libc::dup2(target_fd, 1);
        libc::dup2(target_fd, 2);
    }
    // Drop our handle — the duplicated fds in 1/2 keep the file alive.
    drop(file);
    (
        if saved_stdout_fd >= 0 {
            Some(saved_stdout_fd)
        } else {
            None
        },
        if saved_stderr_fd >= 0 {
            Some(saved_stderr_fd)
        } else {
            None
        },
    )
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Signal the background event-reader thread to exit. It
        // picks this up at the next `event::poll` tick (50ms) and
        // sets `EVENT_READER_EXITED` immediately before returning.
        // Wait on that flag (tight poll, 2ms granularity) so we
        // proceed to the CPR sync the moment the reader is gone —
        // not before (would race for stdin bytes) and not after
        // (would burn unnecessary shutdown time on a fast path).
        EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);

        wait_for_reader_exit(Duration::from_millis(50));
        // Cleanup writes go to /dev/tty, NOT stdout — fd 1 is still
        // redirected to the log file at this point. We restore
        // stdout/stderr AFTER the terminal reset escapes have been
        // emitted so the shell prompt that follows lands on a clean
        // screen.
        let mut tty_writer: Box<dyn std::io::Write> = match open_tty_for_write() {
            Some(f) => Box::new(f),
            None => Box::new(std::io::stdout()),
        };
        let stdout = &mut tty_writer;

        // === Phase 1: tell the terminal to stop reporting things ===
        // Pop the enhanced-keyboard (kitty) flags we pushed at startup, so the
        // shell that follows gets its plain key reporting back. `\x1b[<1u` is
        // the kitty "pop one entry" sequence.
        if self.kbd_flags_pushed {
            let _ = stdout.write_all(b"\x1b[<1u");
        }
        // Explicit DECRST for every mode we might have touched.
        // Mouse capture is enabled in `TerminalGuard::new` for wheel
        // scrolling — the DECRST sequences below take it back down.
        //   ?2004  — bracketed paste
        //   ?1049  — alternate screen (LeaveAlternateScreen)
        // PR #144 follow-up: reset the terminal tab/window title that
        // the `experimental-ui-terminal-tab` feature set. Empty OSC-0
        // releases the title back to the shell's default (most
        // terminals re-derive on the next prompt). ST terminator
        // (`\x1b\\`) matches the canonical xterm form and is tmux-
        // friendly. Emitting unconditionally is fine — terminals
        // that ignore OSC-0 also ignore the reset, and the cost is
        // 5 bytes on shutdown.
        let _ = stdout.write_all(
            b"\x1b[0m\
              \x1b[?25h\
              \x1b[<1u\x1b[?2026l\
              \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\
              \x1b[?2004l\
              \x1b]0;\x1b\\\
              \x1b[?1049l",
        );
        let _ = stdout.flush();

        // === Phase 2: synchronization sentinel ===
        // Some terminals (iTerm2 in particular) reply to alt-screen
        // exit with a flurry of unsolicited reports: OSC 11 bg-color
        // (`\x1b]11;rgb:…`), primary DA (`\x1b[?64;…c`), cursor
        // position (`\x1b[…R`). Drain-by-time is fragile because the
        // round-trip is unbounded (SSH, tmux nesting, slow VT) and
        // anything that arrives AFTER raw mode is disabled will be
        // re-interpreted by the shell's line discipline / readline
        // and become visible garbage at the prompt.
        //
        // Solution: SEND OUR OWN cursor-position query (DSR-CPR,
        // `\x1b[6n`). Terminals process queries in FIFO order, so
        // when we see our own CPR reply (`\x1b[<row>;<col>R`) on
        // stdin, every earlier reply (including the unsolicited
        // alt-screen-exit chatter) has also been delivered. Read
        // stdin until we see ANY `R`-terminated CSI; discard
        // everything along the way. Bounded timeout as a fallback
        // for very-slow / non-responsive terminals (raw write to
        // /dev/null or similar).
        #[cfg(unix)]
        sync_and_drain_via_sentinel(stdout, Duration::from_millis(100));

        // === Phase 3: tear down raw mode ===
        // By here the synchronization sentinel has fired and the
        // stdin buffer is empty. Disable raw mode and exit.
        let _ = terminal::disable_raw_mode();
        // Final cursor-show in cooked mode in case the shell's prompt
        // theme depended on it being visible.
        let _ = stdout.write_all(b"\x1b[?25h");
        let _ = stdout.flush();

        // Drop our TTY handle BEFORE restoring fd 1/2 so any
        // late-shutdown writes by other threads land in the log
        // (where they're harmless) until the very last moment when
        // fd 1/2 point at the real terminal again.
        drop(tty_writer);

        // === Phase 4: restore stdout/stderr ===
        #[cfg(unix)]
        unsafe {
            if let Some(orig) = self.saved_stdout_fd {
                libc::dup2(orig, 1);
                libc::close(orig);
            }
            if let Some(orig) = self.saved_stderr_fd {
                libc::dup2(orig, 2);
                libc::close(orig);
            }
        }

        // === Phase 5: say what killed us ===
        // This runs exactly when the process is actually tearing down,
        // which is the question the panic hook could not answer, and it
        // runs after the reset so the notice lands on a clean screen.
        report_unclaimed_panic();
    }
}

/// Block until the input-reader background thread sets
/// `EVENT_READER_EXITED`, or `budget` expires. Tight-poll (2ms
/// granularity) so the common case — reader exits within a few ms
/// of seeing the shutdown flag — incurs near-zero shutdown latency,
/// while the worst case (reader stuck somewhere in crossterm
/// internals, OS scheduling delay) is bounded.
pub(crate) fn wait_for_reader_exit(budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    while !EVENT_READER_EXITED.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Join the input-reader thread with a timeout budget.
///
/// Unlike `wait_for_reader_exit` which only polls the EXITED flag,
/// this takes the stored `JoinHandle` and actually blocks on
/// `thread::join`. If the thread hasn't exited within `budget`, the
/// handle is returned to storage and we fall back to the flag-only
/// guarantee. On success the handle is consumed so a new reader can
/// be spawned later.
///
/// Used by the sandbox attach path to guarantee the reader thread
/// has fully exited before draining stdin — closing the race window
/// where crossterm's internal `read()` consumes bytes that
/// `drain_stdin_nonblocking` should have captured.
#[cfg(unix)]
pub(crate) fn join_reader(budget: Duration) {
    let handle = match READER_HANDLE.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return,
    };
    let Some(handle) = handle else {
        return;
    };
    // Spawn a watchdog so we don't block forever if the reader is
    // stuck somewhere deep in crossterm that ignores the shutdown
    // flag (unlikely with the poll-based loop, but belts-and-suspenders).
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = std::sync::Arc::clone(&done);
    std::thread::spawn(move || {
        std::thread::sleep(budget);
        done2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    // Busy-wait join: check `is_finished` every 2ms so we can
    // observe the watchdog flag.
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Timeout expired. Return the handle to storage — the thread
    // is still running but EVENT_READER_EXITED is a lower-bound
    // guarantee once it finishes its current poll iteration.
    if let Ok(mut guard) = READER_HANDLE.lock() {
        *guard = Some(handle);
    }
}

/// Drain stdin without blocking. Sets O_NONBLOCK on fd 0, reads until
/// EAGAIN, restores original flags, and returns the drained bytes.
/// Used before sandbox attach to capture keystrokes typed during the
/// TUI suspend window so they can be injected into the PTY.
#[cfg(unix)]
pub(crate) fn drain_stdin_nonblocking() -> Vec<u8> {
    let fd_in: libc::c_int = 0;
    let original_flags = unsafe { libc::fcntl(fd_in, libc::F_GETFL) };
    if original_flags < 0 {
        return Vec::new();
    }
    let nb_flags = original_flags | libc::O_NONBLOCK;
    if unsafe { libc::fcntl(fd_in, libc::F_SETFL, nb_flags) } < 0 {
        return Vec::new();
    }

    let mut drained = Vec::with_capacity(256);
    let mut buf = [0u8; 1024];
    loop {
        let n = unsafe { libc::read(fd_in, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            drained.extend_from_slice(&buf[..n as usize]);
            continue;
        }
        if n == 0 {
            break;
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        match err {
            Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK => break,
            Some(libc::EINTR) => continue,
            _ => break,
        }
    }

    let _ = unsafe { libc::fcntl(fd_in, libc::F_SETFL, original_flags) };
    drained
}

/// Send a DSR-OS query (`\x1b[5n`) and read stdin until the
/// terminal's reply (`\x1b[0n`) appears, discarding every byte
/// along the way. Terminals process queries in FIFO order, so
/// seeing our DSR-OS reply guarantees every PRIOR reply
/// (alt-screen-exit chatter from iTerm2 / kitty / foot — OSC 11
/// bg-color, primary DA, AND iTerm2's own SPONTANEOUS CPR
/// `\x1b[…R`) has already been delivered and discarded by this
/// loop.
///
/// Why DSR-OS instead of CPR (`\x1b[6n`):
/// CPR replies are sent SPONTANEOUSLY by iTerm2 on alt-screen
/// transitions. A previous attempt used CPR as the sentinel; it
/// matched on the spontaneous reply, exited early, and let the
/// reply to OUR sentinel leak after raw mode flipped off. DSR-OS
/// (`\x1b[0n`) is essentially never sent unsolicited — its only
/// purpose is to reply to `\x1b[5n` ("are you OK?"). The exact
/// 4-byte reply `ESC [ 0 n` is uniquely tied to our query.
///
/// Bounded by `budget` as a fallback for terminals that don't
/// reply (rare; mostly headless / pipe contexts).
///
/// Callers should run this function BEFORE spawning the input reader.
/// Both read from fd 0 — if the reader is already active, they race.
#[cfg(unix)]
pub(crate) fn sync_and_drain_via_sentinel(stdout: &mut dyn std::io::Write, budget: Duration) {
    let fd_in: libc::c_int = 0; // stdin

    // Save the current stdin flags so we can restore blocking
    // semantics for the shell when we're done.
    let original_flags = unsafe { libc::fcntl(fd_in, libc::F_GETFL) };
    if original_flags < 0 {
        return;
    }
    let nb_flags = original_flags | libc::O_NONBLOCK;
    if unsafe { libc::fcntl(fd_in, libc::F_SETFL, nb_flags) } < 0 {
        return;
    }

    // Emit DSR-OS. If write fails (broken pipe, e.g. stdout
    // redirected), bail — we can't sync.
    if stdout.write_all(b"\x1b[5n").is_err() {
        let _ = unsafe { libc::fcntl(fd_in, libc::F_SETFL, original_flags) };
        return;
    }
    let _ = stdout.flush();

    // State machine matches the EXACT 4-byte reply `ESC [ 0 n`.
    // Any other escape sequence (OSC, CPR ending in `R`, DA1
    // ending in `c`, SS3) walks past without triggering — only
    // the `\x1b[0n` reply (which only our DSR-OS query elicits)
    // sets `got_reply`. A stray ESC mid-sequence restarts the
    // matcher so an unsolicited OSC can't desync us.
    let deadline = std::time::Instant::now() + budget;
    let mut buf = [0u8; 1024];
    // 0 = waiting for ESC, 1 = saw ESC, 2 = saw ESC[, 3 = saw ESC[0
    let mut match_state: u8 = 0;
    let mut got_reply = false;
    while !got_reply && std::time::Instant::now() < deadline {
        let n = unsafe { libc::read(fd_in, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            for &b in &buf[..n as usize] {
                match (match_state, b) {
                    (0, 0x1b) => match_state = 1,
                    (1, b'[') => match_state = 2,
                    (2, b'0') => match_state = 3,
                    (3, b'n') => {
                        got_reply = true;
                        break;
                    }
                    (_, 0x1b) => match_state = 1,
                    _ => match_state = 0,
                }
            }
            continue;
        }
        if n == 0 {
            break;
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        match err {
            Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK => {
                std::thread::sleep(Duration::from_millis(4));
            }
            Some(libc::EINTR) => continue,
            _ => break,
        }
    }

    // Restore blocking semantics for the shell.
    let _ = unsafe { libc::fcntl(fd_in, libc::F_SETFL, original_flags) };
}

/// Prepare the terminal for handing control to a subprocess attached to a PTY
/// (interactive shell command, sandbox attach). Stops the crossterm input
/// reader, drops out of the alternate screen, resets terminal modes, and
/// drains any keystrokes the user typed so they can be forwarded to the
/// subprocess.
///
/// Returns `Some(drained_stdin)` when `/dev/tty` is available — the TUI is now
/// suspended and the caller MUST pair this with
/// [`resume_tui_after_subprocess`]. Returns `None` when there is no
/// controlling terminal: the input reader is already restored in that case so
/// the caller may fall back to a non-interactive path.
#[cfg(unix)]
pub(crate) fn suspend_tui_for_subprocess(
    user_tx: &tokio::sync::mpsc::UnboundedSender<crate::event::UserEvent>,
) -> Option<Vec<u8>> {
    EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
    join_reader(Duration::from_millis(50));
    // If the reader hasn't exited yet, give it one more chance before
    // proceeding. The 1ms-poll reader should exit within ~2ms of the
    // shutdown flag, but crossterm internal buffers can delay it.
    if !EVENT_READER_EXITED.load(Ordering::Relaxed) {
        join_reader(Duration::from_millis(100));
        if !EVENT_READER_EXITED.load(Ordering::Relaxed) {
            eprintln!("[sandbox] input reader still running after 150ms — relay may race");
        }
    }

    let mut tty = match open_tty_for_write() {
        Some(t) => t,
        None => {
            EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
            EVENT_READER_EXITED.store(false, Ordering::Relaxed);
            crate::ui::input_reader::spawn_input_reader(user_tx.clone());
            return None;
        }
    };

    // Reset terminal: default colors, disable mouse + bracketed paste, clear
    // title, leave the alternate screen.
    let _ = tty.write_all(
        b"\x1b[0m\x1b[<1u\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b]0;\x1b\\\x1b[?1049l",
    );
    let _ = tty.flush();

    let drained_stdin = drain_stdin_nonblocking();

    let _ = tty.write_all(b"\x1b[?25h"); // show cursor for the subprocess
    let _ = tty.flush();

    Some(drained_stdin)
}

/// Counterpart to [`suspend_tui_for_subprocess`]: re-enters the alternate
/// screen, restores TUI modes, forces a repaint, syncs against the terminal,
/// and restarts the input reader.
#[cfg(unix)]
pub(crate) fn resume_tui_after_subprocess(
    renderer: &mut crate::ui::renderer::Renderer,
    user_tx: &tokio::sync::mpsc::UnboundedSender<crate::event::UserEvent>,
) {
    if let Some(mut tty) = open_tty_for_write() {
        // Re-enter alternate screen, clear, hide cursor, re-enable mouse +
        // bracketed paste + focus reporting (`?1004h`, dirge-ph60 — the
        // suspend path emitted `?1004l`, so re-arm it or FocusGained
        // recovery goes dark after any sandbox attach).
        let _ = tty.write_all(
            b"\x1b[?1049h\x1b[2J\x1b[?25l\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1004h",
        );
        let _ = tty.flush();
    }

    renderer.reset_tui();
    renderer.set_needs_repaint();

    if let Some(mut tty) = open_tty_for_write() {
        sync_and_drain_via_sentinel(&mut tty, Duration::from_millis(100));
    }

    EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
    EVENT_READER_EXITED.store(false, Ordering::Relaxed);
    crate::ui::input_reader::spawn_input_reader(user_tx.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_notice_carries_message_location_and_log() {
        let notice = format_panic_notice(
            "index out of bounds: the len is 0 but the index is 3",
            Some("src/ui/mod.rs:42:9"),
            "/home/x/.dirge/dirge.log",
        );
        assert!(notice.contains("dirge panicked"));
        assert!(notice.contains("index out of bounds"));
        assert!(notice.contains("src/ui/mod.rs:42:9"));
        assert!(notice.contains("/home/x/.dirge/dirge.log"));
        // Written to a cooked terminal after reset — every line needs a
        // carriage return or it stair-steps across the screen.
        assert!(notice.contains("\r\n"));
    }

    #[test]
    fn panic_notice_tolerates_unknown_location() {
        let notice = format_panic_notice("boom", None, "stderr");
        assert!(notice.contains("boom"));
        assert!(notice.contains("stderr"));
    }

    /// A panic somebody caught, reported and recovered from is theirs
    /// to explain. Repeating it at exit would describe a disaster the
    /// user already read about — and, worse, would do so on a run that
    /// went on to finish fine.
    #[test]
    fn a_claimed_panic_is_not_reported_again_at_exit() {
        assert_eq!(pending_notice(None, "/tmp/dirge.log"), None);
    }

    #[test]
    fn a_panic_nobody_survived_is_named_at_exit() {
        let notice = pending_notice(
            Some(crate::panic_report::PanicRecord {
                message: "called `Option::unwrap()` on a `None` value".to_string(),
                location: Some("src/ui/mod.rs:42:9".to_string()),
            }),
            "/home/x/.dirge/dirge.log",
        )
        .expect("an unclaimed panic is the one nobody explained");
        assert!(notice.contains("called `Option::unwrap()`"));
        assert!(notice.contains("src/ui/mod.rs:42:9"));
        assert!(notice.contains("/home/x/.dirge/dirge.log"));
    }

    /// dirge-u9xv: the hook must not touch a terminal the process is
    /// still using. It used to decide by asking whether the panicking
    /// thread owned the terminal, which under
    /// `#[tokio::main(flavor = "current_thread")]` is true for every
    /// agent-task panic — the task runs on the UI thread, so a caught,
    /// survivable panic took the fatal branch. The decision moved
    /// wholesale to `Drop`, which runs only when the process really is
    /// going down; nothing here may reset, leave raw mode, or latch a
    /// flag that makes `Drop` skip its own reset.
    #[test]
    fn this_module_installs_no_panic_hook_of_its_own() {
        // Only the production half — this test names the very things it
        // forbids, so scanning the whole file would fail on itself.
        let production = include_str!("terminal.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields a first piece");
        assert!(
            !production.contains("std::panic::set_hook"),
            "recording a panic belongs to panic_report; acting on one belongs to Drop"
        );
        for gone in ["PANIC_HOOK_FIRED", "UI_THREAD_ID", "thread_owns_terminal"] {
            assert!(
                !production.contains(gone),
                "{gone} predicted whether the process was dying — the prediction was the bug"
            );
        }
        // The check is only worth having if the file it reads is this
        // one; a stale include! would pass vacuously forever.
        assert!(
            production.contains("fn report_unclaimed_panic"),
            "scanned the wrong source"
        );
    }

    #[test]
    fn reset_seqs_include_kitty_pop_and_sync_off() {
        let panic_s = std::str::from_utf8(PANIC_RESET_SEQ).unwrap();
        assert!(
            panic_s.contains("[<1u"),
            "PANIC_RESET_SEQ must pop kitty keyboard stack"
        );
        assert!(
            panic_s.contains("[?2026l"),
            "PANIC_RESET_SEQ must disable sync output"
        );

        let mode_s = std::str::from_utf8(MODE_CLEAR).unwrap();
        assert!(
            mode_s.contains("[<1u"),
            "MODE_CLEAR must pop kitty keyboard stack"
        );
        assert!(
            mode_s.contains("[?2026l"),
            "MODE_CLEAR must disable sync output"
        );
    }
}
