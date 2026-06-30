//! PTY relay: isolate a subprocess (SSH) on a pseudo-terminal pair so it
//! never touches `/dev/tty` directly. A relay loop copies bytes between
//! the PTY primary and the real terminal until the child exits.
//!
//! This replaces the previous approach of giving SSH direct `/dev/tty`
//! access, which caused terminal corruption because crossterm's raw-mode
//! event reader, SSH's cooked-mode I/O, and the TUI's alt-screen state
//! machine all fought over the same file descriptors.
//!
//! ```text
//! SSH ──→ PTY secondary (cooked, line discipline)
//!           ↕
//!        PTY primary
//!           ↕
//!     relay thread ──→ /dev/tty (exclusive owner during SSH session)
//! ```

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

/// Holds a PTY pair with a spawned child whose stdin/stdout/stderr are
/// attached to the PTY secondary.
pub(crate) struct PtyRelay {
    primary: std::fs::File,
    /// Held alive so the PTY secondary stays open; its raw fd is
    /// stored in `secondary_raw_fd` for the deferred ECHO-disable
    /// after SSH completes its PTY allocation.
    _secondary: std::fs::File,
    secondary_raw_fd: libc::c_int,
    child: std::process::Child,
    counters: RelayCounters,
    /// When set, the relay records every byte the child writes to the PTY
    /// here. Used by the interactive bang-command path to capture output
    /// for the agent while still streaming it live to the user's terminal.
    capture: Option<Vec<u8>>,
}

// ── relay byte accounting (timing-diagnostics feature) ───────────

#[cfg(feature = "timing-diagnostics")]
mod counters {
    /// Byte-accounting counters for the relay loop. Asserted on drop
    /// to catch lost bytes in either direction.
    pub(crate) struct RelayCounters {
        pub tty_bytes_read: u64,
        pub pty_bytes_written: u64,
        pub pty_bytes_read: u64,
        pub tty_bytes_written: u64,
        pub wouldblock_count: u64,
        pub poll_count: u64,
        /// Bytes injected via write_to_primary (drain-and-reinject).
        /// These came from tty but bypassed the relay read path.
        pub drain_injected: u64,
    }

    impl RelayCounters {
        pub fn new() -> Self {
            Self {
                tty_bytes_read: 0,
                pty_bytes_written: 0,
                pty_bytes_read: 0,
                tty_bytes_written: 0,
                wouldblock_count: 0,
                poll_count: 0,
                drain_injected: 0,
            }
        }
        pub fn poll(&mut self) {
            self.poll_count += 1;
        }
        pub fn tty_read(&mut self, n: usize) {
            self.tty_bytes_read += n as u64;
        }
        pub fn pty_read(&mut self, n: usize) {
            self.pty_bytes_read += n as u64;
        }
        pub fn tty_write(&mut self, n: usize) {
            self.tty_bytes_written += n as u64;
        }
        pub fn pty_write(&mut self, n: usize) {
            self.pty_bytes_written += n as u64;
        }
        pub fn wouldblock(&mut self) {
            self.wouldblock_count += 1;
        }
        pub fn drain_injected(&mut self, n: usize) {
            self.drain_injected += n as u64;
        }
    }

    impl Drop for RelayCounters {
        fn drop(&mut self) {
            let tty_loss = self.tty_bytes_read.saturating_sub(self.pty_bytes_written);
            let pty_loss = self.pty_bytes_read.saturating_sub(self.tty_bytes_written);
            eprintln!(
                "[timing-diagnostics] relay exit: \
                 tty_read={} pty_written={} tty_loss={} \
                 pty_read={} tty_written={} pty_loss={} \
                 wouldblock={} poll={}",
                self.tty_bytes_read,
                self.pty_bytes_written,
                tty_loss,
                self.pty_bytes_read,
                self.tty_bytes_written,
                pty_loss,
                self.wouldblock_count,
                self.poll_count,
            );
            // Assert no loss beyond retry-buffer pending bytes.
            // tty_loss > 0 means we read keystrokes from tty that never
            // reached the PTY (either in flight or lost).> 0 is expected
            // if retry buffers have pending bytes at exit; stress tests
            // check exact equality after waiting for buffers to drain.
            assert!(
                self.tty_bytes_read >= self.pty_bytes_written,
                "RELAY LOSS tty→PTY: read {} bytes from tty, wrote {} to PTY (loss {})",
                self.tty_bytes_read,
                self.pty_bytes_written,
                tty_loss,
            );
            assert!(
                self.pty_bytes_read >= self.tty_bytes_written,
                "RELAY LOSS PTY→tty: read {} bytes from PTY, wrote {} to tty (loss {})",
                self.pty_bytes_read,
                self.tty_bytes_written,
                pty_loss,
            );
        }
    }
}

