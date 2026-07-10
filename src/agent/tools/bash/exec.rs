//! Process-execution layer for the bash tool. Split out of
//! `agent/tools/bash.rs` (dirge-4y4l stage 9a): spawns commands into their
//! own process group, drains stdout+stderr in arrival order, and enforces
//! timeouts / process-group cleanup. Used by `BashTool::call` (synchronous,
//! bounded runs via [`run_with_timeout`]) and the background-shell path
//! (detached, streamed runs via [`spawn_streaming_shell`]).

use tokio::process::Command;
use tokio::time::Duration;

use crate::agent::tools::ToolError;

/// Captured output with stdout + stderr lines preserved in arrival
/// order. Replaces tokio's `Output` (which collects each stream as
/// a separate blob, losing time ordering between them — F12).
#[derive(Debug)]
pub(crate) struct InterleavedOutput {
    /// Lines in the order they arrived from EITHER pipe.
    pub merged: String,
    pub exit_code: i32,
}

/// Per-line read ceiling for the drain loops. A newline-free line longer
/// than this comes back in `MAX_LINE_BYTES` chunks so a `base64 -w0
/// huge.bin` / `tr -d '\n' < huge.log` line can't buffer unbounded and
/// OOM (dirge-yw3o). Mirrors the per-line cap in read.rs.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Read up to and including the next `\n` from `reader` into `buf`, but
/// never buffer more than `max` bytes per call — a line longer than
/// `max` is returned in `max`-sized chunks (dirge-yw3o).
///
/// Reads RAW bytes rather than `read_line`, so non-UTF-8 output no longer
/// fails with `InvalidData` — which the drain loops treated as EOF,
/// silently dropping everything after the first invalid byte (dirge-do5l).
/// Callers lossy-decode the returned bytes. Returns `Ok(0)` only at true
/// EOF (an unterminated final line is returned once, then `Ok(0)`).
pub(super) async fn read_chunk_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    buf.clear();
    loop {
        // Check the cap BEFORE awaiting more input: once a capped chunk
        // is ready we must return it, not block waiting for a newline
        // that may never come.
        if buf.len() >= max {
            return Ok(buf.len());
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF
        }
        let newline_pos = available.iter().position(|&b| b == b'\n');
        let slice = match newline_pos {
            Some(i) => &available[..=i],
            None => available,
        };
        let slice_len = slice.len();
        let take = slice_len.min(max - buf.len());
        buf.extend_from_slice(&slice[..take]);
        // The `available`/`slice` borrow of `reader` ends above; only
        // owned values (`take`, `slice_len`, `newline_pos`) are used past
        // the `consume`.
        reader.consume(take);
        if newline_pos.is_some() && take == slice_len {
            return Ok(buf.len());
        }
        // Otherwise: cap hit mid-slice (loop → cap guard returns) or no
        // newline yet and still under cap (loop pulls more).
    }
}

/// On Unix, SIGKILL the bash process group on drop. Used to clean up
/// grandchildren when the agent task is aborted (Ctrl+C) — tokio's
/// `kill_on_drop` only signals the immediate child, leaving descendants
/// orphaned. Disarmed via [`PgKillGuard::disarm`] on graceful paths
/// (successful completion, timeout — which already calls killpg itself)
/// so we don't double-signal.
#[cfg(unix)]
struct PgKillGuard {
    pid: u32,
    armed: bool,
}

