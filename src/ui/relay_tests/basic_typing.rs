#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Write;
    use std::time::Duration;

    // ── Test 2.1: Single Keystrokes (slow typist) ──────────────────

    #[test]
    fn relay_single_keystrokes() {
        const COUNT: usize = 200;
        const GAP_MS: u64 = 5;

        run_relay_test("relay_single_keystrokes", |tty| {
            let mut injected = Vec::with_capacity(COUNT);
            for i in 0..COUNT {
                let b = b'a' + (i % 26) as u8;
                injected.push(b);
                tty.write_all(&[b]).expect("write");
                tty.flush().ok();
                std::thread::sleep(Duration::from_millis(GAP_MS));
            }
            injected
        });
    }

    // ── Test 2.2: Burst Typing (fast typist) ───────────────────────

    #[test]
    fn relay_burst_typing() {
        const BURSTS: usize = 100;
        const GAP_MS: u64 = 5;

        run_relay_test("relay_burst_typing", |tty| {
            let mut injected = Vec::with_capacity(BURSTS * 8);
            for b_idx in 0..BURSTS {
                let burst_size = 5 + (b_idx % 6); // 5..=10 bytes
                for j in 0..burst_size {
                    let b = b'a' + ((b_idx * 10 + j) % 26) as u8;
                    injected.push(b);
                    tty.write_all(&[b]).expect("write burst");
                }
                tty.flush().ok();
                std::thread::sleep(Duration::from_millis(GAP_MS));
            }
            injected
        });
    }
}
