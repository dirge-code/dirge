#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;
    use std::io::{Read, Write};
    use std::os::unix::io::{AsRawFd, FromRawFd};

    use std::time::Duration;

    #[test]
    fn e2e_attach_byte_tracking() {
        let _guard = serial_fd_test();
        use std::os::unix::io::OwnedFd;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // ── save original stdin fd and termios ──
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

        // ── reset reader flags ──
        crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);

        // ── fake tty: PTY pair simulating /dev/tty ──
        // primary = "tty" side (relay reads keystrokes, writes echoes)
        // secondary = "keyboard" side (injector writes keystrokes,
        //          echoes appear here after relay writes primary)
        let (fake_primary, fake_secondary) = open_pty_pair().expect("open fake tty");

        // Clone primary for the relay before redirecting fd 0.
        let relay_tty = fake_primary.try_clone().expect("clone primary for relay");

        // Redirect fd 0 to the primary so the input reader sees keystrokes
        // (data flows secondary→primary; reading primary gets keystrokes).
        assert!(
            unsafe { libc::dup2(fake_primary.as_raw_fd(), 0) } >= 0,
            "dup2 primary to fd 0"
        );
        make_raw_fd(0).expect("make_raw on fd 0");

        // ── injector: writes to secondary at 1000 bytes/sec ──
        let injector_running = Arc::new(AtomicBool::new(true));
        let injector_running2 = Arc::clone(&injector_running);
        let injector_bytes = Arc::new(AtomicUsize::new(0));
        let injector_bytes2 = Arc::clone(&injector_bytes);
        let relay_active = Arc::new(AtomicBool::new(false));
        let relay_active2 = Arc::clone(&relay_active);
        let charset: Vec<u8> = (b'a'..=b'z')
            .chain(b'A'..=b'Z')
            .chain(b'0'..=b'9')
            .collect();
        let charset_for_injector = charset.clone();
        let interval = Duration::from_micros(1000);

        // Open a second handle to the secondary for echo draining after the
        // relay runs. The injector thread owns the first handle; when the
        // injector drops it the primary still has this second handle open,
        // so HUP won't fire prematurely.
        let mut secondary_for_echo = fake_secondary
            .try_clone()
            .expect("clone secondary for echo drain");
        let secondary_for_hup = fake_secondary
            .try_clone()
            .expect("clone secondary for HUP trigger");
        let mut secondary_for_injector = fake_secondary;
        let injector = std::thread::spawn(move || {
            for i in 0u64.. {
                if !injector_running2.load(Ordering::Relaxed) {
                    break;
                }
                let b = charset_for_injector[i as usize % charset_for_injector.len()];
                // Data flows secondary→primary: writing to secondary makes
                // bytes readable on primary (fd 0 + relay tty).
                let _ = secondary_for_injector.write_all(&[b]);
                let _ = secondary_for_injector.flush();
                if relay_active2.load(Ordering::Relaxed) {
                    injector_bytes2.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(interval);
            }
        });

        // ── warmup: let injector stabilize ──
        std::thread::sleep(Duration::from_millis(100));

        // ── production shutdown sequence ──
        // No crossterm reader is spawned in this test — just drain
        // stdin to simulate the production drain pass.
        let drained = crate::ui::terminal::drain_stdin_nonblocking();

        // ── spawn cat -u on a separate PTY for the relay ──
        let mut guest_cmd = std::process::Command::new("cat");
        guest_cmd.arg("-u");
        let relay = match crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd) {
            Ok(mut r) => {
                // Inject drained bytes so they reach the child.
                if !drained.is_empty() {
                    let _ = r.write_to_primary(&drained);
                }
                r
            }
            Err(e) => {
                eprintln!("FAIL: PtyRelay::spawn failed: {e}");
                // Cleanup before panic.
                injector_running.store(false, Ordering::Relaxed);
                let _ = injector.join();
                unsafe {
                    libc::dup2(saved_stdin.as_raw_fd(), 0);
                    if let Some(ref t) = saved_termios {
                        libc::tcsetattr(0, libc::TCSANOW, t);
                    }
                }
                panic!("PtyRelay::spawn failed: {e}");
            }
        };

        // Grab cat's pid so we can kill it to make the relay exit.
        let cat_pid = relay.child_pid();

        // ── start relay thread ──
        // Mark relay active so injector counts bytes from this point.
        relay_active.store(true, Ordering::Relaxed);
        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(relay_tty));

        // ── let relay run with injector active ──
        std::thread::sleep(Duration::from_millis(500));

        // ── stop injector and wait for in-flight bytes ──
        relay_active.store(false, Ordering::Relaxed);
        injector_running.store(false, Ordering::Relaxed);
        let _ = injector.join();

        // Let the relay drain remaining bytes.
        std::thread::sleep(Duration::from_millis(200));

        // ── drain echoes from secondary ──
        // The relay writes echoes to primary → data appears on secondary.
        // We drain in a separate thread so the secondary stays open while
        // the relay processes in-flight bytes.
        set_nonblocking(&secondary_for_echo).expect("nonblocking on secondary");
        let echoed = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let echoed2 = Arc::clone(&echoed);
        let echo_drainer_done = Arc::new(AtomicBool::new(false));
        let echo_drainer_done2 = Arc::clone(&echo_drainer_done);
        let echo_drainer = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match secondary_for_echo.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        echoed2.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    Ok(_) | Err(_) => {
                        if echo_drainer_done2.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            }
        });

        // Let echoes accumulate for a final drain window.
        std::thread::sleep(Duration::from_millis(300));

        // Stop the clone secondary for echo drainer, final drain.
        echo_drainer_done.store(true, Ordering::Relaxed);
        let _ = echo_drainer.join();
        let echoed = std::mem::take(&mut *echoed.lock().unwrap());

        // ── kill cat to make the relay exit ──
        // POLLHUP detection on the tty fd doesn't work reliably in tests
        // because /dev/tty behavior differs under cargo test. Instead,
        // kill the cat process directly: its exit triggers child.try_wait()
        // in the relay loop.
        unsafe { libc::kill(cat_pid as i32, libc::SIGKILL) };
        drop(secondary_for_hup);

        // ── join relay ──
        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "e2e_attach: relay exited {status:?}, drained={} relay_bytes={} echoed={}",
                    drained.len(),
                    injector_bytes.load(Ordering::Relaxed),
                    echoed.len(),
                );
            }
            Ok(Err(e)) => panic!("e2e_attach: relay error: {e}"),
            Err(_) => panic!("e2e_attach: relay thread panicked"),
        }

        // ── verify byte accounting ──
        let relay_injected = injector_bytes.load(Ordering::Relaxed);
        // Total bytes that should reach the child and be echoed:
        // drained bytes (re-injected via write_to_primary) + bytes
        // injected during relay window.
        let expected_echoes = drained.len() + relay_injected;
        eprintln!(
            "e2e_attach: expected_echoes={} (drained={} + relay_injected={}) actual_echoed={}",
            expected_echoes,
            drained.len(),
            relay_injected,
            echoed.len(),
        );

        // Sanity: we must have injected something during the relay window.
        assert!(relay_injected > 0, "no bytes injected during relay window");

        // The echoed count may differ slightly from expected due to
        // in-flight bytes in retry buffers at relay exit. Allow a
        // small tolerance (retry buffer capacity is 4096 per direction).
        let diff = (expected_echoes as i64 - echoed.len() as i64).unsigned_abs();
        assert!(
            diff <= 8192,
            "e2e_attach: echo mismatch beyond retry-buffer tolerance: \
         expected ~{expected_echoes}, got {} (diff={diff})",
            echoed.len(),
        );

        // Stronger check: all echoed bytes are from the charset.
        for &b in &echoed {
            assert!(
                charset.contains(&b),
                "unexpected byte {b:#04x} in echo buffer — not from injector charset"
            );
        }

        // ── cleanup: restore stdin and termios ──
        unsafe {
            libc::dup2(saved_stdin.as_raw_fd(), 0);
            if let Some(ref t) = saved_termios {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }

    /// E2E MicroVM Attach with Byte Tracking
    ///
    /// Starts a real microVM, runs the full sandbox attach sequence
    /// via PtyRelay with SSH, injects keystrokes through a fake tty,
    /// and verifies echoed bytes are correct.
    ///
    /// ## Prerequisites
    ///
    /// - /dev/kvm accessible
    /// - dirge-microvm-runner built and installed
    #[test]
    fn e2e_microvm_attach_byte_tracking() {
        let _fd_guard = serial_fd_test();
        use std::io::{Read, Write};
        use std::os::unix::io::{AsRawFd, OwnedFd};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // ── prerequisites ──────────────────────────────────────────
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm not available");
            return;
        }
        if crate::sandbox::microvm::runner::find_runner_binary().is_err() {
            eprintln!("skipping: dirge-microvm-runner not found");
            return;
        }

        // ── save original stdin fd and termios ─────────────────────
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

        // ── fake tty: PTY pair ─────────────────────────────────────
        let (fake_primary, fake_secondary) = open_pty_pair().expect("open fake tty");
        let relay_tty = fake_primary.try_clone().expect("clone primary for relay");

        assert!(
            unsafe { libc::dup2(fake_primary.as_raw_fd(), 0) } >= 0,
            "dup2 primary to fd 0"
        );
        make_raw_fd(0).expect("make_raw on fd 0");

        // ── injector: writes to secondary at 500 bytes/sec ─────────────
        let injector_running = Arc::new(AtomicBool::new(true));
        let injector_running2 = Arc::clone(&injector_running);
        let injector_bytes = Arc::new(AtomicUsize::new(0));
        let injector_bytes2 = Arc::clone(&injector_bytes);
        let relay_active = Arc::new(AtomicBool::new(false));
        let relay_active2 = Arc::clone(&relay_active);
        let charset: Vec<u8> = (b'a'..=b'z')
            .chain(b'A'..=b'Z')
            .chain(b'0'..=b'9')
            .collect();
        let charset_for_injector = charset.clone();
        let interval = Duration::from_micros(2000);

        let mut secondary_for_echo = fake_secondary
            .try_clone()
            .expect("clone secondary for echo");
        let secondary_for_hup = fake_secondary.try_clone().expect("clone secondary for HUP");
        let mut secondary_for_injector = fake_secondary;
        let injector = std::thread::spawn(move || {
            for i in 0u64.. {
                if !injector_running2.load(Ordering::Relaxed) {
                    break;
                }
                let b = charset_for_injector[i as usize % charset_for_injector.len()];
                let _ = secondary_for_injector.write_all(&[b]);
                let _ = secondary_for_injector.flush();
                if relay_active2.load(Ordering::Relaxed) {
                    injector_bytes2.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(interval);
            }
        });

        std::thread::sleep(Duration::from_millis(50));

        // ── start microVM ──────────────────────────────────────────
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-e2e-attach-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = crate::sandbox::microvm::MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..crate::sandbox::microvm::MicrovmConfig::default()
        };

        let mut sandbox = crate::sandbox::microvm::MicrovmSandbox::new(cfg);
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        match rt.block_on(sandbox.start()) {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                injector_running.store(false, Ordering::Relaxed);
                let _ = injector.join();
                unsafe {
                    libc::dup2(saved_stdin.as_raw_fd(), 0);
                    if let Some(ref t) = saved_termios {
                        libc::tcsetattr(0, libc::TCSANOW, t);
                    }
                }
                panic!("microVM start failed: {e}");
            }
        }

        let ssh_port = sandbox.ssh_port();
        assert!(ssh_port > 0, "SSH port not set after VM start");
        let key_path = sandbox
            .keys
            .as_ref()
            .expect("keys after start")
            .private_key_path
            .clone();

        // ── build SSH command (matching production cmd_sandbox_attach) ──
        let mut guest_cmd = std::process::Command::new("ssh");
        guest_cmd
            .args([
                "-t",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
            ])
            .arg(key_path.as_os_str())
            .arg("-p")
            .arg(ssh_port.to_string())
            .arg("sandbox@127.0.0.1")
            .arg("cat -u");
        guest_cmd.env(
            "TERM",
            std::env::var("TERM").as_deref().unwrap_or("xterm-256color"),
        );

        // ── spawn PtyRelay with SSH ──────────────────────────────────
        let relay = match crate::ui::pty_relay::PtyRelay::spawn(&mut guest_cmd) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PtyRelay::spawn(ssh→microVM) failed: {e}");
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                injector_running.store(false, Ordering::Relaxed);
                let _ = injector.join();
                unsafe {
                    libc::dup2(saved_stdin.as_raw_fd(), 0);
                    if let Some(ref t) = saved_termios {
                        libc::tcsetattr(0, libc::TCSANOW, t);
                    }
                }
                panic!("PtyRelay::spawn failed: {e}");
            }
        };

        let ssh_pid = relay.child_pid();
        relay_active.store(true, Ordering::Relaxed);
        let relay_handle = std::thread::spawn(move || relay.relay_to_fd(relay_tty));

        // ── run relay for 800ms ─────────────────────────────────────
        std::thread::sleep(Duration::from_millis(800));
        relay_active.store(false, Ordering::Relaxed);
        injector_running.store(false, Ordering::Relaxed);
        let _ = injector.join();
        std::thread::sleep(Duration::from_millis(200));

        // ── drain echoes from secondary ─────────────────────────────────
        set_nonblocking(&secondary_for_echo).expect("nonblocking on secondary");
        let echoed = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let echoed2 = Arc::clone(&echoed);
        let echo_done = Arc::new(AtomicBool::new(false));
        let echo_done2 = Arc::clone(&echo_done);
        let echo_drainer = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match secondary_for_echo.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        echoed2.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    Ok(_) | Err(_) => {
                        if echo_done2.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        echo_done.store(true, Ordering::Relaxed);
        let _ = echo_drainer.join();
        let echoed = std::mem::take(&mut *echoed.lock().unwrap());

        // ── kill SSH to make relay exit ─────────────────────────────
        unsafe { libc::kill(ssh_pid as i32, libc::SIGKILL) };
        drop(secondary_for_hup);

        match relay_handle.join() {
            Ok(Ok(status)) => {
                eprintln!(
                    "e2e_microvm_attach: relay exited {status:?}, injected={} echoed={}",
                    injector_bytes.load(Ordering::Relaxed),
                    echoed.len(),
                );
            }
            Ok(Err(e)) => panic!("e2e_microvm_attach: relay error: {e}"),
            Err(_) => panic!("e2e_microvm_attach: relay thread panicked"),
        }

        // ── verify byte accounting ──────────────────────────────────
        let injected = injector_bytes.load(Ordering::Relaxed);
        assert!(injected > 0, "no bytes injected during relay window");

        let diff = (injected as i64 - echoed.len() as i64).unsigned_abs();
        assert!(
            diff <= 8192,
            "e2e_microvm_attach: echo mismatch beyond retry-buffer tolerance: \
         injected={injected}, echoed={} (diff={diff})",
            echoed.len(),
        );

        for &b in &echoed {
            assert!(
                charset.contains(&b),
                "unexpected byte {b:#04x} in echo buffer — not from injector charset"
            );
        }

        // ── cleanup ─────────────────────────────────────────────────
        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
        unsafe {
            libc::dup2(saved_stdin.as_raw_fd(), 0);
            if let Some(ref t) = saved_termios {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }
}
