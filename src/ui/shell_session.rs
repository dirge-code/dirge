//! PTY-backed interactive shell session for `!cmd` / `!!cmd`.
//!
//! The command runs on its own **pseudo-terminal** so child programs see a
//! real TTY: `gh auth login`, `read -p`, REPL prompts and arrow-key menus all
//! behave normally, and `Ctrl+C` is delivered as a genuine SIGINT by the PTY
//! line discipline (ISIG) instead of being faked by the UI.
//!
//! While the session is active the TUI swaps the bottom input box for a live
//! "shell box" that streams the PTY output; raw keystrokes (arrows, digits,
//! Enter, `Ctrl+C`) are forwarded straight to the PTY.
//!
//! Lifecycle — owned by the UI loop:
//! - PTY stdout/stderr arrive as [`ShellEvent::Output`] **raw** byte chunks
//!   (escapes intact). The UI feeds them to a vt100 screen parser, so
//!   cursor-moving apps like `gh auth login` redraw in place instead of
//!   stacking every redraw as new lines.
//! - keystroke bytes go in via [`ShellSession::input_tx`].
//! - `Esc` fires [`ShellSession::interrupt`], which `SIGKILL`s the whole
//!   process group. (`Ctrl+C` is **not** an interrupt — it is forwarded as the
//!   byte `0x03` so the child gets a graceful SIGINT.)
//! - the shell exits on its own as soon as the child completes (no manual
//!   exit). [`ShellEvent::Exited`] then carries the full captured output +
//!   exit code: `Visible` (`!`) feeds it to the agent **and** writes it to the
//!   chat log; `Invisible` (`!!`) writes it to the chat log only.

use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::sandbox::Sandbox;
use crate::ui::ansi::{StripPolicy, strip_escapes};
#[cfg(unix)]
use crate::ui::pty_relay::stdio_from_fd;
use crate::ui::shell_phase::ShellKind;

/// A raw chunk of PTY output (escapes intact — the UI feeds it to a vt100
/// screen parser) or the final exit outcome.
pub(crate) enum ShellEvent {
    Output(Vec<u8>),
    Exited { outcome: ShellOutcome },
}

/// The resolved result of a finished shell session.
#[derive(Clone, Debug)]
pub(crate) struct ShellOutcome {
    pub exit_code: Option<i32>,
    /// Ansi-stripped, CRLF-normalized stdout+stderr captured for the whole run.
    pub captured: String,
    /// True when the user hard-interrupted via `Esc` (SIGKILL).
    pub interrupted: bool,
}

/// A live PTY-backed shell session. Hold this in the UI state while the shell
/// runs and poll [`ShellSession::events_rx`] in the event loop.
pub(crate) struct ShellSession {
    /// Forward raw keystroke bytes here (arrows, digits, `Ctrl+C` = `0x03`).
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// PTY events — drain this in the UI `select!`.
    pub events_rx: mpsc::UnboundedReceiver<ShellEvent>,
    /// Fire to `SIGKILL` the whole process group (maps to `Esc`).
    pub interrupt: Option<oneshot::Sender<()>>,
    pub join: Option<JoinHandle<()>>,
    pub kind: ShellKind,
    pub command: String,
}

