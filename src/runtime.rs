//! Where agent work runs, and why that is not where the UI runs.
//!
//! dirge starts on one `current_thread` runtime (`main.rs`), which means the
//! UI event loop, the agent task, every tool and the terminal input reader
//! share a single thread. Anything that does not `await` therefore freezes
//! all of them: no paint, no keystroke — Ctrl+C included — and no timer, so
//! not even the dispatch watchdog can fire. That is the mechanism behind the
//! hang audit's §2 (`dirge-pge7`), and the tool tree has 288 direct
//! `std::fs::` calls plus tree-sitter parsing and injection scanning that
//! never yield at all.
//!
//! The fix is to stop sharing the thread. Agent work gets its own runtime;
//! the UI keeps the main thread to itself. This module owns that boundary so
//! there is exactly one answer to "which runtime does this run on", and so
//! the guarantee can be tested rather than assumed.
//!
//! ## Why a separate runtime rather than `flavor = "multi_thread"`
//!
//! Flipping the flavor is one line and makes every `await` in the loop an
//! interleaving point against process-global state that the single thread has
//! been serialising all along — the `TODO_LIST` mirror, the snapshot store,
//! the modified-files map, the rate-limit gate, the verifier, the plugin
//! manager. None of that has been audited for concurrent access because until
//! now it could not happen.
//!
//! The process CWD deserves its own mention because it is the one piece of
//! shared state that cannot be given a lock — it is a process singleton, and
//! roughly twenty agent-side sites read it. What makes it safe is that no
//! command which changes it can run while an agent is running: `/cd` and
//! `/worktree` are both kept out of `is_safe_during_agent`. That invariant is
//! load-bearing for this whole design and is pinned by
//! `no_cwd_mutating_command_is_safe_during_an_agent_run`.
//!
//! (An earlier draft of this note also listed `env::set_var` as a hazard.
//! It is not: every call in the tree is inside a `#[cfg(test)] mod`, so
//! nothing mutates the environment at runtime for a concurrent reader to
//! race. Rust 2024 makes the function `unsafe`, so a production site cannot
//! appear by accident either.)
//!
//! Splitting at this boundary instead keeps the agent loop internally
//! single-threaded, so all of that keeps the serialisation it has today and
//! needs no audit. Only state genuinely shared across the UI/agent boundary
//! has to be dealt with, and that boundary is already narrow and already
//! channel-shaped: [`crate::agent::runner::AgentRunner`] hands the UI an
//! event receiver, a `JoinHandle`, an interject sender and a cancel sender,
//! plus `ask_tx` for the permission round trip. Nothing else crosses.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::task::JoinHandle as TokioJoinHandle;

/// The agent runtime's handle, created on first use.
///
/// Holds only the `Handle`; the `Runtime` itself is moved onto the driver
/// thread and never dropped, which is also what keeps `Runtime::drop` — which
/// panics when called from async context — from ever running.
static AGENT: OnceLock<Handle> = OnceLock::new();

/// Name of the thread the agent runtime is driven on, so it is identifiable
/// in a debugger, a profile, or a panic message.
const AGENT_THREAD: &str = "dirge-agent";

