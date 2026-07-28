//! Tests that the relay exits promptly (does not spin) when the tty
//! side hangs up or when a non-PTY fd like /dev/null is used as the tty.
//!
//! These tests reproduce the CPU-spin regression introduced by commit
//! 4522cdbf: `Ok(0) => continue` on the tty read path kept the loop
//! alive after TTY EOF, and missing POLLNVAL in the fatal-revents mask
//! left /dev/null unrecognised as a dead fd.

#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::time::{Duration, Instant};

    /// When the tty side hangs up (e.g. the user closes their terminal),
    /// the relay must kill the child and exit rather than spin forever
    /// re-reading EOF from the dead fd.
    #[test]
    fn tty_hangup_relay_exits() {
        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let (tty_primary, tty_secondary) = open_pty_pair().expect("open fake tty PTY");

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

        // Give the relay a moment to enter the poll loop, then hang up.
        std::thread::sleep(Duration::from_millis(100));
        drop(tty_secondary);

        // The relay must exit within 5 s — with the bug it spins forever.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if relay_handle.is_finished() {
                break;
            }
            if Instant::now() > deadline {
                panic!("relay did not exit after tty hangup (spin/hang)");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        match relay_handle.join().expect("relay thread panicked") {
            Ok(_status) => {} // child was killed, status doesn't matter
            Err(e) => panic!("relay error after tty hangup: {e}"),
        }
    }

    /// When a non-PTY fd like /dev/null is passed as the tty, poll(2)
    /// may return POLLNVAL. The relay must treat POLLNVAL as fatal on
    /// both fds and exit promptly instead of spinning in a tight loop.
    #[test]
    fn dev_null_tty_relay_exits() {
        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay =
            crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
        relay.disable_guest_echo();

        let null_tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");

        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(null_tty));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if relay_handle.is_finished() {
                break;
            }
            if Instant::now() > deadline {
                panic!("relay did not exit with /dev/null tty (spin)");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        match relay_handle.join().expect("relay thread panicked") {
            Ok(_status) => {}
            Err(e) => panic!("relay error with /dev/null tty: {e}"),
        }
    }
}
