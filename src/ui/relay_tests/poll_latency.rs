//! Measures end-to-end latency from byte-on-PTY to event-on-channel
//! through the production crossterm input reader.
//!
//! All tests in this module require `sandbox-microvm`.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::collections::VecDeque;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "join_reader timeout race with unbounded channel — run with: cargo test --features sandbox-microvm -- poll_latency_under_load --include-ignored"]
    fn poll_latency_under_load() {
        let _guard = serial_fd_test();

        // ── save/restore real stdin ──
        let saved_stdin = unsafe { libc::dup(0) };
        assert!(saved_stdin >= 0, "dup stdin");
        let mut saved_termios: libc::termios = unsafe { std::mem::zeroed() };
        let have_termios = unsafe { libc::tcgetattr(0, &mut saved_termios) } == 0;

        // ── reset reader flags ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // ── PTY as fake stdin (once, shared across all rates) ──
        let (primary, secondary) = open_pty_pair().expect("open PTY pair");
        assert!(unsafe { libc::dup2(secondary.as_raw_fd(), 0) } >= 0);
        make_raw_fd(0).expect("make_raw on fd 0");

        let rates: [(&str, usize, u64); 3] = [
            ("poll_latency_baseline", 50, 100_000),
            ("poll_latency_moderate", 200, 10_000),
            ("poll_latency_stress", 500, 2_000),
        ];

        for (name, count, gap_us) in &rates {
            let count = *count;
            let gap_us = *gap_us;

            // ── spawn production input reader ──
            let (tokio_tx, mut tokio_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();
            crate::ui::input_reader::spawn_input_reader(tokio_tx);

            let latencies: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::with_capacity(count)));
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

            // injector thread
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

            // collector thread
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

            // Wait for collector to finish.
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

            // ── shutdown this reader instance ──
            crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
            crate::ui::terminal::join_reader(Duration::from_millis(100));
            crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
            crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

            // ── compute stats ──
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

        // ── cleanup ──
        unsafe {
            libc::dup2(saved_stdin, 0);
            if have_termios {
                libc::tcsetattr(0, libc::TCSANOW, &saved_termios);
            }
        }
    }
}
