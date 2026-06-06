//! Sandbox slash commands: /sandbox attach, /sandbox snapshot.
//!
//! `/sandbox ssh` is a hidden alias for `/sandbox attach`.

use super::{SlashCtx, c_agent, c_error, c_result};

/// /sandbox — manage the microVM sandbox.
pub(super) async fn cmd_sandbox(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("help");
    match sub {
        "attach" | "ssh" => cmd_sandbox_attach(ctx).await?,
        "snapshot" => cmd_sandbox_snapshot(ctx, parts).await?,
        "reboot" | "start" => cmd_sandbox_reboot(ctx).await?,
        "help" | "--help" | "-h" => {
            ctx.renderer.write_line("sandbox commands:", c_agent())?;
            ctx.renderer.write_line(
                "  /sandbox attach        —   shell into the microVM",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox reboot/start —   boot/restart the microVM",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot save <name>   —   save VM state",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot list         —   list saved snapshots",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot restore <name> —   restore (VM must be stopped)",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot delete <name> —   delete a snapshot",
                c_result(),
            )?;
        }
        _ => {
            ctx.renderer.write_line(
                &format!("unknown sandbox sub-command: {sub} (try /sandbox help)"),
                c_error(),
            )?;
        }
    }
    Ok(())
}

