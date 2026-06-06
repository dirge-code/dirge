#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    // ── Test 2.3: Sustained Stream (paste) ─────────────────────────

    #[test]
    fn relay_sustained_stream() {
        const BLOCKS: usize = 8;
        const BLOCK_SIZE: usize = 4096;
        const PAUSE_MS: u64 = 100;

        // Use raw setup with non-blocking tty so we can drain echoed
        // bytes between blocks — prevents the tty PTY buffer from
        // filling and stalling the relay's write path.
        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let (tty_primary, mut tty_secondary) = open_pty_pair().expect("open fake tty PTY");
        make_raw_fd(tty_secondary.as_raw_fd()).expect("raw mode on tty secondary");
        set_nonblocking(&tty_secondary).expect("nonblocking");

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        let mut injected = Vec::with_capacity(BLOCKS * BLOCK_SIZE);
        let mut echoed = Vec::with_capacity(BLOCKS * BLOCK_SIZE);
        for block_idx in 0..BLOCKS {
            let block: Vec<u8> = (0..BLOCK_SIZE)
                .map(|j| b'a' + ((block_idx * BLOCK_SIZE + j) % 26) as u8)
                .collect();

            // Write block byte-by-byte with WouldBlock retry.
            // Between retries, drain echoed bytes to free tty buffer space.
            let mut offset = 0;
            while offset < block.len() {
                match tty_secondary.write(&block[offset..]) {
                    Ok(0) => panic!("write returned 0"),
                    Ok(n) => offset += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Drain echoed bytes to free PTY buffer space,
                        // unblocking the relay's tty→PTY read path.
                        echoed.append(&mut drain_fd_nonblock(&mut tty_secondary));
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("write error: {e}"),
                }
                // Drain after each successful write too.
                echoed.append(&mut drain_fd_nonblock(&mut tty_secondary));
            }
            tty_secondary.flush().ok();
            injected.extend_from_slice(&block);
            if block_idx + 1 < BLOCKS {
                std::thread::sleep(Duration::from_millis(PAUSE_MS));
            }
        }

        // Final drain.
        std::thread::sleep(Duration::from_millis(500));
        echoed.append(&mut drain_fd_nonblock(&mut tty_secondary));
        drop(tty_secondary);

        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "relay_sustained_stream: relay exited {status:?}, injected={} echoed={}",
                    injected.len(),
                    echoed.len()
                );
            }
            Ok(Err(e)) => panic!("relay_sustained_stream: relay error: {e}"),
            Err(_) => panic!("relay_sustained_stream: relay thread panicked"),
        }

        let mut echoed_sorted = echoed.clone();
        echoed_sorted.sort();
        let mut injected_sorted = injected.clone();
        injected_sorted.sort();
        assert_eq!(
            echoed_sorted,
            injected_sorted,
            "sustained stream: echo mismatch: injected {} bytes, got {} back",
            injected.len(),
            echoed.len()
        );
    }

    // ── Test 2.4: Overlap Attack (concurrent bidirectional writes) ─

    /// Write to tty-secondary from two threads simultaneously while the
    /// relay is draining. Both threads send interleaved byte streams;
    /// the relay must correctly forward all bytes without loss.
    #[test]
    fn relay_overlap_attack() {
        const PER_THREAD: usize = 1000;
        const TOTAL: usize = PER_THREAD * 2;

        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let (tty_primary, tty_secondary) = open_pty_pair().expect("open fake tty PTY");
        make_raw_fd(tty_secondary.as_raw_fd()).expect("raw mode on tty secondary");
        // Blocking mode for the injector threads.

        let tty_shared = std::sync::Arc::new(std::sync::Mutex::new(tty_secondary));

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        let tty_a = tty_shared.clone();
        let tty_b = tty_shared.clone();

        let t1 = std::thread::spawn(move || {
            let mut injected = Vec::with_capacity(PER_THREAD);
            for i in 0..PER_THREAD {
                let b = b'a' + (i % 26) as u8;
                injected.push(b);
                let mut tty = tty_a.lock().unwrap();
                tty.write_all(&[b]).ok();
                tty.flush().ok();
                drop(tty);
                if i % 5 == 0 {
                    std::thread::yield_now();
                }
            }
            injected
        });

        let t2 = std::thread::spawn(move || {
            let mut injected = Vec::with_capacity(PER_THREAD);
            for i in 0..PER_THREAD {
                let b = b'A' + (i % 26) as u8;
                injected.push(b);
                let mut tty = tty_b.lock().unwrap();
                tty.write_all(&[b]).ok();
                tty.flush().ok();
                drop(tty);
                if i % 5 == 0 {
                    std::thread::yield_now();
                }
            }
            injected
        });

        let injected_a = t1.join().unwrap();
        let injected_b = t2.join().unwrap();

        let mut injected = injected_a;
        injected.extend_from_slice(&injected_b);
        assert_eq!(injected.len(), TOTAL);

        // Switch to non-blocking, then drain echoes.
        {
            let final_tty = tty_shared.lock().unwrap();
            set_nonblocking(&final_tty).expect("set nonblocking for drain");
        }

        // Give the relay time to process all bytes.
        std::thread::sleep(Duration::from_millis(500));

        let mut final_tty = tty_shared.lock().unwrap();
        let echoed = drain_fd_nonblock(&mut final_tty);
        drop(final_tty);
        drop(tty_shared);

        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "relay_overlap_attack: relay exited {status:?}, injected={} echoed={}",
                    injected.len(),
                    echoed.len()
                );
            }
            Ok(Err(e)) => panic!("relay_overlap_attack: relay error: {e}"),
            Err(_) => panic!("relay_overlap_attack: relay thread panicked"),
        }

        let mut echoed_sorted = echoed.clone();
        echoed_sorted.sort();
        let mut injected_sorted = injected.clone();
        injected_sorted.sort();
        assert_eq!(
            echoed_sorted,
            injected_sorted,
            "overlap attack: echo mismatch: injected {} bytes, got {} back",
            injected.len(),
            echoed.len()
        );
    }
}
