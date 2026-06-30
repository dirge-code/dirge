use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::Hide;
use crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen};

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
}

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        // Reset both flags in case the binary previously held a
        // guard in the same process (test harness, embedded use).
        EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // Open /dev/tty for all subsequent setup writes AND for
        // ratatui's backend to use later. If /dev/tty isn't
        // available (no controlling terminal — CI, pipe), fall back
        // to stdout; ratatui will too.
        let mut tty_writer: Box<dyn std::io::Write> = match open_tty_for_write() {
            Some(f) => Box::new(f),
            None => Box::new(std::io::stdout()),
        };
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
        // Hide the hardware cursor by default. While the agent streams output,
        // the renderer issues many MoveTo calls and the visible cursor would
        // flicker across the screen. draw_bottom re-shows it only after
        // positioning it at the input prompt.
        tty_writer.execute(Hide)?;
        terminal::enable_raw_mode()?;
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
        });
        #[cfg(not(unix))]
        return Ok(TerminalGuard {});
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
        b"\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b]0;\x1b\\\x1b[?1049l",
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
        // bracketed paste.
        let _ = tty.write_all(
            b"\x1b[?1049h\x1b[2J\x1b[?25l\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h",
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
