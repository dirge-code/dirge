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

/// Registry of every live detached child process-group id (LSP servers,
/// MCP servers, DAP adapters, bash subtrees). Because those children are
/// spawned with [`detach_session`] (`setsid`), they have no controlling
/// terminal, so terminal-delivered signals never reach them — only an
/// explicit `kill(-pgid)` reaps them, and that only runs from each guard's
/// `Drop`. On a signal-terminated exit (SIGTERM from `kill`, SIGHUP when
/// the terminal/tab closes, SIGINT) `Drop` does NOT run, so the servers are
/// orphaned and keep running (rust-analyzer indexing → 1 GB+). This registry
/// lets the signal reaper ([`crate::signal`]) `kill(-pgid)` them all before
/// the process dies. Entries are added at guard construction and removed on
/// `Drop` (the normal path), so at signal time it holds exactly the groups
/// still alive.
#[cfg(unix)]
static LIVE_GROUPS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Record a live child process group. Reserved groups (`<= 1`) are ignored
/// — the guard constructors already refuse them.
#[cfg(unix)]
pub(crate) fn register_group(pgid: u32) {
    if pgid <= 1 {
        return;
    }
    let mut g = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    if !g.contains(&pgid) {
        g.push(pgid);
    }
}

/// Drop a child process group from the registry — its guard has reaped it
/// (or is about to). Idempotent.
#[cfg(unix)]
pub(crate) fn unregister_group(pgid: u32) {
    let mut g = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = g.iter().position(|&p| p == pgid) {
        g.swap_remove(i);
    }
}

/// SIGKILL every registered child process group. Invoked by the signal
/// reaper on SIGINT/SIGTERM/SIGHUP, where per-guard `Drop` won't run. Safe
/// to call repeatedly and harmless if a group already died (`kill` returns
/// ESRCH, ignored). Does not clear the registry: the process exits
/// immediately afterward.
#[cfg(unix)]
pub(crate) fn reap_all_groups() {
    let pgids: Vec<u32> = {
        let g = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        g.clone()
    };
    reap_groups(&pgids);
}

/// SIGKILL each process group in `pgids`. Split out from `reap_all_groups`
/// so tests can exercise the kill on a single known child WITHOUT reaping
/// the whole shared registry (which would kill other parallel tests' live
/// children). A group that already died is a harmless ESRCH.
#[cfg(unix)]
fn reap_groups(pgids: &[u32]) {
    for &pgid in pgids {
        if pgid > 1 {
            // SAFETY: a negative pid targets the process group. SIGKILL is
            // identical on every POSIX platform; libc::pid_t is i32 on every
            // platform dirge supports.
            unsafe {
                let _ = libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

// No non-unix counterpart: the registry, `reap_all_groups`, and the signal
// reaper are all `cfg(unix)`. Off Unix children aren't `setsid`-detached and
// `kill_on_drop` reaps the direct child, so there's nothing to register.

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
            pid.filter(|&p| p > 1).map(|pgid| {
                // Track the group so the signal reaper can kill it even on
                // an exit path where this guard's Drop never runs.
                register_group(pgid);
                Self { pgid }
            })
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
        unregister_group(self.pgid);
        unsafe {
            let _ = libc::kill(-(self.pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn group_is_registered(pgid: u32) -> bool {
        LIVE_GROUPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&pgid)
    }

    /// A guard registers its group on construction and removes it on drop,
    /// so the reaper's view holds only groups still live. Uses a synthetic
    /// pgid that isn't a real process group — the drop's `kill(-pgid)` is a
    /// harmless ESRCH.
    #[test]
    fn guard_registers_on_construct_and_unregisters_on_drop() {
        // A high, unlikely-to-exist pgid so the drop kill is a no-op and we
        // don't collide with a concurrent test's real child group.
        let fake = 1_000_003;
        assert!(!group_is_registered(fake));
        let guard = ProcessGroupGuard::from_pid(Some(fake)).expect("guard");
        assert!(
            group_is_registered(fake),
            "constructing a guard must register its group for the reaper"
        );
        drop(guard);
        assert!(
            !group_is_registered(fake),
            "dropping a guard (normal path) must unregister its group"
        );
    }

    /// The signal-path reaper: a registered child is killed by the group
    /// reap WITHOUT its guard being dropped — proving that a snapshot of the
    /// registry alone (what the signal handler works from) reaps the subtree.
    /// Reaps only THIS child's group (via `reap_groups`) rather than the
    /// whole shared registry, so it can't kill other parallel tests' live
    /// children (which broke `run_with_timeout_kills_orphaned_child`).
    #[tokio::test]
    async fn reap_groups_kills_a_registered_child() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30").kill_on_drop(false);
        detach_session(&mut cmd);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child pid");

        // Construct (registers) but deliberately keep the guard alive so
        // only the reap — not the guard's Drop — can have killed the child.
        let guard = ProcessGroupGuard::from_pid(Some(pid)).expect("guard");
        assert!(
            group_is_registered(pid),
            "reap_all_groups() would include this pid via its registry snapshot"
        );

        // What `reap_all_groups()` does to each snapshotted pgid, scoped to
        // just ours to avoid cross-test interference.
        reap_groups(&[pid]);

        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().await.expect("wait");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "the group reap must SIGKILL the registered child group"
        );
        drop(guard); // unregister + redundant no-op kill on the dead group
    }

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
