//! Validates that running the sentinel drain BEFORE spawning the input
//! reader does not cause keystroke loss.
//!
//! All tests in this module require `sandbox-microvm`.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Write;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    #[ignore = "join_reader timeout race with unbounded channel — run with: cargo test --features sandbox-microvm -- restore_sentinel_before_reader_no_keystroke_loss --include-ignored"]
    fn restore_sentinel_before_reader_no_keystroke_loss() {
        let _guard = serial_fd_test();

        // ── save original stdin fd ──
        let saved_stdin = unsafe { OwnedFd::from_raw_fd(libc::dup(0)) };
        if saved_stdin.as_raw_fd() < 0 {
            eprintln!("skipping: dup(0) failed");
            return;
        }

        let saved_termios: Option<libc::termios> = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) >= 0 {
                Some(t)
            } else {
                None
            }
        };

        // ── reset reader flags ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // ── PTY as fake stdin ──
        let (mut primary, secondary) = open_pty_pair().expect("open PTY pair");
        assert!(
            unsafe { libc::dup2(secondary.as_raw_fd(), 0) } >= 0,
            "dup2 to fd 0 failed"
        );
        make_raw_fd(0).expect("make_raw on fd 0");
        set_nonblocking(&primary).expect("nonblocking on primary");

        // ── inject DSR-OS reply from a background thread so the sentinel
        //     doesn't time out waiting for a real terminal ──
        let mut primary_clone = primary.try_clone().expect("clone PTY primary");
        let reply_injected = Arc::new(AtomicBool::new(false));
        let reply_injected2 = Arc::clone(&reply_injected);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            let _ = primary_clone.write_all(b"\x1b[0n");
            let _ = primary_clone.flush();
            reply_injected2.store(true, Ordering::Relaxed);
        });

        // ── sentinel drain with /dev/null as stdout ──
        let mut null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        crate::ui::terminal::sync_and_drain_via_sentinel(&mut null, Duration::from_millis(100));
        // Flush any leftover bytes between sentinel and reader spawn.
        drain_fd_nonblock(&mut primary);

        // ── spawn input reader AFTER sentinel ──
        let (tokio_tx, mut tokio_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
        crate::ui::input_reader::spawn_input_reader(tokio_tx);

        // ── inject keystrokes ──
        let keystrokes: &[u8] = b"the quick brown fox";
        for &b in keystrokes {
            primary.write_all(&[b]).unwrap();
            primary.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }

        // ── drain events with timeout ──
        let mut key_events = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match tokio_rx.try_recv() {
                Ok(crate::event::UserEvent::Key(_)) => key_events += 1,
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if key_events >= keystrokes.len() || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        assert_eq!(
            key_events,
            keystrokes.len(),
            "expected {} key events, got {}",
            keystrokes.len(),
            key_events
        );

        // ── shutdown reader ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        crate::ui::terminal::join_reader(Duration::from_millis(50));

        // ── cleanup: restore stdin and termios ──
        let saved_fd = saved_stdin.as_raw_fd();
        unsafe {
            libc::dup2(saved_fd, 0);
            if let Some(ref t) = saved_termios {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);
    }
}
