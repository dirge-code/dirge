//! Shared off-loop "phase" machinery (dirge-qhfk).
//!
//! Several UI features run slow or interactive work OFF the single
//! `current_thread` runtime so the `run_interactive` `select!` loop stays
//! responsive (renders, accepts input, services plugin dialogs, honors
//! Ctrl+C): compaction ([`crate::ui::compaction`]), `/plan`
//! ([`crate::agent::plan::runtime`]), and — added here — plugin lifecycle
//! hooks. Each spawns a task that streams events back over an `mpsc`
//! channel, and a dedicated `select!` arm drains the channel and runs the
//! continuation on-loop.
//!
//! The common plumbing is the channel + the spawned task + abort-on-drop.
//! [`PhaseHandle`] factors that out; each phase keeps its own event enum and
//! any install-time payload BESIDE the handle (see the note on `Drop` below).
//!
//! ## Why the task body must use `spawn_blocking` for plugin dispatch
//!
//! The app runs under `#[tokio::main(flavor = "current_thread")]`, so there
//! is exactly one runtime thread — the loop thread. `Worker::eval` blocks on
//! a std channel; calling it from a plain `async` block still runs on that
//! one thread and re-freezes the loop (the very deadlock this module fixes).
//! A phase task that dispatches plugin hooks must wrap the blocking call in
//! `tokio::task::spawn_blocking` so it runs on the blocking pool while the
//! loop keeps draining `dialog_rx` — the same discipline the tool-hook path
//! already uses (`crate::agent::agent_loop::plugin_hooks`).

/// Off-loop phase task plus its event channel.
///
/// Abort-on-drop: dropping the handle cancels the task, so callers never
/// hand-write `task.abort()` on Ctrl+C/Esc — `phase.take()` (which drops the
/// handle) is enough. `abort()` is idempotent and a no-op once the task has
/// finished, so dropping after the terminal event has been drained is safe.
///
/// Phases that carry install-time payload (e.g. a continuation, a captured
/// cut index) must store it in a WRAPPER struct alongside a `PhaseHandle`
/// field, NOT by adding fields here and NOT by implementing `Drop` on the
/// wrapper: completion arms destructure the wrapper by move to pull the
/// payload out, and you cannot move fields out of a type that implements
/// `Drop`. Keeping `Drop` only on the nested `PhaseHandle` preserves those
/// partial moves.
// dirge-qhfk: call sites land in the following stages (mid-run hooks,
// on-prompt phase, done chain). Allow dead_code until then so the Stage 0
// commit builds clean under `-D warnings`; removed once wired.
#[allow(dead_code)]
pub(crate) struct PhaseHandle<E> {
    pub rx: tokio::sync::mpsc::Receiver<E>,
    pub task: tokio::task::JoinHandle<()>,
}

impl<E> Drop for PhaseHandle<E> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<E: Send + 'static> PhaseHandle<E> {
    /// Spawn `body` — which owns the event `Sender` — as the phase task.
    /// `capacity` sizes the channel: 1 for a single terminal event
    /// (compaction, done-chain), larger for progress-streaming phases (plan).
    #[allow(dead_code)] // wired up in the following stages (dirge-qhfk)
    pub(crate) fn spawn<F, Fut>(capacity: usize, body: F) -> Self
    where
        F: FnOnce(tokio::sync::mpsc::Sender<E>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::channel::<E>(capacity.max(1));
        let task = tokio::spawn(body(tx));
        PhaseHandle { rx, task }
    }
}

#[cfg(test)]
mod tests {
    use super::PhaseHandle;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn drop_aborts_the_task() {
        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let flag = ran_to_completion.clone();
        let handle = PhaseHandle::<()>::spawn(1, move |_tx| async move {
            // Long sleep: only a completed (non-aborted) task sets the flag.
            tokio::time::sleep(Duration::from_secs(30)).await;
            flag.store(true, Ordering::SeqCst);
        });
        // Grab an abort handle BEFORE dropping so we can observe the task's
        // terminal state afterwards.
        let abort = handle.task.abort_handle();
        drop(handle);
        // Give the runtime a moment to process the cancellation.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            abort.is_finished(),
            "dropping the handle must abort the task"
        );
        assert!(
            !ran_to_completion.load(Ordering::SeqCst),
            "aborted task must not have run to completion"
        );
    }

    #[tokio::test]
    async fn terminal_event_is_delivered() {
        let mut handle = PhaseHandle::<u32>::spawn(1, |tx| async move {
            let _ = tx.send(42).await;
        });
        assert_eq!(handle.rx.recv().await, Some(42));
    }
}
