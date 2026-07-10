//! Shared spawn hardening for stdio child processes: session isolation
//! (`setsid`) plus a process-group SIGKILL on drop (dirge-wupp).
//!
//! DAP already open-coded both ([`crate::dap::client`]) and the bash tool
//! has a disarmable variant ([`crate::agent::tools::bash`]); LSP and MCP
//! had only `kill_on_drop(true)`, which signals just the *direct* child.
//! So a language server's own child — `typescript-language-server` →
//! `tsserver`, `jdtls` → `java`, `pyright` → a node worker — was orphaned
//! on session end, and repeated sessions accumulated orphans. Those
//! servers also kept dirge's controlling terminal (no `setsid`), the
//! TUI-corruption hazard the DAP path documents.
//!
//! This module is the single home DAP/LSP/MCP share. (The bash tool keeps
//! its own copy: its guard is *disarmed* on graceful completion / timeout,
//! because its timeout path issues the group kill itself — a coupling the
//! other three don't have.)

/// Put a spawned child in its own session: `setsid` gives it a new session
/// with NO controlling terminal and a new process group whose pgid == pid.
///
/// Without a controlling terminal the child can't open `/dev/tty` (git/ssh
/// credential prompts fail fast with ENXIO instead of blocking on dirge's
/// tty) or call `tcsetpgrp()` to steal the foreground and corrupt the TUI.
/// The pgid == pid invariant lets [`ProcessGroupGuard`] reach the whole
/// subtree with `kill(-pgid)`. No-op off Unix (Windows relies on
/// `kill_on_drop`).
#[cfg(unix)]
pub fn detach_session(cmd: &mut tokio::process::Command) {
    // SAFETY: pre_exec runs in the forked child before exec. setsid() is
    // async-signal-safe; immediately after fork the child is not a
    // process-group leader, so setsid() succeeds and creates a new session
    // (no controlling terminal). Returning the error aborts the spawn
    // rather than running the command attached to dirge's tty.
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

#[cfg(not(unix))]
pub fn detach_session(_cmd: &mut tokio::process::Command) {}

/// SIGKILL a child's entire process group on drop — not just the direct
/// child (which `kill_on_drop(true)` already reaps). Requires the child was
/// spawned with [`detach_session`] so its pgid == pid. Fires once, on drop.
///
/// Hold this for the lifetime of the process handle, declared so it drops
/// while the leader child is still un-reaped: the live zombie pins the
/// PID/PGID and stops the kernel recycling it for an unrelated group
/// between drops. Off Unix it is an inert placeholder so call sites stay
/// uniform.
// Off Unix the struct is an empty, never-constructed placeholder
// (`from_pid` always returns `None`); silence the resulting dead-code lint
// so the windows-default build stays warning-clean.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: u32,
}

impl ProcessGroupGuard {
    /// Build a guard from a freshly-spawned child's pid (its pgid after
    /// [`detach_session`]). Returns `None` when the pid is unavailable
    /// (child already reaped) or is a reserved group: `0` is dirge's OWN
    /// process group (`kill(-0, …)` would be suicide) and `1` targets every
    /// process we may signal.
    pub fn from_pid(pid: Option<u32>) -> Option<Self> {
        #[cfg(unix)]
        {
            pid.filter(|&p| p > 1).map(|pgid| Self { pgid })
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            None
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // SAFETY: kill() with a negative pid targets the process group.
        // `from_pid` already refuses pgid <= 1; re-check defensively so a
        // future caller can't turn this into `kill(-0)` (our own group) or
        // `kill(-1)` (everything). SIGKILL is identical on every POSIX
        // platform; libc::pid_t is i32 on every platform dirge supports.
        if self.pgid <= 1 {
            return;
        }
        unsafe {
            let _ = libc::kill(-(self.pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn from_pid_rejects_reserved_and_missing_groups() {
        assert!(ProcessGroupGuard::from_pid(None).is_none());
        assert!(
            ProcessGroupGuard::from_pid(Some(0)).is_none(),
            "group 0 is our own"
        );
        assert!(
            ProcessGroupGuard::from_pid(Some(1)).is_none(),
            "group 1 is everything"
        );
        assert!(ProcessGroupGuard::from_pid(Some(4242)).is_some());
    }

    /// The core orphan-reaping mechanism: a child spawned with
    /// `detach_session` (its own group) is killed by the guard's group
    /// SIGKILL on drop — with `kill_on_drop(false)` and no direct kill, so
    /// only the `kill(-pgid)` can have done it.
    #[tokio::test]
    async fn guard_group_kill_reaps_the_child_on_drop() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30").kill_on_drop(false);
        detach_session(&mut cmd);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child pid");
        // pgid == pid after setsid; the group is alive.
        assert_eq!(
            unsafe { libc::kill(-(pid as libc::pid_t), 0) },
            0,
            "process group should be alive before the guard drops"
        );

        let guard = ProcessGroupGuard::from_pid(Some(pid)).expect("guard");
        drop(guard); // sends SIGKILL to -pgid

        // The child must have died from SIGKILL (nothing else killed it).
        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().await.expect("wait");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "child must be terminated by the group SIGKILL"
        );
    }
}
