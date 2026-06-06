//! PTY harness: inject keystrokes through a pseudo-terminal to measure
//! real crossterm input-reader latency while KVM vCPU threads compete
//! for the CPU.
//!
//! [`KeystrokeDriver`] sets up a real PTY pair, redirects stdin to the
//! secondary, and spawns the production input-reader thread + a background
//! injector. The caller receives per-keystroke [`KeyTick`]s via a sync
//! channel and can measure inter-arrival gaps to compute p50/p99/max.
//!
//! On drop, stdin is restored and the input-reader shutdown flag is
//! set so the reader thread exits cleanly.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

/// A keystroke delivered by the input reader, timestamped at arrival.
pub struct KeyTick {
    pub timestamp: Instant,
}

/// Drives keystrokes through a PTY-backed crossterm input reader.
///
/// On construction this saves fd 0, creates a PTY, dup2s the secondary
/// onto fd 0, enables raw mode, and spawns the production
/// [`crate::ui::input_reader::spawn_input_reader`] + a bridge thread
/// + an injector thread.
///
/// The injector writes `bytes_per_sec` ASCII bytes/sec (cycling
/// through `a-zA-Z0-9`) indefinitely — the caller should use
/// `receiver().iter().take(N)` to collect N samples.
///
/// On drop, stdin is restored and the input-reader shutdown flag is
/// restored to its pre-test value so the reader thread exits cleanly
/// without leaking the flag to other tests.
pub struct KeystrokeDriver {
    rx: mpsc::Receiver<KeyTick>,
    _saved_stdin: OwnedFd,
    _pty_secondary: std::fs::File,
    prev_shutdown_flag: bool,
    saved_termios: libc::termios,
}

impl KeystrokeDriver {
    pub fn new(bytes_per_sec: usize) -> Option<Self> {
        // Save the original stdin fd so we can restore it on drop.
        let saved_stdin = unsafe { OwnedFd::from_raw_fd(libc::dup(0)) };
        if saved_stdin.as_raw_fd() < 0 {
            return None;
        }

        // Save the real terminal's termios BEFORE we redirect fd 0.
        // After redirect_stdin, fd 0 points to the PTY secondary and
        // tcgetattr would return PTY defaults — not the user's actual
        // terminal settings. Restoring PTY defaults to the real terminal
        // on drop would corrupt it, requiring `reset` to recover.
        let saved_termios = save_termios()?;

        let pair = open_pty()?;
        redirect_stdin(&pair.secondary)?;
        // Now fd 0 is the PTY — enable raw mode so crossterm's event
        // reader gets individual keystrokes.
        make_raw_terminal()?;

        let (tx, rx) = mpsc::channel::<KeyTick>();

        // Spawn the production crossterm input reader.
        let (tokio_tx, mut tokio_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
        crate::ui::input_reader::spawn_input_reader(tokio_tx);

        // Bridge thread: poll the tokio channel in a tight loop
        // (try_recv + yield_now) so keystrokes are timestamped
        // immediately without tokio-runtime scheduling batching.
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            loop {
                match tokio_rx.try_recv() {
                    Ok(crate::event::UserEvent::Key(_)) => {
                        if tx2
                            .send(KeyTick {
                                timestamp: Instant::now(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        std::thread::yield_now();
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
        });

        // Injector: write bytes to PTY primary on a cadence.
        let charset: Vec<u8> = (b'a'..=b'z')
            .chain(b'A'..=b'Z')
            .chain(b'0'..=b'9')
            .collect();
        let interval = std::time::Duration::from_secs_f64(1.0 / bytes_per_sec as f64);
        let mut primary = pair.primary;
        std::thread::spawn(move || {
            for i in 0u64.. {
                let b = charset[i as usize % charset.len()];
                let _ = std::io::Write::write_all(&mut primary, &[b]);
                let _ = std::io::Write::flush(&mut primary);
                std::thread::sleep(interval);
            }
        });

        Some(Self {
            rx,
            _saved_stdin: saved_stdin,
            _pty_secondary: pair.secondary,
            prev_shutdown_flag: crate::ui::terminal::EVENT_READER_SHUTDOWN.load(Ordering::Relaxed),
            saved_termios,
        })
    }

    pub fn receiver(&self) -> &mpsc::Receiver<KeyTick> {
        &self.rx
    }
}

impl Drop for KeystrokeDriver {
    fn drop(&mut self) {
        // Signal the input reader to exit.
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        // Restore original stdin so subsequent tests / the test runner
        // aren't talking to a dead PTY.
        let saved = self._saved_stdin.as_raw_fd();
        unsafe {
            libc::dup2(saved, 0);
        }
        // Restore original terminal settings (raw mode was set by
        // make_raw_terminal). Without this, the caller's shell is left
        // in raw mode — no echo, no line buffering — and requires
        // `reset` to recover.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved_termios);
        }
        // Restore the pre-test shutdown flag so other tests aren't
        // polluted by the shutdown signal.
        crate::ui::terminal::EVENT_READER_SHUTDOWN
            .store(self.prev_shutdown_flag, Ordering::Relaxed);
    }
}

// ── PTY / terminal helpers ─────────────────────────────────────────

struct PtyPair {
    primary: std::fs::File,
    secondary: std::fs::File,
}

fn open_pty() -> Option<PtyPair> {
    let primary_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if primary_fd < 0 {
        return None;
    }
    if unsafe { libc::grantpt(primary_fd) } < 0 || unsafe { libc::unlockpt(primary_fd) } < 0 {
        unsafe { libc::close(primary_fd) };
        return None;
    }
    let secondary_name = unsafe { libc::ptsname(primary_fd) };
    if secondary_name.is_null() {
        unsafe { libc::close(primary_fd) };
        return None;
    }
    let secondary_fd = unsafe { libc::open(secondary_name, libc::O_RDWR | libc::O_NOCTTY) };
    if secondary_fd < 0 {
        unsafe { libc::close(primary_fd) };
        return None;
    }
    let primary = unsafe { std::fs::File::from_raw_fd(primary_fd) };
    let secondary = unsafe { std::fs::File::from_raw_fd(secondary_fd) };
    Some(PtyPair { primary, secondary })
}

fn redirect_stdin(secondary: &std::fs::File) -> Option<()> {
    if unsafe { libc::dup2(secondary.as_raw_fd(), 0) } < 0 {
        return None;
    }
    Some(())
}

fn save_termios() -> Option<libc::termios> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut termios) } < 0 {
        return None;
    }
    Some(termios)
}

fn make_raw_terminal() -> Option<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut termios) } < 0 {
        return None;
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) } < 0 {
        return None;
    }
    Some(())
}