/// The runtime that owns agent work — the loop, tool dispatch, and anything
/// they block on.
///
/// Callers should not reach for [`Handle::current`] instead: the whole point
/// is that agent work is *not* on the caller's runtime, and reading the
/// current handle silently reintroduces the coupling this module exists to
/// remove.
pub(crate) fn agent_handle() -> Handle {
    AGENT
        .get_or_init(|| {
            // CURRENT_THREAD, deliberately, and this is the whole safety
            // argument for the split. The agent loop reaches process-global
            // state that the single thread has been serialising all along —
            // the `TODO_LIST` mirror, the snapshot store, the modified-files
            // map, the rate-limit gate, the verifier, the plugin manager.
            // Giving the agent one thread of its own preserves every one of
            // those orderings; giving it a thread POOL would reintroduce
            // exactly the races that made flipping the main runtime's flavor
            // the wrong move. Blocking work in a tool still blocks other
            // agent tasks — background review, subagents, the checkpointer —
            // which is unchanged from today and is the point.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name(AGENT_THREAD)
                .build()
                .expect("the agent runtime is required for dirge to run at all");
            let handle = rt.handle().clone();
            std::thread::Builder::new()
                .name(AGENT_THREAD.to_string())
                .spawn(move || {
                    // A current_thread runtime only advances while something
                    // drives it, so this thread's job is to drive it for the
                    // life of the process. Tasks arrive via `Handle::spawn`
                    // from the UI thread.
                    rt.block_on(std::future::pending::<()>());
                })
                .expect("the agent runtime thread is required for dirge to run at all");
            handle
        })
        .clone()
}

/// Spawn agent work on the agent runtime.
///
/// The returned handle behaves exactly like [`tokio::spawn`]'s, including
/// `abort()` — the UI's Ctrl+C path depends on that.
pub(crate) fn spawn_agent<F>(future: F) -> TokioJoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    agent_handle().spawn(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// How long the stand-in tool blocks its thread for. Comfortably longer
    /// than the UI tick so the two are never confusable.
    const BLOCK_FOR: Duration = Duration::from_millis(500);
    /// One UI tick — a paint, or a keystroke being read.
    const UI_TICK: Duration = Duration::from_millis(20);
    /// The bar. Far enough above `UI_TICK` that scheduler noise on a loaded
    /// machine cannot reach it, and far enough below `BLOCK_FOR` that a UI
    /// that actually waited for the tool cannot sneak under it.
    const STALL_BAR: Duration = Duration::from_millis(250);

    /// THE GUARANTEE: blocking work inside a tool must not stall the UI.
    ///
    /// A tool that does not await — 288 `std::fs::` calls, a tree-sitter
    /// parse, an injection scan — holds its thread. If that is the UI's
    /// thread, the UI stops painting and stops reading input for the
    /// duration, which is the reported hang.
    ///
    /// Written to FAIL before the split: with one `current_thread` runtime
    /// the UI tick cannot be polled until the blocking task lets the thread
    /// go, so the elapsed time is `BLOCK_FOR`, not `UI_TICK`.
    #[tokio::test]
    async fn blocking_agent_work_does_not_stall_the_ui_runtime() {
        let start = Instant::now();
        // Stand-in for a tool doing synchronous work. `std::thread::sleep`
        // rather than `tokio::time::sleep` is the entire point: it holds the
        // thread instead of yielding it.
        let blocked = spawn_agent(async {
            std::thread::sleep(BLOCK_FOR);
        });

        // Stand-in for the UI loop servicing one tick.
        tokio::time::sleep(UI_TICK).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < STALL_BAR,
            "the UI stalled for {elapsed:?} waiting on blocking agent work \
             (bar is {STALL_BAR:?}); agent work is still sharing the UI's thread"
        );

        // Let the blocking task finish so it cannot outlive the test and
        // stall an unrelated one.
        let _ = blocked.await;
    }

    /// The discrimination half, so the test above cannot pass vacuously.
    ///
    /// If `BLOCK_FOR` were mistuned, or `std::thread::sleep` were quietly
    /// swapped for an awaiting sleep, the guarantee test would pass while
    /// measuring nothing. This asserts the stand-in really does hold a
    /// thread for as long as it claims.
    #[tokio::test]
    async fn the_blocking_stand_in_really_blocks() {
        let start = Instant::now();
        std::thread::sleep(BLOCK_FOR);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= BLOCK_FOR,
            "the stand-in returned in {elapsed:?}, so it is not blocking and \
             the guarantee test above is measuring nothing"
        );
        assert!(
            BLOCK_FOR > STALL_BAR,
            "a blocked UI must be distinguishable from a healthy one"
        );
    }
}
