//! /sandbox attach — SSH into the microVM sandbox.

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_sandbox_attach(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let info = match ctx.sandbox.ssh_connect_info() {
        Some(info) => info,
        None => {
            if ctx.sandbox.is_microvm() {
                ctx.renderer.write_line(
                    "VM not running yet — run a bash command first to boot the microVM.",
                    c_error(),
                )?;
            } else {
                ctx.renderer.write_line(
                    "microVM sandbox not active — start dirge with --sandbox microvm.",
                    c_error(),
                )?;
            }
            return Ok(());
        }
    };
    let (port, key_path, host_public_key) = info;

    ctx.renderer
        .write_line(&format!("connecting to VM on port {port}..."), c_agent())?;

    let known_hosts_dir =
        std::env::temp_dir().join(format!("dirge-known-hosts-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&known_hosts_dir)
        .map_err(|e| anyhow::anyhow!("failed to create temp dir for known_hosts: {e}"))?;
    let known_hosts_path = known_hosts_dir.join("known_hosts");
    std::fs::write(
        &known_hosts_path,
        format!("[127.0.0.1]:{port} {host_public_key}\n"),
    )?;

    let preflight = std::process::Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=yes",
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
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts_path.display()))
        .arg("-p")
        .arg(port.to_string())
        .arg("sandbox@127.0.0.1")
        .arg("echo ok")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    match preflight {
        Ok(ref out) if out.status.success() => {}
        Ok(ref out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            ctx.renderer.write_line(
                &format!(
                    "SSH pre-flight failed (exit {}): {}\n\
                     key: {}\n\
                     Try manually: ssh -i {} -p {} sandbox@127.0.0.1",
                    out.status.code().unwrap_or(-1),
                    stderr.trim_end(),
                    key_path.display(),
                    key_path.display(),
                    port,
                ),
                c_error(),
            )?;
            return Ok(());
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to run ssh: {e}"), c_error())?;
            return Ok(());
        }
    }

    use std::io::Write;
    use std::sync::atomic::Ordering;

    #[cfg(feature = "timing-diagnostics")]
    let t0 = std::time::Instant::now();

    crate::ui::terminal::EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
    crate::ui::terminal::join_reader(std::time::Duration::from_millis(50));

    #[cfg(feature = "timing-diagnostics")]
    {
        let elapsed = t0.elapsed();
        let reader_exited = crate::ui::terminal::EVENT_READER_EXITED.load(Ordering::Acquire);
        eprintln!(
            "[timing-diag] reader_shutdown_signal→wait_done: {:?} reader_exited={}",
            elapsed, reader_exited
        );
    }

    let drained_stdin;
    {
        let mut tty = match crate::ui::terminal::open_tty_for_write() {
            Some(f) => f,
            None => {
                crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
                crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);
                crate::ui::input_reader::spawn_input_reader(ctx.user_tx.clone());
                ctx.renderer
                    .write_line("no /dev/tty available — cannot attach", c_error())?;
                return Ok(());
            }
        };
        let _ = tty.write_all(
            b"\x1b[0m\
              \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l\
              \x1b[?2004l\
              \x1b]0;\x1b\\\
              \x1b[?1049l",
        );
        let _ = tty.flush();

        #[cfg(feature = "timing-diagnostics")]
        let t_drain_start = std::time::Instant::now();
        drained_stdin = crate::ui::terminal::drain_stdin_nonblocking();
        #[cfg(feature = "timing-diagnostics")]
        eprintln!(
            "[timing-diag] drain_stdin_nonblocking: {:?} bytes={}",
            t_drain_start.elapsed(),
            drained_stdin.len()
        );
        let _ = tty.write_all(b"\x1b[?25h");
        let _ = tty.flush();
    }

    let mut cmd = std::process::Command::new("ssh");
    cmd.args([
        "-t",
        "-o",
        "StrictHostKeyChecking=yes",
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
    .arg("-o")
    .arg(format!("UserKnownHostsFile={}", known_hosts_path.display()))
    .arg("-p")
    .arg(port.to_string())
    .arg("sandbox@127.0.0.1")
    .arg("cd /workspace && exec $SHELL -l");
    cmd.env(
        "TERM",
        std::env::var("TERM").as_deref().unwrap_or("xterm-256color"),
    );

    let status = match crate::ui::pty_relay::PtyRelay::spawn(&mut cmd) {
        Ok(mut relay) => {
            if !drained_stdin.is_empty() {
                let _ = relay.write_to_primary(&drained_stdin);
            }
            #[cfg(feature = "timing-diagnostics")]
            {
                let t_relay_start = std::time::Instant::now();
                eprintln!(
                    "[timing-diag] relay_start: {:?} after_t0",
                    t_relay_start.duration_since(t0)
                );
            }
            match tokio::task::spawn_blocking(move || relay.relay()).await {
                Ok(Ok(s)) => Ok(s),
                Ok(Err(e)) => {
                    ctx.renderer
                        .write_line(&format!("PTY relay error: {e}"), c_error())?;
                    Err(())
                }
                Err(join_err) => {
                    ctx.renderer
                        .write_line(&format!("PTY relay panic: {join_err}"), c_error())?;
                    Err(())
                }
            }
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to spawn PTY: {e}"), c_error())?;
            Err(())
        }
    };

    {
        let _tty = crate::ui::terminal::open_tty_for_write();
        if let Some(mut tty) = _tty {
            let _ = tty.write_all(b"\x1b[?1049h\x1b[2J\x1b[?25l");
            let _ = tty.write_all(b"\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
            let _ = tty.flush();
        }
    }

    ctx.renderer.reset_tui();
    ctx.renderer.set_needs_repaint();
    if let Some(mut tty) = crate::ui::terminal::open_tty_for_write() {
        crate::ui::terminal::sync_and_drain_via_sentinel(
            &mut tty,
            std::time::Duration::from_millis(100),
        );
    }

    crate::ui::terminal::EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
    crate::ui::terminal::EVENT_READER_EXITED.store(false, Ordering::Relaxed);
    crate::ui::input_reader::spawn_input_reader(ctx.user_tx.clone());

    match status {
        Ok(s) if s.success() => {
            ctx.renderer.write_line("SSH session ended.", c_agent())?;
        }
        Ok(s) => {
            let code = s.code().unwrap_or(-1);
            ctx.renderer
                .write_line(&format!("ssh exited with code {code}"), c_error())?;
        }
        Err(()) => {}
    }
    Ok(())
}