/// Spawn `command` on a fresh PTY. The child becomes a session leader with the
/// PTY as its controlling terminal, so `isatty(0..2)` is true and `Ctrl+C`
/// (`0x03`) reaches it as SIGINT.
#[cfg(unix)]
pub(crate) fn spawn(
    command: &str,
    sandbox: &Sandbox,
    kind: ShellKind,
    cols: u16,
    rows: u16,
) -> io::Result<ShellSession> {
    let (primary, secondary) = open_pty_pair()?;
    let secondary_fd = secondary.as_raw_fd();
    // cooked-mode defaults a fresh terminal gives bash: echo + canonical line
    // editing + ISIG (so Ctrl+C -> SIGINT) + CRLF translation. Child apps
    // (gh survey, bash `read`, etc.) adjust termios themselves as needed.
    set_cooked(secondary_fd);
    set_winsize(secondary_fd, cols, rows);

    // Build the command with the PTY slave as stdin/stdout/stderr.
    let mut cmd = sandbox.command_for_interactive(command);
    let din = secondary.try_clone()?;
    let dout = secondary.try_clone()?;
    let derr = secondary.try_clone()?;
    unsafe {
        cmd.stdin(stdio_from_fd(din));
        cmd.stdout(stdio_from_fd(dout));
        cmd.stderr(stdio_from_fd(derr));
    }
    cmd.kill_on_drop(true);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // fd 0 is the slave (set up before pre_exec). Acquire it as the
            // controlling terminal so ISIG/SIGINT and job control work.
            let _ = libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Dup the master for the reader/writer threads, then drop the originals —
    // the child holds the slave; the dups hold the master.
    let primary_read = primary.try_clone()?;
    let primary_write = primary.try_clone()?;
    drop(secondary);
    drop(primary);

    let (events_tx, events_rx) = mpsc::unbounded_channel::<ShellEvent>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (interrupt_tx, interrupt_rx) = oneshot::channel::<()>();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    // Reader thread: master -> Output events + raw capture.
    let captured_r = captured.clone();
    let events_tx_r = events_tx.clone();
    let reader = std::thread::Builder::new()
        .name("shell-pty-read".into())
        .spawn(move || {
            let mut f = primary_read;
            let mut buf = [0u8; 4096];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut c) = captured_r.lock() {
                            c.extend_from_slice(chunk);
                        }
                        // Forward the raw bytes (escapes intact): the UI feeds
                        // them to a vt100 screen parser, so cursor-moving apps
                        // (gh survey, etc.) render in place instead of stacking
                        // every redraw. The ansi-stripped capture for the agent
                        // / chat log is computed once on exit from `captured`.
                        let _ = events_tx_r.send(ShellEvent::Output(chunk.to_vec()));
                    }
                    // Linux returns EIO when the slave side is closed (child gone).
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
        })?;

    // Writer thread: keystroke bytes -> master. The user drives EOF/Ctrl+D by
    // pressing Ctrl+D (forwarded as `0x04` via `key_to_bytes`); we do NOT
    // synthesize it on close, so teardown never injects stray bytes.
    let _writer = std::thread::Builder::new()
        .name("shell-pty-write".into())
        .spawn(move || {
            let mut f = primary_write;
            while let Some(bytes) = input_rx.blocking_recv() {
                if f.write_all(&bytes).is_err() {
                    break;
                }
                let _ = f.flush();
            }
        })?;

    let join = tokio::spawn(async move {
        let (status, interrupted) = tokio::select! {
            biased;
            Ok(()) = interrupt_rx => { kill_group(pid); (child.wait().await, true) }
            s = child.wait() => (s, false),
        };

        // Drain the reader: child gone -> slave closed -> master read returns
        // EOF/EIO -> reader ends. Joined off the async worker (std thread join).
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || {
                let _ = reader.join();
            }),
        )
        .await;

        let raw = captured
            .lock()
            .map(|c| String::from_utf8_lossy(&c).into_owned())
            .unwrap_or_default();
        let exit_code = status.ok().and_then(|s| s.code());
        let captured = strip_escapes(&raw, StripPolicy::KEEP_NEWLINE);
        let outcome = ShellOutcome {
            exit_code,
            captured,
            interrupted,
        };
        let _ = events_tx.send(ShellEvent::Exited { outcome });
        // The writer thread is detached: it ends once the UI drops `input_tx`
        // (which it does when it processes the `Exited` event just sent).
    });

    Ok(ShellSession {
        input_tx,
        events_rx,
        interrupt: Some(interrupt_tx),
        join: Some(join),
        kind,
        command: command.to_string(),
    })
}

/// Map a crossterm key to its standard terminal byte encoding, for forwarding
/// to the child PTY. Returns `None` for keys with no byte encoding (e.g.
/// bare modifier presses). `Esc` is handled by the caller (hard-interrupt),
/// not here.
pub(crate) fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let KeyEvent {
        code, modifiers, ..
    } = key;
    // Ctrl+letter / Ctrl+@.._ -> control byte (Ctrl+C = 0x03, Ctrl+D = 0x04, ...).
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let b = c.to_ascii_lowercase() as u32;
            if (b'@' as u32..=b'_' as u32).contains(&b) {
                return Some(vec![(b as u8) & 0o37]);
            }
        }
        return None;
    }
    match code {
        KeyCode::Char(c) => Some(c.encode_utf8(&mut [0u8; 4]).as_bytes().to_vec()),
        KeyCode::Enter => Some(vec![0o015]), // CR; PTY ICRNL turns it into NL for the child
        KeyCode::Backspace => Some(vec![0o177]), // DEL
        KeyCode::Tab => Some(vec![0o011]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

#[cfg(not(unix))]
async fn pump<R>(
    mut reader: R,
    events_tx: mpsc::UnboundedSender<ShellEvent>,
    captured: Arc<Mutex<Vec<u8>>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if let Ok(mut c) = captured.lock() {
                    c.extend_from_slice(chunk);
                }
                let _ = events_tx.send(ShellEvent::Output(chunk.to_vec()));
            }
        }
    }
}

