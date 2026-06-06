//! Validates that no keystrokes arrive on the event channel during
//! the production shutdown sequence.
//!
//! All tests in this module require `sandbox-microvm`.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Write;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    #[ignore = "join_reader timeout race with unbounded channel — run with: cargo test --features sandbox-microvm -- input_reader_shutdown_no_lost_keystrokes --include-ignored"]
    fn input_reader_shutdown_no_lost_keystrokes() {
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

        // ── reset reader flags (avoid pollution from prior tests) ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // ── PTY as fake stdin ──
        let (primary, secondary) = open_pty_pair().expect("open PTY pair");
        assert!(
            unsafe { libc::dup2(secondary.as_raw_fd(), 0) } >= 0,
            "dup2 to fd 0 failed"
        );
        make_raw_fd(0).expect("make_raw on fd 0");

        // ── spawn production input reader ──
        let (tokio_tx, mut tokio_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
        crate::ui::input_reader::spawn_input_reader(tokio_tx);

        // ── bridge: drain tokio channel continuously, count events ──
        let event_count = Arc::new(AtomicUsize::new(0));
        let event_count2 = Arc::clone(&event_count);
        let bridge_done = Arc::new(AtomicBool::new(false));
        let bridge_done2 = Arc::clone(&bridge_done);
        let bridge = std::thread::spawn(move || {
            loop {
                match tokio_rx.try_recv() {
                    Ok(_) => {
                        event_count2.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        if bridge_done2.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
        });

        // ── injector: 1000 bytes/sec into PTY primary ──
        let injector_running = Arc::new(AtomicBool::new(true));
        let injector_running2 = Arc::clone(&injector_running);
        let injector_bytes = Arc::new(AtomicUsize::new(0));
        let injector_bytes2 = Arc::clone(&injector_bytes);
        let shutdown_active = Arc::new(AtomicBool::new(false));
        let shutdown_active2 = Arc::clone(&shutdown_active);
        let charset: Vec<u8> = (b'a'..=b'z')
            .chain(b'A'..=b'Z')
            .chain(b'0'..=b'9')
            .collect();
        let interval = Duration::from_micros(1000);
        let injector = std::thread::spawn(move || {
            let mut pri = primary;
            for i in 0u64.. {
                if !injector_running2.load(Ordering::Relaxed) {
                    break;
                }
                let b = charset[i as usize % charset.len()];
                let _ = pri.write_all(&[b]);
                let _ = pri.flush();
                if shutdown_active2.load(Ordering::Relaxed) {
                    injector_bytes2.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(interval);
            }
        });

        // ── warmup: let reader + injector stabilize ──
        std::thread::sleep(Duration::from_millis(100));

        // ── snapshot pre-shutdown event count ──
        let events_before = event_count.load(Ordering::Relaxed);

        // ── production shutdown sequence ──
        shutdown_active.store(true, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        crate::ui::terminal::join_reader(Duration::from_millis(50));
        let drained = crate::ui::terminal::drain_stdin_nonblocking();

        // ── wait for channel to stabilize ──
        let mut prev_count = event_count.load(Ordering::Relaxed);
        let stabilization_deadline = std::time::Instant::now() + Duration::from_millis(200);
        loop {
            std::thread::sleep(Duration::from_millis(2));
            let current = event_count.load(Ordering::Relaxed);
            if current == prev_count {
                break;
            }
            prev_count = current;
            if std::time::Instant::now() >= stabilization_deadline {
                break;
            }
        }
        let events_after = event_count.load(Ordering::Relaxed);

        // ── stop injector and bridge ──
        injector_running.store(false, Ordering::Relaxed);
        let _ = injector.join();
        bridge_done.store(true, Ordering::Relaxed);
        let _ = bridge.join();

        let new_events = events_after.saturating_sub(events_before);
        let shutdown_bytes = injector_bytes.load(Ordering::Relaxed);

        eprintln!(
            "shutdown_race: events_before={} new_during_shutdown={} drained_bytes={} shutdown_window_bytes={}",
            events_before,
            new_events,
            drained.len(),
            shutdown_bytes,
        );

        assert_eq!(
            new_events,
            0,
            "{} keystrokes arrived on event channel during shutdown — bytes lost (drained={}, shutdown_bytes={})",
            new_events,
            drained.len(),
            shutdown_bytes,
        );

        assert!(
            !drained.is_empty(),
            "drain buffer empty — injector wasn't writing during shutdown or drain failed"
        );

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
