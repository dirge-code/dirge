#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    #[test]
    fn relay_slow_tty_consumer() {
        // Large enough to fill the PTY buffer and trigger WouldBlock on
        // the relay's tty writes. Linux PTY buffer is typically 4096.
        const BURST_SIZE: usize = 32768;
        const FOLLOWUP_SIZE: usize = 256;
        const TOTAL: usize = BURST_SIZE + FOLLOWUP_SIZE;

        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let (tty_primary, mut tty_secondary) = open_pty_pair().expect("open fake tty PTY");
        make_raw_fd(tty_secondary.as_raw_fd()).expect("raw mode on tty secondary");
        // tty_primary is set to O_NONBLOCK by relay_to_fd.
        // tty_secondary stays BLOCKING — we DON'T drain during the
        // test to simulate a slow terminal consumer.

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        // Write the burst byte-by-byte with WouldBlock retry.
        // The relay reads these, writes them to the PTY, cat echoes,
        // relay reads echoes, writes them back to tty_primary.
        // Since we never drain tty_secondary, the PTY buffer fills
        // and the relay's tty writes hit WouldBlock.
        let mut burst: Vec<u8> = Vec::with_capacity(BURST_SIZE);
        for i in 0..BURST_SIZE {
            let b = b'a' + (i % 26) as u8;
            burst.push(b);
            loop {
                match tty_secondary.write(&[b]) {
                    Ok(0) => panic!("write returned 0"),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // PTY buffer is full — this is expected.
                        // The relay should still be processing.
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("write error: {e}"),
                }
            }
        }
        tty_secondary.flush().ok();

        // Now inject follow-up bytes. These must still be forwarded
        // by the relay even though the tty buffer is backed up.
        let mut followup: Vec<u8> = Vec::with_capacity(FOLLOWUP_SIZE);
        for i in 0..FOLLOWUP_SIZE {
            let b = b'Z' - (i % 26) as u8;
            followup.push(b);
            loop {
                match tty_secondary.write(&[b]) {
                    Ok(0) => panic!("write returned 0"),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("write error: {e}"),
                }
            }
        }
        tty_secondary.flush().ok();

        // Now drain everything. Switch to non-blocking and poll-drain
        // until every injected byte has echoed back, or a generous
        // timeout elapses. The relay is still flushing the backed-up PTY
        // buffer at this point, so a single fixed sleep + one-shot drain
        // raced it under CI load and lost trailing echoes (flaky).
        set_nonblocking(&tty_secondary).expect("set nonblocking for drain");
        let mut echoed: Vec<u8> = Vec::with_capacity(TOTAL);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while echoed.len() < TOTAL && std::time::Instant::now() < deadline {
            echoed.extend(drain_fd_nonblock(&mut tty_secondary));
            if echoed.len() < TOTAL {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        drop(tty_secondary);

        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "relay_slow_tty_consumer: relay exited {status:?}, \
                 burst={BURST_SIZE} followup={FOLLOWUP_SIZE} echoed={}",
                    echoed.len()
                );
            }
            Ok(Err(e)) => panic!("relay_slow_tty_consumer: relay error: {e}"),
            Err(_) => panic!("relay_slow_tty_consumer: relay thread panicked"),
        }

        let mut echoed_sorted = echoed.clone();
        echoed_sorted.sort();
        let mut injected: Vec<u8> = Vec::with_capacity(TOTAL);
        injected.extend_from_slice(&burst);
        injected.extend_from_slice(&followup);
        injected.sort();
        assert_eq!(
            echoed_sorted,
            injected,
            "slow tty consumer: echo mismatch: injected {} bytes, got {} back",
            injected.len(),
            echoed.len()
        );
    }

    // ── Test 2.5: Final Drain on Child Exit ─────────────────────────
    //
    // When the child process exits, the relay must flush buffered writes
    // and drain any remaining PTY output before returning. Without this
    // drain, the user loses the last screen update, the shell prompt,
    // and any output produced between the last poll() and child exit.

    #[test]
    fn relay_drain_inject_boundary() {
        const PRE_INJECT: usize = 500;
        const LIVE_INJECT: usize = 500;
        const TOTAL: usize = PRE_INJECT + LIVE_INJECT;

        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let mut relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let (tty_primary, mut tty_secondary) = open_pty_pair().expect("open fake tty PTY");
        make_raw_fd(tty_secondary.as_raw_fd()).expect("raw mode on tty secondary");

        // Phase 1: drain simulation — inject bytes via write_to_primary.
        let pre_bytes: Vec<u8> = (0..PRE_INJECT).map(|i| b'a' + (i % 26) as u8).collect();
        relay
            .write_to_primary(&pre_bytes)
            .expect("write_to_primary");

        // Phase 2: start relay.
        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        // Small gap so the relay can pick up the pre-injected bytes.
        std::thread::sleep(Duration::from_millis(50));

        // Phase 3: live keystroke simulation — write through tty secondary.
        let live_bytes: Vec<u8> = (0..LIVE_INJECT).map(|i| b'A' + (i % 26) as u8).collect();
        for b in &live_bytes {
            tty_secondary.write_all(&[*b]).expect("write live");
            tty_secondary.flush().ok();
            std::thread::sleep(Duration::from_millis(1));
        }

        // Phase 4: drain echoes.
        set_nonblocking(&tty_secondary).expect("set nonblocking");
        std::thread::sleep(Duration::from_millis(500));
        let echoed = drain_fd_nonblock(&mut tty_secondary);
        drop(tty_secondary);

        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "relay_drain_inject_boundary: relay exited {status:?}, \
                 pre_inject={PRE_INJECT} live_inject={LIVE_INJECT} echoed={}",
                    echoed.len()
                );
            }
            Ok(Err(e)) => panic!("relay_drain_inject_boundary: relay error: {e}"),
            Err(_) => panic!("relay_drain_inject_boundary: relay thread panicked"),
        }

        assert_eq!(
            echoed.len(),
            TOTAL,
            "drain-inject boundary: expected {TOTAL} echoed bytes, got {}",
            echoed.len()
        );

        let mut echoed_sorted = echoed.clone();
        echoed_sorted.sort();
        let mut expected: Vec<u8> = Vec::with_capacity(TOTAL);
        expected.extend_from_slice(&pre_bytes);
        expected.extend_from_slice(&live_bytes);
        expected.sort();
        assert_eq!(
            echoed_sorted, expected,
            "drain-inject boundary: echo mismatch"
        );
    }
}