/// Non-Unix fallback (Windows): there is no PTY, so the child runs with piped
/// stdio. Its combined stdout/stderr is forwarded as [`ShellEvent::Output`]
/// chunks then a single [`ShellEvent::Exited`]; keystrokes are not forwarded
/// (no controlling terminal to receive them). The live shell box, if it mounts,
/// just streams the captured output.
#[cfg(not(unix))]
pub(crate) fn spawn(
    command: &str,
    sandbox: &Sandbox,
    kind: ShellKind,
    _cols: u16,
    _rows: u16,
) -> io::Result<ShellSession> {
    use std::process::Stdio;

    let mut cmd = sandbox.command_for_interactive(command);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (events_tx, events_rx) = mpsc::unbounded_channel::<ShellEvent>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (interrupt_tx, interrupt_rx) = oneshot::channel::<()>();

    let join = tokio::spawn(async move {
        // No TTY to write keystrokes to; drain the channel so it doesn't linger
        // for the session's lifetime.
        let _drain = tokio::spawn(async move { while input_rx.recv().await.is_some() {} });

        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let out_task = stdout.map(|s| tokio::spawn(pump(s, events_tx.clone(), cap)));
        let cap = captured.clone();
        let err_task = stderr.map(|s| tokio::spawn(pump(s, events_tx.clone(), cap)));

        let (status, interrupted) = tokio::select! {
            biased;
            Ok(()) = interrupt_rx => {
                let _ = child.kill().await;
                (child.wait().await, true)
            }
            s = child.wait() => (s, false),
        };

        // Flush remaining output once the pipes close on child exit.
        if let Some(t) = out_task {
            let _ = t.await;
        }
        if let Some(t) = err_task {
            let _ = t.await;
        }

        let raw = captured
            .lock()
            .map(|c| String::from_utf8_lossy(&c).into_owned())
            .unwrap_or_default();
        let exit_code = status.ok().and_then(|s| s.code());
        let stripped = strip_escapes(&raw, StripPolicy::KEEP_NEWLINE);
        let outcome = ShellOutcome {
            exit_code,
            captured: stripped,
            interrupted,
        };
        let _ = events_tx.send(ShellEvent::Exited { outcome });
    });

    Ok(ShellSession {
        input_tx,
        events_rx,
        interrupt: Some(interrupt_tx),
        join: Some(join),
        kind,
        command: command.to_string(),
    })
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            // The child is a session leader (setsid), so -pid targets its group.
            if libc::kill(-(pid as libc::pid_t), libc::SIGKILL) == -1 {
                let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

// ── PTY helpers ────────────────────────────────────────────────────────────

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};

/// Open a new PTY pair, returning `(master, slave)`.
#[cfg(unix)]
fn open_pty_pair() -> io::Result<(std::fs::File, std::fs::File)> {
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    macro_rules! fail {
        () => {{
            let e = io::Error::last_os_error();
            unsafe {
                libc::close(master_fd);
            }
            return Err(e);
        }};
    }
    if unsafe { libc::grantpt(master_fd) } < 0 || unsafe { libc::unlockpt(master_fd) } < 0 {
        fail!();
    }
    let name_ptr = unsafe { libc::ptsname(master_fd) };
    if name_ptr.is_null() {
        fail!();
    }
    let slave_fd = unsafe { libc::open(name_ptr, libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        fail!();
    }
    Ok((unsafe { std::fs::File::from_raw_fd(master_fd) }, unsafe {
        std::fs::File::from_raw_fd(slave_fd)
    }))
}

/// Set sane cooked-mode termios on a PTY fd (echo, canonical, ISIG, CRLF).
#[cfg(unix)]
fn set_cooked(fd: libc::c_int) {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } < 0 {
        return;
    }
    t.c_lflag |= libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
    // Don't visually echo control chars (`^C`/`^D`/`^Z`): ISIG/EOF still work,
    // this just keeps stray Ctrl+D (e.g. the writer's EOF) out of the output.
    t.c_lflag &= !libc::ECHOCTL;
    t.c_iflag |= libc::ICRNL | libc::IXON;
    t.c_oflag |= libc::OPOST | libc::ONLCR;
    // Sensible cooked-mode control characters.
    t.c_cc[libc::VINTR] = 0o03;
    t.c_cc[libc::VQUIT] = 0o34;
    t.c_cc[libc::VEOF] = 0o04;
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &t);
    }
}

