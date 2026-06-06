#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    #[test]
    fn relay_final_drain_no_loss() {
        // Spawn bash -c that prints a known string and exits quickly.
        let mut guest_cmd = std::process::Command::new("bash");
        guest_cmd.arg("-c").arg("echo FINAL_LINE; exit 0");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn bash on PTY");

        let (tty_primary, mut tty_secondary) = open_pty_pair().expect("open fake tty PTY");
        // Don't set raw mode on tty_secondary — its termios only affects
        // input processing from the local side, which we don't use.
        set_nonblocking(&tty_secondary).expect("set nonblocking for drain");

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        // Drain while the relay is running and after it exits.
        let mut output = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..50 {
            loop {
                match tty_secondary.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if relay_handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // One last drain after exit.
        loop {
            match tty_secondary.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        drop(tty_secondary);

        let status = match relay_handle.join() {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => panic!("relay_final_drain_no_loss: relay error: {e}"),
            Err(_) => panic!("relay_final_drain_no_loss: relay thread panicked"),
        };

        let output_str = String::from_utf8_lossy(&output);
        eprintln!(
            "relay_final_drain_no_loss: relay exited {status:?}, output_len={} output={output_str:?}",
            output.len()
        );

        assert!(
            output_str.contains("FINAL_LINE"),
            "final drain: expected FINAL_LINE in output, got {output_str:?}"
        );
    }

    // ── Test 2.5b: ONLCR Newline Translation ────────────────────────
    //
    // Verify that make_raw() + ONLCR translates \n → \r\n on output.
    // cfmakeraw clears OPOST (which includes ONLCR), but the fix
    // re-enables OPOST|ONLCR so the PTY line discipline converts
    // bare newlines to CR+NL for the terminal.

    #[test]
    fn relay_nl_cr_roundtrip() {
        let (mut pty_primary, pty_secondary) =
            open_pty_pair().expect("open PTY pair for NL→CR test");

        // Apply production raw mode + ONLCR on the secondary.
        crate::ui::pty_relay::make_raw(pty_secondary.as_raw_fd())
            .expect("make_raw on PTY secondary");

        // Verify ONLCR is set in termios.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        assert!(
            unsafe { libc::tcgetattr(pty_secondary.as_raw_fd(), &mut termios) } >= 0,
            "tcgetattr after make_raw"
        );
        assert!(
            termios.c_oflag & libc::ONLCR != 0,
            "ONLCR must be set by make_raw: c_oflag={:#x}",
            termios.c_oflag
        );

        // ONLCR is an output-processing flag: it converts \n → \r\n
        // when the application WRITES to the PTY secondary (stdout).
        // So we write to secondary, read from primary.
        use std::io::Write;
        let mut secondary = pty_secondary;
        secondary.write_all(b"X\n").expect("write to PTY secondary");
        secondary.flush().expect("flush secondary");

        // Read from primary — should receive "X\r\n" (3 bytes).
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut buf = [0u8; 16];
        let n = pty_primary.read(&mut buf).expect("read from PTY primary");
        assert_eq!(
            &buf[..n],
            b"X\r\n",
            "ONLCR: expected X\\r\\n from primary, got {:?}",
            String::from_utf8_lossy(&buf[..n])
        );
    }
}