#[cfg(not(feature = "timing-diagnostics"))]
mod counters {
    /// Zero-cost stub: all methods compile to nothing.
    pub(crate) struct RelayCounters;
    impl RelayCounters {
        #[inline(always)]
        pub fn new() -> Self {
            Self
        }
        #[inline(always)]
        pub fn poll(&mut self) {}
        #[inline(always)]
        pub fn tty_read(&mut self, _n: usize) {}
        #[inline(always)]
        pub fn pty_read(&mut self, _n: usize) {}
        #[inline(always)]
        pub fn tty_write(&mut self, _n: usize) {}
        #[inline(always)]
        pub fn pty_write(&mut self, _n: usize) {}
        #[inline(always)]
        pub fn wouldblock(&mut self) {}
        #[inline(always)]
        pub fn drain_injected(&mut self, _n: usize) {}
    }
}

use counters::RelayCounters;

impl PtyRelay {
    /// Create a PTY pair, spawn `cmd` with the PTY secondary as
    /// stdin/stdout/stderr, and return a `PtyRelay` ready to relay I/O.
    pub(crate) fn spawn(cmd: &mut std::process::Command) -> io::Result<Self> {
        // ── open PTY ────────────────────────────────────────────────
        let primary_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if primary_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::grantpt(primary_fd) } < 0 || unsafe { libc::unlockpt(primary_fd) } < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(primary_fd) };
            return Err(e);
        }
        let secondary_name = unsafe { libc::ptsname(primary_fd) };
        if secondary_name.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(primary_fd) };
            return Err(e);
        }
        let secondary_fd = unsafe { libc::open(secondary_name, libc::O_RDWR | libc::O_NOCTTY) };
        if secondary_fd < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(primary_fd) };
            return Err(e);
        }

        let primary = unsafe { std::fs::File::from_raw_fd(primary_fd) };
        let secondary = unsafe { std::fs::File::from_raw_fd(secondary_fd) };

        let secondary_raw_fd = secondary.as_raw_fd();

        // Set raw mode on the PTY secondary so the line discipline
        // doesn't echo or buffer — the relay owns all I/O processing.
        make_raw(secondary_raw_fd)?;

        // Temporarily re-enable ECHO so the SSH client reads ECHO=1
        // during its PTY allocation request (RFC 4254 §8). ECHO is
        // disabled again in the relay loop after the first data transfer
        // (guaranteeing SSH has completed its PTY allocation).
        {
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(secondary_raw_fd, &mut termios) } == 0 {
                termios.c_lflag |= libc::ECHO;
                unsafe { libc::tcsetattr(secondary_raw_fd, libc::TCSANOW, &termios) };
            }
        }

        // ── spawn child on PTY secondary ─────────────────────────────
        let secondary_dup_in = secondary.try_clone()?;
        let secondary_dup_out = secondary.try_clone()?;
        let secondary_dup_err = secondary.try_clone()?;
        // SAFETY: we own these fds; the child inherits them.
        unsafe {
            cmd.stdin(stdio_from_fd(secondary_dup_in));
            cmd.stdout(stdio_from_fd(secondary_dup_out));
            cmd.stderr(stdio_from_fd(secondary_dup_err));
        }

        let child = cmd.spawn()?;

        Ok(PtyRelay {
            primary,
            _secondary: secondary,
            secondary_raw_fd,
            child,
            counters: RelayCounters::new(),
            capture: None,
        })
    }

    /// Disable ECHO on the guest PTY secondary now, rather than waiting
    /// for the first data transfer. Call this when the command is known
    /// to not need the ECHO=1 handshake (e.g. `cat -u` in tests).
    #[cfg(all(test, feature = "sandbox-microvm"))]
    pub(crate) fn disable_guest_echo(&self) {
        disable_echo(self.secondary_raw_fd);
    }

    /// Write bytes to the PTY primary. Used to inject keystrokes
    /// drained from stdin before the relay was started, so keys
    /// typed during the TUI suspend window aren't lost.
    pub(crate) fn write_to_primary(&mut self, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.primary.write_all(data)?;
        self.counters.drain_injected(data.len());
        Ok(())
    }

    /// Return the child process id. Used in tests to externally
    /// kill the child to make the relay exit.
    #[cfg(all(test, feature = "sandbox-microvm"))]
    pub(crate) fn child_pid(&self) -> u32 {
        self.child.id()
    }

    /// Block until the child exits, relaying I/O between PTY primary and
    /// `/dev/tty`. Returns the child's exit status.
    ///
    /// Uses `poll(2)` with retry buffers. When a non-blocking `write`
    /// returns `WouldBlock` (other side isn't draining fast enough),
    /// unsent bytes are queued and retried on the next poll iteration.
    pub(crate) fn relay(self) -> io::Result<std::process::ExitStatus> {
        // ── relay priority: below input reader (-20), above KVM (19) ──
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, -19);
        }
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        self.relay_to_fd(tty)
    }

    /// Same as [`relay`] but takes an explicit tty file descriptor instead
    /// of opening `/dev/tty`. Used by stress tests that inject keystrokes
    /// through a PTY pair instead of a real terminal.
    pub(crate) fn relay_to_fd(
        mut self,
        mut tty: std::fs::File,
    ) -> io::Result<std::process::ExitStatus> {
        self.run_loop(&mut tty)
    }

    /// Run the relay loop against an explicit tty. When `self.capture` is
    /// `Some`, record every byte the child writes to the PTY so callers can
    /// feed it back. Shared core of [`PtyRelay::relay`] / [`relay_to_fd`].
    fn run_loop(&mut self, tty: &mut std::fs::File) -> io::Result<std::process::ExitStatus> {
        #[cfg(feature = "timing-diagnostics")]
        let t_relay_enter = std::time::Instant::now();

        set_pty_winsize(&self.primary)?;

        let counters = &mut self.counters;

        let pty_fd = self.primary.as_raw_fd();
        let tty_fd = tty.as_raw_fd();

        // Both fds must be non-blocking so writes don't stall the
        // relay loop when the other side isn't draining fast enough.
        // The retry-buffer pattern handles WouldBlock on both reads
        // and writes for both fds.
        set_nonblocking(pty_fd)?;
        set_nonblocking(tty_fd)?;

        // Retry buffers: bytes queued when write hits WouldBlock.
        let mut pty_write_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut tty_write_buf: Vec<u8> = Vec::with_capacity(4096);

        let mut fds = [
            libc::pollfd {
                fd: pty_fd,
                events: 0,
                revents: 0,
            },
            libc::pollfd {
                fd: tty_fd,
                events: 0,
                revents: 0,
            },
        ];

        let mut read_buf = [0u8; 4096];

        #[cfg(feature = "timing-diagnostics")]
        let mut first_poll = true;

        // Disable ECHO on the secondary after the first data flows
        // through the relay — guarantees SSH has completed its PTY
        // allocation (RFC 4254 §8) and the remote shell will echo
        // keystrokes. Before this point ECHO=1 so SSH reads it during
        // its startup; after, ECHO=0 prevents local double-echo.
        let mut echo_disabled = false;

        let mut exit_status: Option<std::process::ExitStatus> = None;

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    exit_status = Some(status);
                    break;
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }

            #[cfg(feature = "timing-diagnostics")]
            if first_poll {
                first_poll = false;
                eprintln!(
                    "[timing-diag] relay_first_poll: {:?} after relay_enter",
                    t_relay_enter.elapsed()
                );
            }

            fds[0].events = libc::POLLIN;
            fds[1].events = libc::POLLIN;
            if !pty_write_buf.is_empty() {
                fds[0].events |= libc::POLLOUT;
            }
            if !tty_write_buf.is_empty() {
                fds[1].events |= libc::POLLOUT;
            }
            fds[0].revents = 0;
            fds[1].revents = 0;

            let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, 20) };
            counters.poll();
            if ret < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            // ── flush buffered writes first ───────────────────────
            if fds[0].revents & libc::POLLOUT != 0 && !pty_write_buf.is_empty() {
                match self.primary.write(&pty_write_buf) {
                    Ok(n) if n > 0 => {
                        counters.pty_write(n);
                        pty_write_buf.drain(..n);
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        counters.wouldblock();
                    }
                    Err(_) => break,
                }
            }
            if fds[1].revents & libc::POLLOUT != 0 && !tty_write_buf.is_empty() {
                match tty.write(&tty_write_buf) {
                    Ok(n) if n > 0 => {
                        counters.tty_write(n);
                        tty_write_buf.drain(..n);
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        counters.wouldblock();
                    }
                    Err(_) => break,
                }
            }

            // ── PTY → tty (guest output → user screen) ────────────
            if fds[0].revents & libc::POLLIN != 0 {
                match self.primary.read(&mut read_buf) {
                    Ok(0) => return self.child.wait(),
                    Ok(n) => {
                        counters.pty_read(n);
                        if let Some(c) = self.capture.as_mut() {
                            c.extend_from_slice(&read_buf[..n]);
                        }
                        if !echo_disabled {
                            echo_disabled = true;
                            disable_echo(self.secondary_raw_fd);
                        }
                        if !tty_write_buf.is_empty() {
                            match tty.write(&tty_write_buf) {
                                Ok(w) if w > 0 => {
                                    counters.tty_write(w);
                                    tty_write_buf.drain(..w);
                                }
                                Ok(_) => {}
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    counters.wouldblock();
                                }
                                Err(_) => break,
                            }
                        }
                        match tty.write(&read_buf[..n]) {
                            Ok(w) if w < n => {
                                counters.tty_write(w);
                                tty_write_buf.extend_from_slice(&read_buf[w..n]);
                            }
                            Ok(w) => {
                                counters.tty_write(w);
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                counters.wouldblock();
                                tty_write_buf.extend_from_slice(&read_buf[..n]);
                            }
                            Err(_) => break,
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return self.child.wait();
            }

            // ── tty → PTY (user keystrokes → guest) ───────────────
            if fds[1].revents & libc::POLLIN != 0 {
                match tty.read(&mut read_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        counters.tty_read(n);
                        if !pty_write_buf.is_empty() {
                            match self.primary.write(&pty_write_buf) {
                                Ok(w) if w > 0 => {
                                    counters.pty_write(w);
                                    pty_write_buf.drain(..w);
                                }
                                Ok(_) => {}
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    counters.wouldblock();
                                }
                                Err(_) => break,
                            }
                        }
                        match self.primary.write(&read_buf[..n]) {
                            Ok(w) if w < n => {
                                counters.pty_write(w);
                                pty_write_buf.extend_from_slice(&read_buf[w..n]);
                            }
                            Ok(w) => {
                                counters.pty_write(w);
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                counters.wouldblock();
                                pty_write_buf.extend_from_slice(&read_buf[..n]);
                            }
                            Err(_) => break,
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                // Tty is gone — kill the child so we don't block in wait().
                let _ = self.child.kill();
                break;
            }
        }

        // ── final drain: flush write buffers + capture last PTY output ──
        if let Some(status) = exit_status {
            // Flush both write buffers. The child has exited so no new
            // data arrives on the tty side; we just need to push out
            // any buffered bytes. Cap retries so a blocked tty doesn't
            // keep the relay alive indefinitely.
            const MAX_FLUSH_ITER: usize = 50;
            let mut flush_iter = 0;
            while (!pty_write_buf.is_empty() || !tty_write_buf.is_empty())
                && flush_iter < MAX_FLUSH_ITER
            {
                flush_iter += 1;
                fds[0].events = 0;
                fds[1].events = 0;
                if !pty_write_buf.is_empty() {
                    fds[0].events |= libc::POLLOUT;
                }
                if !tty_write_buf.is_empty() {
                    fds[1].events |= libc::POLLOUT;
                }
                fds[0].revents = 0;
                fds[1].revents = 0;
                let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, 20) };
                if ret < 0 {
                    break;
                }
                if fds[0].revents & libc::POLLOUT != 0 && !pty_write_buf.is_empty() {
                    match self.primary.write(&pty_write_buf) {
                        Ok(n) if n > 0 => {
                            pty_write_buf.drain(..n);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        _ => break,
                    }
                }
                if fds[1].revents & libc::POLLOUT != 0 && !tty_write_buf.is_empty() {
                    match tty.write(&tty_write_buf) {
                        Ok(n) if n > 0 => {
                            tty_write_buf.drain(..n);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        _ => break,
                    }
                }
            }

            // Drain any remaining PTY output the child produced between
            // the last poll iteration and its exit.
            loop {
                fds[0].events = libc::POLLIN;
                fds[0].revents = 0;
                let ret = unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) };
                if ret <= 0 {
                    break;
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    match self.primary.read(&mut read_buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Some(c) = self.capture.as_mut() {
                                c.extend_from_slice(&read_buf[..n]);
                            }
                            // Best-effort write to tty; ignore errors.
                            let _ = tty.write_all(&read_buf[..n]);
                        }
                    }
                } else {
                    break;
                }
            }

            return Ok(status);
        }

        self.child.wait()
    }
}