#[cfg(unix)]
fn set_winsize(fd: libc::c_int, cols: u16, rows: u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_col = cols;
    ws.ws_row = rows;
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::{Sandbox, SandboxMode};
    use std::time::Duration;
    use tokio::time::timeout;

    fn sb() -> Sandbox {
        Sandbox::new(SandboxMode::Off)
    }

    /// Drain events until `Exited` and return the outcome.
    async fn collect_outcome(rx: &mut mpsc::UnboundedReceiver<ShellEvent>) -> ShellOutcome {
        while let Some(ev) = rx.recv().await {
            if let ShellEvent::Exited { outcome } = ev {
                return outcome;
            }
        }
        panic!("events stream closed without Exited");
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let s = spawn(
            "echo hello; echo oops 1>&2",
            &sb(),
            ShellKind::Visible,
            80,
            24,
        )
        .unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        drop(input_tx);
        let out = collect_outcome(&mut events_rx).await;
        assert_eq!(out.exit_code, Some(0));
        assert!(out.captured.contains("hello"), "got: {:?}", out.captured);
        assert!(out.captured.contains("oops"), "got: {:?}", out.captured);
    }

    #[tokio::test]
    async fn child_sees_a_real_tty() {
        // The whole point of the PTY path: isatty(0) must be true.
        let s = spawn(
            "if [ -t 0 ]; then echo ISATTY; fi",
            &sb(),
            ShellKind::Visible,
            80,
            24,
        )
        .unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        drop(input_tx);
        let out = collect_outcome(&mut events_rx).await;
        assert!(
            out.captured.contains("ISATTY"),
            "stdin should be a tty, got: {:?}",
            out.captured
        );
    }

    #[tokio::test]
    async fn forwards_stdin_to_child() {
        let s = spawn("cat", &sb(), ShellKind::Visible, 80, 24).unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        input_tx.send(b"ping\n".to_vec()).unwrap();
        // Simulate the user pressing Ctrl+D -> PTY cooked-mode EOF -> cat exits.
        tokio::time::sleep(Duration::from_millis(150)).await;
        input_tx.send(b"\x04".to_vec()).unwrap();
        drop(input_tx);
        let out = timeout(Duration::from_secs(5), collect_outcome(&mut events_rx))
            .await
            .expect("cat did not exit on EOF")
            .clone();
        assert!(out.captured.contains("ping"), "got: {:?}", out.captured);
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn stdin_drives_a_read_echo_command() {
        let s = spawn(
            "read line; echo \"you said: $line\"",
            &sb(),
            ShellKind::Visible,
            80,
            24,
        )
        .unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        tokio::time::sleep(Duration::from_millis(150)).await;
        input_tx.send(b"world\n".to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(input_tx);
        let out = collect_outcome(&mut events_rx).await;
        assert!(
            out.captured.contains("you said: world"),
            "got: {:?}",
            out.captured
        );
    }

    #[tokio::test]
    async fn propagates_nonzero_exit_code() {
        let s = spawn("exit 7", &sb(), ShellKind::Visible, 80, 24).unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        drop(input_tx);
        let out = collect_outcome(&mut events_rx).await;
        assert_eq!(out.exit_code, Some(7));
    }

    #[tokio::test]
    async fn strips_ansi_escapes() {
        let s = spawn(
            "printf '\\033[31mred\\033[0m\\n'",
            &sb(),
            ShellKind::Visible,
            80,
            24,
        )
        .unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        drop(input_tx);
        let out = collect_outcome(&mut events_rx).await;
        assert_eq!(out.captured, "red\n", "got: {:?}", out.captured);
    }

    #[tokio::test]
    async fn ctrl_c_delivers_sigint() {
        // Ctrl+C must arrive as a real SIGINT (the gh-auth-login use case).
        let s = spawn(
            "trap 'echo CAUGHT; exit 0' INT; sleep 5",
            &sb(),
            ShellKind::Visible,
            80,
            24,
        )
        .unwrap();
        let ShellSession {
            input_tx,
            mut events_rx,
            ..
        } = s;
        // Let the trap install before sending the interrupt byte.
        tokio::time::sleep(Duration::from_millis(300)).await;
        input_tx.send(b"\x03".to_vec()).unwrap();
        let out = timeout(Duration::from_secs(6), collect_outcome(&mut events_rx))
            .await
            .expect("Ctrl+C did not terminate the child via SIGINT")
            .clone();
        assert!(
            out.captured.contains("CAUGHT"),
            "SIGINT trap should fire, got: {:?}",
            out.captured
        );
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.interrupted, "Ctrl+C is graceful, not a hard kill");
    }

    #[tokio::test]
    async fn interrupt_kills_long_running_process() {
        let s = spawn("sleep 30", &sb(), ShellKind::Visible, 80, 24).unwrap();
        let mut session = s;
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(tx) = session.interrupt.take() {
            let _ = tx.send(());
        }
        drop(session.input_tx);
        let out = timeout(
            Duration::from_secs(5),
            collect_outcome(&mut session.events_rx),
        )
        .await
        .expect("interrupt did not kill the child")
        .clone();
        assert!(out.interrupted);
    }
}
