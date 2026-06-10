#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    use std::time::Duration;

    fn reset_reader_and_drain(primary: &mut std::fs::File) {
        crate::ui::terminal::EVENT_READER_SHUTDOWN
            .store(true, std::sync::atomic::Ordering::Relaxed);
        crate::ui::terminal::join_reader(Duration::from_millis(100));
        crate::ui::terminal::EVENT_READER_SHUTDOWN
            .store(false, std::sync::atomic::Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, std::sync::atomic::Ordering::Relaxed);
        drain_fd_nonblock(primary);
    }

    #[test]
    fn crossterm_input_reader_suite() {
        let _guard = serial_fd_test();

        use std::collections::VecDeque;
        use std::io::Write;
        use std::os::unix::io::OwnedFd;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Instant;

        // ── save original stdin ──
        let saved_stdin = unsafe { OwnedFd::from_raw_fd(libc::dup(0)) };
        assert!(saved_stdin.as_raw_fd() >= 0, "dup(0) failed");
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

        // ── PTY as fake stdin (one pair for all phases) ──
        let (mut primary, secondary) = open_pty_pair().expect("open PTY pair");
        assert!(unsafe { libc::dup2(secondary.as_raw_fd(), 0) } >= 0);
        make_raw_fd(0).expect("make_raw on fd 0");
        set_nonblocking(&primary).expect("nonblocking on primary");

        // ═══════════════════════════════════════════════════════════
        // Phase 3.1: Sentinel-before-reader
        // ═══════════════════════════════════════════════════════════
        {
            let mut primary_clone = primary.try_clone().expect("clone primary");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                let _ = primary_clone.write_all(b"\x1b[0n");
                let _ = primary_clone.flush();
            });

            let mut null = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap();
            crate::ui::terminal::sync_and_drain_via_sentinel(&mut null, Duration::from_millis(100));
            drain_fd_nonblock(&mut primary);

            let (tokio_tx, mut tokio_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
            crate::ui::input_reader::spawn_input_reader(tokio_tx);

            let keystrokes: &[u8] = b"the quick brown fox";
            for &b in keystrokes {
                primary.write_all(&[b]).unwrap();
                primary.flush().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }

            let mut key_events = 0usize;
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match tokio_rx.try_recv() {
                    Ok(crate::event::UserEvent::Key(_)) => key_events += 1,
                    Ok(_) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        if key_events >= keystrokes.len() || Instant::now() >= deadline {
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
                "3.1 sentinel-before-reader: expected {} key events, got {}",
                keystrokes.len(),
                key_events
            );

            reset_reader_and_drain(&mut primary);
        }

        // ═══════════════════════════════════════════════════════════
        // Phase 3.2: Shutdown race
        // ═══════════════════════════════════════════════════════════
        {
            let (tokio_tx, mut tokio_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
            crate::ui::input_reader::spawn_input_reader(tokio_tx);

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
            let mut pri = primary.try_clone().expect("clone primary");
            let injector = std::thread::spawn(move || {
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
                    std::thread::sleep(Duration::from_micros(1000));
                }
            });

            std::thread::sleep(Duration::from_millis(100));
            let events_before = event_count.load(Ordering::Relaxed);

            shutdown_active.store(true, Ordering::Relaxed);
            crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
            crate::ui::terminal::join_reader(Duration::from_millis(50));

            // The reader is now joined (consuming nothing). The earlier
            // version drained whatever the free-running injector happened
            // to have buffered at this instant — racy, so the drain came
            // back empty intermittently. Instead write a DETERMINISTIC
            // sentinel (a byte outside the injector's alnum charset) now
            // that nothing will consume it, and drain until it appears.
            // This still exercises the drain-capture path (the real
            // invariant) without depending on injector timing.
            const SENTINEL: u8 = b'~';
            {
                let mut sentinel_pri = primary.try_clone().expect("clone primary for sentinel");
                let _ = sentinel_pri.write_all(&[SENTINEL; 4]);
                let _ = sentinel_pri.flush();
            }
            let mut drained: Vec<u8> = Vec::new();
            let drain_deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < drain_deadline {
                drained.extend_from_slice(&crate::ui::terminal::drain_stdin_nonblocking());
                if drained.contains(&SENTINEL) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }

            let mut prev_count = event_count.load(Ordering::Relaxed);
            let stabilization_deadline = Instant::now() + Duration::from_millis(200);
            loop {
                std::thread::sleep(Duration::from_millis(2));
                let current = event_count.load(Ordering::Relaxed);
                if current == prev_count {
                    break;
                }
                prev_count = current;
                if Instant::now() >= stabilization_deadline {
                    break;
                }
            }
            let events_after = event_count.load(Ordering::Relaxed);

            injector_running.store(false, Ordering::Relaxed);
            let _ = injector.join();
            bridge_done.store(true, Ordering::Relaxed);
            let _ = bridge.join();

            let new_events = events_after.saturating_sub(events_before);
            let shutdown_bytes = injector_bytes.load(Ordering::Relaxed);

            eprintln!(
                "shutdown_race: events_before={} new={} drained={} shutdown_window_bytes={}",
                events_before,
                new_events,
                drained.len(),
                shutdown_bytes,
            );

            // The injector floods bytes at ~1/ms right up to shutdown, so
            // at the instant EVENT_READER_SHUTDOWN is set there is almost
            // always a byte already mid-read; the reader emits that one
            // final event before its loop observes the flag. "Exactly 0"
            // was therefore flaky (new=1 intermittently). The real
            // invariant is that shutdown stops the flood PROMPTLY — a
            // bounded handful, not the dozens that would arrive if the
            // reader ignored the flag entirely.
            const MAX_SHUTDOWN_RACE_EVENTS: usize = 3;
            assert!(
                new_events <= MAX_SHUTDOWN_RACE_EVENTS,
                "3.2 shutdown race: {} events arrived during shutdown (max {}; drained={}, shutdown_bytes={})",
                new_events,
                MAX_SHUTDOWN_RACE_EVENTS,
                drained.len(),
                shutdown_bytes,
            );
            assert!(
                drained.contains(&SENTINEL),
                "3.2: drain buffer never captured the post-shutdown sentinel (len={})",
                drained.len(),
            );

            reset_reader_and_drain(&mut primary);
        }

        // ═══════════════════════════════════════════════════════════
        // Phase 3.3: Poll latency (3 rates)
        // ═══════════════════════════════════════════════════════════
        {
            let rates: [(&str, usize, u64); 3] = [
                ("poll_latency_baseline", 50, 100_000),
                ("poll_latency_moderate", 200, 10_000),
                ("poll_latency_stress", 500, 2_000),
            ];

            for (name, count, gap_us) in &rates {
                let count = *count;
                let gap_us = *gap_us;

                let (tokio_tx, mut tokio_rx) =
                    tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
                crate::ui::input_reader::spawn_input_reader(tokio_tx);

                let latencies: Arc<Mutex<Vec<u128>>> =
                    Arc::new(Mutex::new(Vec::with_capacity(count)));
                let timestamps: Arc<Mutex<VecDeque<Instant>>> =
                    Arc::new(Mutex::new(VecDeque::with_capacity(count)));
                let timestamps2 = Arc::clone(&timestamps);
                let latencies2 = Arc::clone(&latencies);
                let injector_done = Arc::new(AtomicBool::new(false));
                let injector_done2 = Arc::clone(&injector_done);
                let charset: Vec<u8> = (b'a'..=b'z')
                    .chain(b'A'..=b'Z')
                    .chain(b'0'..=b'9')
                    .collect();

                let mut pri = primary.try_clone().expect("clone primary");
                let injector = std::thread::spawn(move || {
                    for i in 0..count {
                        let b = charset[i % charset.len()];
                        {
                            let mut ts = timestamps2.lock().unwrap();
                            ts.push_back(Instant::now());
                        }
                        let _ = pri.write_all(&[b]);
                        let _ = pri.flush();
                        if gap_us > 0 {
                            std::thread::sleep(Duration::from_micros(gap_us));
                        }
                    }
                    injector_done2.store(true, Ordering::Relaxed);
                });

                let collector_done = Arc::new(AtomicBool::new(false));
                let collector_done2 = Arc::clone(&collector_done);
                let collector = std::thread::spawn(move || {
                    let mut received = 0usize;
                    loop {
                        match tokio_rx.try_recv() {
                            Ok(_) => {
                                let now = Instant::now();
                                let ts_opt = timestamps.lock().unwrap().pop_front();
                                if let Some(ts) = ts_opt {
                                    latencies2
                                        .lock()
                                        .unwrap()
                                        .push(now.duration_since(ts).as_micros());
                                }
                                received += 1;
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                if injector_done.load(Ordering::Relaxed) && received >= count {
                                    break;
                                }
                                std::thread::yield_now();
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                    collector_done2.store(true, Ordering::Relaxed);
                });

                let deadline = Instant::now() + Duration::from_secs(10);
                while !collector_done.load(Ordering::Relaxed) {
                    if Instant::now() >= deadline {
                        eprintln!("{name}: collector timed out");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }

                let _ = injector.join();
                let _ = collector.join();

                reset_reader_and_drain(&mut primary);

                let lats = latencies.lock().unwrap();
                if lats.is_empty() {
                    eprintln!("{name}: NO LATENCY DATA");
                    continue;
                }
                let min = lats.iter().min().unwrap();
                let max = lats.iter().max().unwrap();
                let avg: f64 = lats.iter().sum::<u128>() as f64 / lats.len() as f64;
                let mut sorted = lats.clone();
                sorted.sort_unstable();
                let median = sorted[sorted.len() / 2];
                let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];

                eprintln!(
                    "{name}: n={} min={min}us max={max}us avg={avg:.0}us median={median}us p99={p99}us",
                    lats.len(),
                );
            }
        }

        // ── cleanup: restore stdin ──
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