/// Set the PTY window size to match the real terminal, so SSH -t and
/// full-screen programs (vim, less, htop) see correct dimensions.
fn set_pty_winsize(primary: &std::fs::File) -> io::Result<()> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // Try to get the real terminal size from /dev/tty.
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).open("/dev/tty") {
        if unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } < 0 {
            // Fall through with defaults.
            ws.ws_row = 24;
            ws.ws_col = 80;
        }
    } else {
        ws.ws_row = 24;
        ws.ws_col = 80;
    }
    if unsafe { libc::ioctl(primary.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
        // Non-fatal: SSH uses a default size.
    }
    Ok(())
}

/// Set O_NONBLOCK on a file descriptor.
fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set raw mode (cfmakeraw) on a file descriptor. Disables echo,
/// canonical processing, and signal chars so the PTY line discipline
/// passes bytes through without local processing.
pub(crate) fn make_raw(fd: libc::c_int) -> io::Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    // Re-enable ONLCR so \n is translated to \r\n on output.
    // cfmakeraw clears OPOST; ONLCR requires OPOST to be set.
    termios.c_oflag |= libc::OPOST | libc::ONLCR;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Disable ECHO on an fd. Best-effort — failures are silent because
/// ECHO is cosmetic (double-echo at worst, never data loss).
fn disable_echo(fd: libc::c_int) {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } == 0 {
        termios.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
    }
}

