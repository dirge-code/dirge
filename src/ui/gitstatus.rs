//! Background git-status poller for the left panel's `[GIT]` section.
//!
//! Mirrors `sysload.rs`: a detached tokio task refreshes a shared
//! `Option<GitSnapshot>` on a cadence so the UI render path never has to
//! shell out to `git` (which can be slow on a big repo) on the hot path.
//! Reads the *process* current dir each poll, so it follows `/cd`
//! (which calls `std::env::set_current_dir`). When the cwd isn't a git
//! repo — or `git` isn't installed — the snapshot is `None` and the panel
//! section is hidden.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ui::panel_data::GitSnapshot;

/// Shared, lock-protected git snapshot. Cheap to clone (an Arc bump).
#[derive(Clone)]
pub struct SharedGit(Arc<Mutex<Option<GitSnapshot>>>);

impl SharedGit {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Current snapshot, or `None` until the first successful poll / when
    /// the cwd isn't a repo.
    pub fn snapshot(&self) -> Option<GitSnapshot> {
        self.0.lock_ignore_poison().clone()
    }

    fn store(&self, snap: Option<GitSnapshot>) {
        *self.0.lock_ignore_poison() = snap;
    }
}

/// Spawn a background poller on the current tokio runtime. The task ends
/// when the runtime shuts down. Cadence is floored at 1s to avoid
/// hammering `git` on a large worktree.
pub fn spawn_poller(interval: Duration) -> SharedGit {
    let shared = SharedGit::new();
    let out = shared.clone();
    let cadence = interval.max(Duration::from_secs(1));
    tokio::spawn(async move {
        loop {
            shared.store(poll_once().await);
            tokio::time::sleep(cadence).await;
        }
    });
    out
}

/// Run `git` once against the process cwd and build a snapshot, or `None`
/// if this isn't a git repo / `git` failed.
async fn poll_once() -> Option<GitSnapshot> {
    use tokio::process::Command;
    let status = Command::new("git")
        .args(["status", "--porcelain=v1", "--branch"])
        .output()
        .await
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let porcelain = String::from_utf8_lossy(&status.stdout);
    let (branch, staged, unstaged, untracked) = parse_status(&porcelain);

    // Best-effort last-commit subject; empty on a repo with no commits.
    let last_commit = match Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    };

    Some(GitSnapshot {
        branch,
        staged,
        unstaged,
        untracked,
        last_commit,
    })
}

/// Parse `git status --porcelain=v1 --branch` output into
/// `(branch, staged, unstaged, untracked)`.
///
/// Porcelain v1 status codes are two columns `XY`: `X` = index (staged)
/// state, `Y` = worktree (unstaged) state. `??` marks untracked and `!!`
/// ignored. The leading `## <branch>...<upstream>` header carries the
/// branch name.
fn parse_status(porcelain: &str) -> (String, usize, usize, usize) {
    let mut branch = String::new();
    let (mut staged, mut unstaged, mut untracked) = (0usize, 0usize, 0usize);
    for line in porcelain.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // "main...origin/main [ahead 1]" → "main"
            branch = rest
                .split("...")
                .next()
                .unwrap_or(rest)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            continue;
        }
        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        if x == '?' && y == '?' {
            untracked += 1;
            continue;
        }
        if x == '!' && y == '!' {
            continue; // ignored
        }
        if x != ' ' {
            staged += 1;
        }
        if y != ' ' {
            unstaged += 1;
        }
    }
    (branch, staged, unstaged, untracked)
}

#[cfg(test)]
mod tests {
    use super::parse_status;

    #[test]
    fn parses_branch_and_counts() {
        // Built line-by-line: a `\`-continuation would strip the leading
        // space that porcelain's unstaged (Y) column relies on.
        let out = [
            "## main...origin/main [ahead 2]",
            "M  src/a.rs", // staged
            " M src/b.rs", // unstaged
            "MM src/c.rs", // both
            "A  src/d.rs", // staged
            "?? new.txt",  // untracked
            "?? other.txt",
            "!! target/", // ignored
        ]
        .join("\n");
        let (branch, staged, unstaged, untracked) = parse_status(&out);
        assert_eq!(branch, "main");
        // staged: a.rs (M_), c.rs (M of MM), d.rs (A_) = 3
        assert_eq!(staged, 3, "staged");
        // unstaged: b.rs (_M), c.rs (_M of MM) = 2
        assert_eq!(unstaged, 2, "unstaged");
        assert_eq!(untracked, 2, "untracked");
    }

    #[test]
    fn clean_repo_is_all_zero() {
        let (branch, s, u, t) = parse_status("## feature/x\n");
        assert_eq!(branch, "feature/x");
        assert_eq!((s, u, t), (0, 0, 0));
    }

    #[test]
    fn detached_head_has_no_branch_name() {
        // Detached HEAD prints "## HEAD (no branch)".
        let (branch, ..) = parse_status("## HEAD (no branch)\n");
        assert_eq!(branch, "HEAD");
    }
}
