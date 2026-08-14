//! The agent task ended and never said so.
//!
//! `agent_loop::run_end` makes the run's terminal event a property of
//! the task's lifetime, so in practice the UI learns a run is over
//! through the ordinary `AgentEvent::Error` path. This module is the
//! backstop underneath that, for the endings the epitaph cannot narrate
//! from inside the task: a channel too full for its `try_send`, a task
//! aborted by something that is not the UI, a future dropped before the
//! guard was ever constructed.
//!
//! It exists because the UI's run state was tied to the *events* a run
//! emitted rather than to the run. `is_running` is cleared by `Done`,
//! `Error`, `Interjected`, `PlanReview`, `ContextOverflow` and `/quit`;
//! the `Some(event) = rx.recv()` select arm quietly disables itself when
//! the channel closes. Together that means an agent task that ends
//! without emitting one of those leaves the UI at "running" with no arm
//! left to change its mind — a hang, with nothing on screen to explain
//! it and no timeout that can fire.
//!
//! So the UI also watches the task handle. Whatever the reason, the run
//! is over, and the user gets an error and their prompt back.

use tokio::task::JoinError;

/// How a run ended when it did not report its own ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunExit {
    /// The task panicked. Carries the payload text; `tokio` kept the
    /// panic in the `JoinHandle` rather than letting it reach the
    /// process, which is why the run could disappear so quietly.
    Crashed(String),
    /// The task was aborted — by a `/quit` racing the run, a dropped
    /// fork guard, or runtime shutdown. Not the UI's own Ctrl+C: that
    /// path takes the handle before aborting, so this arm never sees it.
    Cancelled,
    /// The task returned normally without a terminal event. Nothing
    /// crashed; the run simply had nothing to say, which the consumer
    /// has no way to tell apart from a run still in progress.
    Silent,
}

/// Read the ending off the `JoinHandle`'s result.
pub(crate) fn classify(result: Result<(), JoinError>) -> RunExit {
    match result {
        Ok(()) => RunExit::Silent,
        Err(e) if e.is_cancelled() => RunExit::Cancelled,
        Err(e) => RunExit::Crashed(panic_text(e)),
    }
}

/// What the user is told. Every branch has to end in something they can
/// act on, because the alternative this replaces is a cursor that never
/// comes back.
pub(crate) fn describe(exit: &RunExit) -> String {
    match exit {
        RunExit::Crashed(what) => format!("the agent run crashed: {what}"),
        RunExit::Cancelled => "the agent run was cancelled".to_string(),
        RunExit::Silent => "the agent run ended without a result".to_string(),
    }
}

/// The panic payload out of a `JoinError`, as text.
fn panic_text(e: JoinError) -> String {
    if !e.is_panic() {
        return "unknown".to_string();
    }
    let payload = e.into_panic();
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_task_is_read_as_a_crash_with_its_message() {
        let handle = tokio::spawn(async { panic!("index out of bounds") });
        match classify(handle.await) {
            RunExit::Crashed(what) => assert!(what.contains("index out of bounds"), "{what}"),
            other => panic!("expected a crash, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_formatted_panic_payload_survives_as_a_string() {
        let tool = "read";
        let handle = tokio::spawn(async move { panic!("{tool} exploded") });
        assert_eq!(
            classify(handle.await),
            RunExit::Crashed("read exploded".to_string())
        );
    }

    #[tokio::test]
    async fn an_aborted_task_is_read_as_cancelled() {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        assert_eq!(classify(handle.await), RunExit::Cancelled);
    }

    /// The negative half: a task that finished normally is not a crash.
    /// The UI reaches this arm only when the handle is still installed,
    /// which after a handled `Done` it never is — so `Silent` means a
    /// run that produced nothing, not a run that produced an answer.
    #[tokio::test]
    async fn a_task_that_simply_returned_is_not_a_crash() {
        let handle = tokio::spawn(async {});
        assert_eq!(classify(handle.await), RunExit::Silent);
    }

    #[test]
    fn every_ending_says_something_the_user_can_read() {
        for exit in [
            RunExit::Crashed("boom".to_string()),
            RunExit::Cancelled,
            RunExit::Silent,
        ] {
            let msg = describe(&exit);
            assert!(msg.contains("the agent run"), "{msg}");
            assert!(!msg.is_empty());
        }
        assert!(describe(&RunExit::Crashed("boom".to_string())).contains("boom"));
    }
}