/// /sandbox attach — drop into an interactive shell inside the microVM.
async fn cmd_sandbox_attach(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
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
    let (port, key_path) = info;

    ctx.renderer
        .write_line(&format!("connecting to VM on port {port}..."), c_agent())?;

    // Pre-flight: try a quick SSH connection to verify the key works.
    let preflight = std::process::Command::new("ssh")
        .args([
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

    // ── interactive session via PTY relay ────────────────────
    //
    // SSH runs on a PTY secondary; a relay thread copies bytes
    // between the PTY primary and /dev/tty. This keeps SSH off
    // the real terminal entirely — no races with crossterm's
    // raw-mode event reader, no stale keystrokes in stdin after
    // exit.

    use std::io::Write;
    use std::sync::atomic::Ordering;

    // ── timing probes (timing-diagnostics feature) ──────────────
    #[cfg(feature = "timing-diagnostics")]
    let t0 = std::time::Instant::now();

    // 1. Stop the input reader so it doesn't race for stdin bytes.
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

    // 2. Suspend TUI: leave alt screen, disable mouse/paste,
    //    show cursor. Keep raw mode — PTY relay reads individual
    //    keystrokes so readline/emacs bindings work in the sandbox.
    let drained_stdin;
    {
        let mut tty = match crate::ui::terminal::open_tty_for_write() {
            Some(f) => f,
            None => {
                // Restart the reader before returning.
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

    // 3. Build SSH command and spawn on PTY.
    // Attach to the sandbox interactively — start in the workspace
    // directory so the user lands where their project files are.
    let mut cmd = std::process::Command::new("ssh");
    cmd.args([
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
    .arg(port.to_string())
    .arg("sandbox@127.0.0.1")
    .arg("cd /workspace && exec $SHELL -l");
    cmd.env(
        "TERM",
        std::env::var("TERM").as_deref().unwrap_or("xterm-256color"),
    );

    let status = match crate::ui::pty_relay::PtyRelay::spawn(&mut cmd) {
        Ok(mut relay) => {
            // Inject keystrokes drained during the TUI suspend window
            // so they reach the guest shell instead of being lost.
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
            // Run the relay on a blocking thread so it doesn't
            // tie up a tokio worker. Relay has its own
            // setpriority — it's self-contained.
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

    // 4. Restore TUI: re-enter alt screen, clear it, enable mouse/paste,
    //    hide cursor, drain re-entry chatter. Raw mode was never disabled,
    //    so skip enable_raw_mode() (which writes to fd 1, now the log file).
    {
        let _tty = crate::ui::terminal::open_tty_for_write();
        if let Some(mut tty) = _tty {
            let _ = tty.write_all(b"\x1b[?1049h\x1b[2J\x1b[?25l");
            let _ = tty.write_all(b"\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
            let _ = tty.flush();
        }
    }

    // Rebuild ratatui's Terminal with a fresh diff buffer so the next
    // paint is a full redraw (not a diff against the stale pre-attach
    // buffer), then flag it.
    ctx.renderer.reset_tui();
    ctx.renderer.set_needs_repaint();

    // ── Drain re-entry chatter BEFORE spawning the input reader ──
    // The DSR-OS sentinel reads directly from fd 0 (stdin). If the
    // crossterm input reader is already running, both read from the
    // same fd simultaneously — the sentinel can consume real keystrokes
    // (discarding them), and the reader can consume the DSR-OS reply
    // (causing the sentinel to time out). Running the sentinel first
    // eliminates the race: after it drains all re-entry chatter,
    // the reader starts on a clean fd 0 with no contention.
    if let Some(mut tty) = crate::ui::terminal::open_tty_for_write() {
        crate::ui::terminal::sync_and_drain_via_sentinel(
            &mut tty,
            std::time::Duration::from_millis(100),
        );
    }

    // Spawn the input reader AFTER the sentinel so keystrokes typed
    // after the drain window are captured cleanly — no race with
    // the sentinel's libc::read(0, …) loop.
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
        Err(()) => {
            // PTY error already reported inline.
        }
    }
    Ok(())
}

/// /sandbox reboot — stop and restart the microVM.
async fn cmd_sandbox_reboot(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if !ctx.sandbox.is_microvm() {
        ctx.renderer.write_line(
            "microVM sandbox not active — start dirge with --sandbox microvm.",
            c_error(),
        )?;
        return Ok(());
    }
    ctx.renderer.write_line("rebooting microVM...", c_agent())?;
    match ctx.sandbox.reboot_microvm().await {
        Ok(()) => {
            ctx.renderer
                .write_line("microVM rebooted — fresh VM is ready.", c_result())?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("reboot failed: {e}"), c_error())?;
        }
    }
    Ok(())
}

// ── snapshot subcommands ─────────────────────────────────────────

async fn cmd_sandbox_snapshot(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let action = parts.get(2).copied().unwrap_or("help");
    match action {
        "save" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot save <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.save_snapshot(name) {
                Ok(()) => {
                    ctx.renderer
                        .write_line(&format!("snapshot '{name}' saved."), c_result())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot save failed: {e}"), c_error())?;
                }
            }
        }
        "list" => match ctx.sandbox.list_snapshots() {
            Ok(names) if names.is_empty() => {
                ctx.renderer
                    .write_line("no snapshots saved yet.", c_agent())?;
            }
            Ok(names) => {
                ctx.renderer.write_line("snapshots:", c_agent())?;
                for name in &names {
                    ctx.renderer.write_line(&format!("  {name}"), c_result())?;
                }
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("list snapshots failed: {e}"), c_error())?;
            }
        },
        "restore" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot restore <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.restore_snapshot(name) {
                Ok(()) => {
                    ctx.renderer.write_line(
                        &format!("snapshot '{name}' restored — restart the VM to use it."),
                        c_result(),
                    )?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot restore failed: {e}"), c_error())?;
                }
            }
        }
        "delete" => {
            let name = parts.get(3).copied().unwrap_or("");
            if name.is_empty() {
                ctx.renderer
                    .write_line("usage: /sandbox snapshot delete <name>", c_error())?;
                return Ok(());
            }
            match ctx.sandbox.delete_snapshot(name) {
                Ok(()) => {
                    ctx.renderer
                        .write_line(&format!("snapshot '{name}' deleted."), c_result())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("snapshot delete failed: {e}"), c_error())?;
                }
            }
        }
        "help" | "--help" | "-h" => {
            ctx.renderer.write_line("snapshot commands:", c_agent())?;
            ctx.renderer.write_line(
                "  /sandbox snapshot save <name>      —   save current VM state",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot list             —   list saved snapshots",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot restore <name>    —   restore (stop VM first)",
                c_result(),
            )?;
            ctx.renderer.write_line(
                "  /sandbox snapshot delete <name>     —   delete a snapshot",
                c_result(),
            )?;
        }
        _ => {
            ctx.renderer.write_line(
                &format!("unknown snapshot command: {action} (try /sandbox snapshot help)"),
                c_error(),
            )?;
        }
    }
    Ok(())
}