/// Convert an owned `File` into a `Stdio` for `Command` plumbing.
/// SAFETY: the caller must ensure the fd is valid and not used elsewhere.
pub(crate) unsafe fn stdio_from_fd(file: std::fs::File) -> std::process::Stdio {
    let fd = file.as_raw_fd();
    // Leak the OwnedFd we'd get from into_raw_fd, transferring ownership
    // to the Stdio (which will close it when the child drops).
    let owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Prevent the File's Drop from closing the fd.
    std::mem::forget(file);
    owned.into()
}

#[cfg(all(unix, test))]
mod tests {
    //! These run in the default test suite (no `sandbox-microvm` feature)
    //! because they only need a PTY pair, not a microvm. They verify the
    //! output-capture path used by interactive bang commands (`!`/`!!`).

    use super::*;
    use std::os::unix::io::FromRawFd;
    use std::process::Command;

    /// Open a PTY pair (primary, secondary) via posix_openpt. Mirrors the
    /// helper in relay_tests/common.rs but kept local so this test runs
    /// without the `sandbox-microvm` feature.
    fn open_pty_pair() -> Option<(std::fs::File, std::fs::File)> {
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
        Some((unsafe { std::fs::File::from_raw_fd(primary_fd) }, unsafe {
            std::fs::File::from_raw_fd(secondary_fd)
        }))
    }

    /// `run_loop` must capture everything the child writes to the PTY so the
    /// interactive bang-command path can feed it back to the agent.
    #[test]
    fn run_loop_captures_child_stdout() {
        // A fake "tty": the primary of a second PTY pair. It stays open for
        // the relay's lifetime (its secondary is kept alive in `_sec`) so the
        // loop doesn't see an immediate POLLHUP and kill the child.
        let (tty, _sec) = match open_pty_pair() {
            Some(p) => p,
            None => return, // no PTY support in this environment
        };
        let mut tty = tty;

        let mut cmd = Command::new("bash");
        cmd.args(["-c", "printf 'CAPTURED_MARKER_42\\n'; exit 0"]);

        let mut relay = PtyRelay::spawn(&mut cmd).expect("spawn relay");
        relay.capture = Some(Vec::new());

        let status = relay.run_loop(&mut tty).expect("relay loop");
        assert!(status.success(), "child should exit 0");

        let cap = relay.capture.take().expect("capture buffer");
        let text = String::from_utf8_lossy(&cap);
        assert!(
            text.contains("CAPTURED_MARKER_42"),
            "captured output should contain the marker, got: {text:?}"
        );
    }
}
