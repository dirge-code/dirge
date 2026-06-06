//! Shared PTY helpers for relay integration tests.
//! Included via `#[cfg(test)]` in the `relay_tests` module.

use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::time::Duration;

/// Guard for tests that manipulate fd 0 or crossterm global state.
pub(super) static FD_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn serial_fd_test() -> std::sync::MutexGuard<'static, ()> {
    FD_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open a PTY pair and return (primary, secondary) as `File`s.
pub(super) fn open_pty_pair() -> Option<(std::fs::File, std::fs::File)> {
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
    Some((primary, secondary))
}

/// Set raw mode (cfmakeraw) on an fd.
pub(super) fn make_raw_fd(fd: std::os::unix::io::RawFd) -> io::Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read all available bytes from a non-blocking fd.
pub(super) fn drain_fd_nonblock(fd: &mut std::fs::File) -> Vec<u8> {
    let mut buf = [0u8; 4096];
    let mut all = Vec::new();
    loop {
        match fd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    all
}

/// Switch a fd to O_NONBLOCK.
pub(super) fn set_nonblocking(fd: &std::fs::File) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Shared harness: spawn `cat -u` on PTY, wire fake tty, run relay in
/// thread, inject bytes via closure, and assert all were echoed.
pub(super) fn run_relay_test(test_name: &str, inject: fn(&mut std::fs::File) -> Vec<u8>) {
    let mut guest_cmd = std::process::Command::new("cat");
    guest_cmd.arg("-u");
    let relay = crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd).expect("spawn cat on PTY");
    relay.disable_guest_echo();

    let (tty_primary, mut tty_secondary) = open_pty_pair().expect("open fake tty PTY");
    make_raw_fd(tty_secondary.as_raw_fd()).expect("raw mode on tty secondary");

    let relay_handle = std::thread::spawn(move || relay.relay_to_fd(tty_primary));

    let injected = inject(&mut tty_secondary);

    set_nonblocking(&tty_secondary).expect("set nonblocking for drain");
    std::thread::sleep(Duration::from_millis(200));

    let echoed = drain_fd_nonblock(&mut tty_secondary);
    drop(tty_secondary);

    match relay_handle.join() {
        Ok(Ok(status)) => {
            eprintln!(
                "{test_name}: relay exited {status:?}, injected={} echoed={}",
                injected.len(),
                echoed.len()
            );
        }
        Ok(Err(e)) => panic!("{test_name}: relay error: {e}"),
        Err(_) => panic!("{test_name}: relay thread panicked"),
    }

    let mut echoed_sorted = echoed.clone();
    echoed_sorted.sort();
    let mut injected_sorted = injected.clone();
    injected_sorted.sort();
    assert_eq!(
        echoed_sorted,
        injected_sorted,
        "{test_name}: echo mismatch: injected {} bytes, got {} back",
        injected.len(),
        echoed.len()
    );
}

/// Shared harness for event-channel stress: spawns a producer thread
/// sending `count` events at `gap_us` microsecond intervals, then
/// verifies every send is matched by a receive.
pub(super) fn channel_stress(test_name: &str, count: usize, gap_us: u64) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::event::UserEvent>();

    let sent = Arc::new(AtomicUsize::new(0));
    let sent2 = Arc::clone(&sent);
    let done = Arc::new(AtomicBool::new(false));
    let done2 = Arc::clone(&done);

    let producer = std::thread::spawn(move || {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for i in 0..count {
            let ev = crate::event::UserEvent::Key(KeyEvent::new(
                KeyCode::Char((b'a' + (i % 26) as u8) as char),
                KeyModifiers::NONE,
            ));
            if tx.send(ev).is_err() {
                break;
            }
            sent2.fetch_add(1, Ordering::Relaxed);
            if gap_us > 0 {
                std::thread::sleep(Duration::from_micros(gap_us));
            }
        }
        done2.store(true, Ordering::Relaxed);
    });

    let received = Arc::new(AtomicUsize::new(0));
    let received2 = Arc::clone(&received);
    let sent_for_consumer = Arc::clone(&sent);
    let consumer = std::thread::spawn(move || {
        loop {
            match rx.try_recv() {
                Ok(_) => {
                    received2.fetch_add(1, Ordering::Relaxed);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if done.load(Ordering::Relaxed)
                        && received2.load(Ordering::Relaxed)
                            >= sent_for_consumer.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    std::thread::yield_now();
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    });

    let _ = producer.join();
    let _ = consumer.join();

    let sent_val = sent.load(Ordering::Relaxed);
    let received_val = received.load(Ordering::Relaxed);
    assert_eq!(
        received_val, sent_val,
        "{test_name}: sent {sent_val} events, received {received_val}"
    );
    assert_eq!(
        sent_val, count,
        "{test_name}: expected {count} events, sent {sent_val}"
    );
}
