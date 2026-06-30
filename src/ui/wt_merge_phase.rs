//! Non-blocking `/wt-merge` (dirge-iagk).
//!
//! Merging a worktree runs several git subprocesses (`merge_worktree`), a
//! synchronous call that blocked the event loop — and the runtime thread — for
//! its whole duration (seconds on a large repo). This module runs the merge on
//! a blocking thread; the `wt_merge_phase` arm performs the post-merge
//! continuation (return to the main repo, remove the worktree, rebuild the
//! agent) once it lands.
//!
//! The handle type is unconditional so the `tokio::select!` arm can be too
//! (select! doesn't accept `#[cfg]` on its arms); only [`spawn`] — which calls
//! the `git-worktree`-gated git helpers — is feature-gated. In a non-worktree
//! build the field stays `None` (spawn is never reachable) and the arm idles.

/// Handle to the spawned merge: the result channel the loop drains, the task
/// (so Ctrl+C can `abort()` it), and the parsed merge parameters the arm needs
/// for the post-merge continuation.
pub(crate) struct WtMergePhaseHandle {
    pub rx: tokio::sync::mpsc::Receiver<Result<(), String>>,
    pub task: tokio::task::JoinHandle<()>,
    pub branch: String,
    pub target: String,
    pub main_path: String,
    pub wt_path: String,
}

/// Spawn the worktree merge off-thread. `merge_worktree` is synchronous (git
/// subprocesses), so it runs on a blocking thread; the result (or a stringified
/// panic) is sent back over a capacity-1 channel.
#[cfg(feature = "git-worktree")]
pub(crate) fn spawn(
    branch: String,
    target: String,
    main_path: String,
    wt_path: String,
) -> WtMergePhaseHandle {
    let info = crate::extras::git_worktree::WorktreeInfo {
        branch: branch.clone(),
        worktree_path: std::path::PathBuf::from(&wt_path),
        main_repo_path: std::path::PathBuf::from(&main_path),
    };
    let target_for_merge = target.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<(), String>>(1);
    let task = tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            crate::extras::git_worktree::merge_worktree(&info, &target_for_merge)
        })
        .await
        .unwrap_or_else(|e| Err(format!("merge task panicked: {e}")));
        let _ = tx.send(result).await;
    });
    WtMergePhaseHandle {
        rx,
        task,
        branch,
        target,
        main_path,
        wt_path,
    }
}