#[cfg(unix)]
impl PgKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PgKillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: killpg with negative pid sends to the process
        // group. SIGKILL is the same on every POSIX platform;
        // libc::pid_t is i32 on every platform dirge supports. The
        // pid was set by us via `process_group(0)` so we know this
        // group exists and is bash + descendants.
        unsafe {
            let _ = libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Put the spawned child in its own session: a new session with no
/// controlling terminal, and (as a side effect of `setsid`) a new
/// process group whose pgid == pid.
///
/// This is the Unix mechanism that makes interactive prompts fail fast.
/// `stdin(Stdio::null())` is not enough: git/ssh and friends read
/// credentials from `/dev/tty` — the *controlling terminal* — not stdin.
/// In Off sandbox mode the bash child would otherwise inherit dirge's
/// controlling terminal (a bare `process_group(0)` makes a new group but
/// keeps the session + terminal), so a `git clone` username prompt blocks
/// on the TUI's tty until the timeout. With no controlling terminal the
/// `/dev/tty` open fails with ENXIO and the prompt errors out instantly.
/// Bwrap mode already gets this via bwrap's `--new-session`; this brings
/// Off mode in line. Mirrors the DAP adapter spawn (`dap/client.rs`).
///
/// pgid == pid is preserved, so the `killpg(-pid, SIGKILL)` paths below
/// (timeout + `PgKillGuard`) still reach the whole subprocess tree.
#[cfg(unix)]
pub(crate) fn detach_session(cmd: &mut Command) {
    // SAFETY: pre_exec runs in the forked child before exec. setsid()
    // is async-signal-safe; immediately after fork the child is not a
    // process-group leader, so setsid() succeeds and creates a new
    // session (no controlling terminal). Returning the error aborts the
    // spawn rather than running the command attached to dirge's tty.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Spawn `cmd` into its own process group and wait for it,
/// capped at `secs`. On timeout, send SIGKILL to the process
/// group so the whole subprocess tree dies — not just bash. On
/// Windows we fall back to tokio's `kill_on_drop` which signals
/// the direct child only (Windows job objects would be cleaner
/// but require extra deps). F6 + F12 fix.
pub(crate) async fn run_with_timeout(
    cmd: Command,
    secs: u64,
) -> Result<InterleavedOutput, ToolError> {
    use std::process::Stdio;
    let mut cmd = cmd;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop(true)` ensures the immediate child gets a
    // signal when the tokio future is dropped — necessary for
    // ANY platform's timeout to actually clean up the bash process.
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        // New session (not just a new process group): detaches the
        // controlling terminal so interactive prompts fail fast, and
        // still gives pgid == pid so `killpg(-pid)` reaches every
        // descendant. See `detach_session`.
        detach_session(&mut cmd);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Msg(format!("failed to spawn: {}", e)))?;
    let pid = child.id();
    // pid drives only the Unix process-group kill paths below; on
    // non-unix it's unused — consume it so `-D warnings` passes.
    #[cfg(not(unix))]
    let _ = pid;

    // Drop guard: on Unix, `kill_on_drop(true)` SIGKILLs the immediate
    // bash child when the future is dropped (e.g. user Ctrl+C aborts
    // the agent task) but leaves bash's *descendants* running as
    // grandchildren of pid 1. The timeout branch below already
    // handles this by calling `killpg(-pid, SIGKILL)`; the same is
    // needed for any other drop path. Holding a `PgKillGuard` for
    // the lifetime of the future does that.
    #[cfg(unix)]
    let _pgguard = pid.map(PgKillGuard::new);

    // F12: drain stdout + stderr concurrently into a single buffer
    // so the order of lines reflects actual arrival time. The prior
    // implementation (`wait_with_output`) buffered each stream
    // separately and concatenated stdout + stderr at the end, which
    // mis-ordered every command that wrote to both interleaved
    // (e.g. `make`, `npm install`, `cargo build`).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = async move {
        let mut merged = String::new();
        let mut so = stdout.map(tokio::io::BufReader::new);
        let mut se = stderr.map(tokio::io::BufReader::new);
        // TOOL-7: streaming cap. Previously this loop appended
        // every line to `merged` without bound; a child writing
        // GBs (`cat /dev/urandom | head -c 10G`) would buffer all
        // of it in memory before the post-drain truncation. Stop
        // appending once we cross the cap; keep draining so the
        // child doesn't block on a full pipe buffer (which would
        // hold open the pgid past the timeout).
        const DRAIN_CAP_BYTES: usize = 256 * 1024;
        let mut overflow_bytes: usize = 0;
        loop {
            // Decide presence BEFORE constructing futures — the
            // `if` guards on select! borrow `so` and `se`, which
            // would conflict with the futures' mutable borrows.
            let has_so = so.is_some();
            let has_se = se.is_some();
            if !has_so && !has_se {
                break;
            }
            let mut so_buf: Vec<u8> = Vec::new();
            let mut se_buf: Vec<u8> = Vec::new();
            // Build futures lazily; each is "noop" if its reader
            // is None. We funnel both into `Result<usize>` so the
            // select! arms have matching types. `read_chunk_capped`
            // reads raw bytes (do5l) with a per-line ceiling (yw3o).
            let so_fut = async {
                match so.as_mut() {
                    Some(r) => read_chunk_capped(r, &mut so_buf, MAX_LINE_BYTES)
                        .await
                        .map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            let se_fut = async {
                match se.as_mut() {
                    Some(r) => read_chunk_capped(r, &mut se_buf, MAX_LINE_BYTES)
                        .await
                        .map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            tokio::select! {
                biased;
                r = so_fut, if has_so => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { so = None; }
                    Ok(Some(n)) => {
                        if merged.len() < DRAIN_CAP_BYTES {
                            merged.push_str(&String::from_utf8_lossy(&so_buf));
                        } else {
                            overflow_bytes = overflow_bytes.saturating_add(n);
                        }
                    },
                },
                r = se_fut, if has_se => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                    Ok(Some(n)) => {
                        if merged.len() < DRAIN_CAP_BYTES {
                            merged.push_str(&String::from_utf8_lossy(&se_buf));
                        } else {
                            overflow_bytes = overflow_bytes.saturating_add(n);
                        }
                    },
                },
            }
        }
        if overflow_bytes > 0 {
            if !merged.is_empty() && !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(&format!(
                "…[bash output exceeded cap; discarded {} additional bytes streamed after the {}-KiB cap]",
                overflow_bytes,
                DRAIN_CAP_BYTES / 1024,
            ));
        }
        merged
    };

    let wait = async {
        let merged = drain.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((merged, status))
    };

    let outcome = tokio::time::timeout(Duration::from_secs(secs), wait).await;
    match outcome {
        Ok(Ok((merged, status))) => {
            // Graceful completion — process group is already gone.
            // Disarm the guard so its Drop doesn't issue a useless
            // SIGKILL against a reaped pgid (worst case: signal
            // races into a PID re-used by the OS).
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
            }
            Ok(InterleavedOutput {
                merged,
                exit_code: status.code().unwrap_or(-1),
            })
        }
        Ok(Err(e)) => Err(ToolError::Msg(format!("wait failed: {}", e))),
        Err(_) => {
            // Timeout path already issues the killpg below; disarm
            // the drop guard so we don't double-signal.
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
                if let Some(pid) = pid {
                    // SAFETY: killpg with negative pid sends to the
                    // process group. SIGKILL is the same on every
                    // POSIX platform; libc::pid_t is i32 on every
                    // platform dirge supports.
                    unsafe {
                        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
            let _ = pid;
            Err(ToolError::Msg(format!("Command timed out after {}s", secs)))
        }
    }
}

/// Spawn `cmd` detached and stream its output into the background-shell
/// store as it arrives. Returns the drain-task `JoinHandle` so the store
/// can abort it (which drops the child + its `PgKillGuard`, SIGKILLing the
/// whole process group). `timeout` is optional: `None` runs unbounded
/// (the Claude-Code model — for dev servers / watchers); `Some(secs)`
/// auto-kills after that long. The task records a terminal `ShellStatus`
/// on the store when it ends.
///
/// Process-group cleanup is Unix-only (same as `run_with_timeout`): on
/// Windows there's no `process_group`/`PgKillGuard`, so aborting the task
/// signals only the immediate child via `kill_on_drop` — a backgrounded
/// process's grandchildren can be orphaned. Background shells are an
/// inherently Unix-oriented feature; Windows is best-effort.
pub(super) fn spawn_streaming_shell(
    cmd: Command,
    store: crate::agent::tools::bg_shell::BackgroundShellStore,
    id: String,
    timeout: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    use crate::agent::tools::bg_shell::ShellStatus;
    use std::process::Stdio;

    /// dirge-e8sb: terminal-status backstop. The happy path records
    /// the real exit status at the end of the task; if the task dies
    /// any other way — aborted outside `kill()` (runtime teardown) or
    /// a panic — this guard's Drop records `Failed` so the registry
    /// never reports a dead shell as Running. `finish` is
    /// first-terminal-wins, so the backstop is a no-op whenever a
    /// real status (Exited/Killed/Failed) landed first.
    struct FinishOnDrop {
        store: crate::agent::tools::bg_shell::BackgroundShellStore,
        id: String,
    }
    impl Drop for FinishOnDrop {
        fn drop(&mut self) {
            self.store.finish(
                &self.id,
                ShellStatus::Failed("drain task ended without recording an exit status".into()),
            );
        }
    }

    tokio::spawn(async move {
        let _finish_backstop = FinishOnDrop {
            store: store.clone(),
            id: id.clone(),
        };
        let mut cmd = cmd;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        detach_session(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                store.finish(&id, ShellStatus::Failed(format!("failed to spawn: {e}")));
                return;
            }
        };
        let pid = child.id();
        // Unix-only process-group kill paths use pid; consume on non-unix.
        #[cfg(not(unix))]
        let _ = pid;
        #[cfg(unix)]
        let mut pgguard = pid.map(PgKillGuard::new);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Stream stdout + stderr line-by-line into the store in arrival
        // order. The closure borrows `store`/`id`, so keep it scoped.
        let drain = {
            let store = store.clone();
            let id = id.clone();
            async move {
                let mut so = stdout.map(tokio::io::BufReader::new);
                let mut se = stderr.map(tokio::io::BufReader::new);
                loop {
                    let has_so = so.is_some();
                    let has_se = se.is_some();
                    if !has_so && !has_se {
                        break;
                    }
                    let mut so_buf: Vec<u8> = Vec::new();
                    let mut se_buf: Vec<u8> = Vec::new();
                    // Raw byte reads with a per-line ceiling: non-UTF-8
                    // output survives lossy-decoded (do5l) and a giant
                    // newline-free line can't buffer unbounded (yw3o).
                    let so_fut = async {
                        match so.as_mut() {
                            Some(r) => read_chunk_capped(r, &mut so_buf, MAX_LINE_BYTES)
                                .await
                                .map(Some),
                            None => Ok::<_, std::io::Error>(None),
                        }
                    };
                    let se_fut = async {
                        match se.as_mut() {
                            Some(r) => read_chunk_capped(r, &mut se_buf, MAX_LINE_BYTES)
                                .await
                                .map(Some),
                            None => Ok::<_, std::io::Error>(None),
                        }
                    };
                    tokio::select! {
                        biased;
                        r = so_fut, if has_so => match r {
                            Ok(Some(0)) | Ok(None) | Err(_) => { so = None; }
                            Ok(Some(_)) => store.append(&id, &String::from_utf8_lossy(&so_buf)),
                        },
                        r = se_fut, if has_se => match r {
                            Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                            Ok(Some(_)) => store.append(&id, &String::from_utf8_lossy(&se_buf)),
                        },
                    }
                }
            }
        };

        let wait = async {
            drain.await;
            child.wait().await
        };

        let status = match timeout {
            Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), wait).await {
                Ok(Ok(st)) => ShellStatus::Exited(st.code().unwrap_or(-1)),
                Ok(Err(e)) => ShellStatus::Failed(e.to_string()),
                Err(_) => {
                    // Timed out: SIGKILL the whole group, then mark killed.
                    #[cfg(unix)]
                    if let Some(pid) = pid {
                        unsafe {
                            let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                        }
                    }
                    let _ = pid;
                    ShellStatus::Failed(format!("auto-killed after {secs}s timeout"))
                }
            },
            None => match wait.await {
                Ok(st) => ShellStatus::Exited(st.code().unwrap_or(-1)),
                Err(e) => ShellStatus::Failed(e.to_string()),
            },
        };
        // We only reach here when the task was NOT aborted: the process
        // has already exited (natural) or been SIGKILLed (timeout). Disarm
        // the guard so its Drop doesn't re-signal a now-reaped (possibly
        // OS-recycled) process-group id — matching `run_with_timeout`. On
        // abort (kill_shell / kill_all) we never get here, so the guard
        // fires on drop and kills the group — which is how kill works.
        #[cfg(unix)]
        if let Some(g) = pgguard.as_mut() {
            g.disarm();
        }
        store.finish(&id, status);
    })
}
