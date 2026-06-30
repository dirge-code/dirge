//! Phase 3 (P3e): the runtime glue that turns a forked phase runner into final
//! text. The phase *logic* (which prompt + tools each phase gets, the
//! explore→plan handoff, the reviewer-runs-code loop) lives in
//! [`crate::agent::plan::workflow`] and is unit-tested there without a runtime.
//!
//! This module supplies the missing half: [`collect_runner_text`] drains a
//! real [`AgentRunner`]'s event stream into the final `String`, and
//! [`spawn_review`] forks a write-disabled reviewer OFF the UI thread, turning
//! its verdict into a [`ReviewStep`] delivered over a channel. It also defines
//! the live `/plan` workflow state ([`ActivePlan`] / [`PlanKickoff`]). The
//! interactive entry is the `/plan` slash command (`ui/slash/cmd_plan.rs`): it
//! runs the explore→plan forks via `collect_runner_text` +
//! `agent.spawn_phase_runner(..)`, then the UI loop launches the streamed
//! implement run and `run_handlers/done.rs` spawns the reviewer via
//! [`spawn_review`], whose verdict the `review_phase` arm applies.

use crate::agent::plan::workflow::{PhaseOutput, ReviewStep, next_review_step};
use crate::agent::runner::{AbortRunnerOnDrop, AgentRunner};
use crate::event::AgentEvent;

/// Runtime state for an in-flight `/plan` workflow, carried across `Done`
/// events so the reviewer loop can drive successive implement retries without
/// blocking on the streamed implement run.
pub(crate) struct ActivePlan {
    /// The plan text, reused as the reviewer's task each cycle.
    pub plan: String,
    /// Remaining reviewer-runs-code fix cycles.
    pub cycles_left: usize,
}

/// Kickoff payload the `/plan` command produces once its explore→plan forks
/// finish. The UI loop turns this into the first (streamed) implement run and
/// seeds the [`ActivePlan`] that the reviewer loop then drives.
pub(crate) struct PlanKickoff {
    /// Seeds the implement run (the original request + the plan).
    pub impl_prompt: String,
    /// Becomes the live [`ActivePlan`] when the implement run launches.
    pub active: ActivePlan,
}

/// Events emitted by the async explore→plan task and drained by the UI loop, so
/// the (minutes-long) forks no longer park the event loop [dirge-vuzz]. The
/// loop renders `Progress` lines, launches the implement run on `Ready`, and
/// drops the busy state on `Aborted`.
pub(crate) enum PlanPhaseEvent {
    /// A status line to render. `error` selects the color.
    Progress { text: String, error: bool },
    /// Both forks succeeded — launch the streamed implement run from this.
    Ready(Box<PlanKickoff>),
    /// A phase produced nothing or errored (a `Progress` line already said why).
    Aborted,
}

/// Handle to the spawned explore→plan task: the event stream the UI loop drains
/// plus the task itself, so Ctrl+C can `abort()` it (which drops the in-flight
/// `collect_runner_text` guard and cancels the inner phase runner too).
pub(crate) struct PlanPhaseHandle {
    pub rx: tokio::sync::mpsc::Receiver<PlanPhaseEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Drain a forked phase runner to completion and return its final assistant
/// text. `Token`s accumulate; the authoritative `Done { response }` payload is
/// preferred once it arrives (but an empty `Done` keeps the streamed text); the
/// first `Error` surfaces as `Err`. Everything else (tool calls/results, turn
/// boundaries, reasoning) is consumed silently — phases communicate through
/// their final report, not their intermediate chatter.
pub(crate) async fn collect_runner_text(runner: AgentRunner) -> PhaseOutput {
    let AgentRunner {
        event_rx,
        task,
        cancel_tx,
        ..
    } = runner;
    let _guard = AbortRunnerOnDrop { task, cancel_tx };
    let mut rx = event_rx;
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Token(t) => text.push_str(&t),
            AgentEvent::Done { response, .. } => {
                if !response.is_empty() {
                    text = response.to_string();
                }
                break;
            }
            AgentEvent::Error(msg) => return Err(msg.to_string()),
            _ => {}
        }
    }
    Ok(text)
}

/// Terminal event from the spawned reviewer task (dirge-4koy). One event per
/// review: the parsed [`ReviewStep`], or the reviewer's error.
pub(crate) enum ReviewPhaseEvent {
    Done { result: Result<ReviewStep, String> },
}

/// Handle to an off-thread reviewer pass. Mirrors the `/plan` and compaction
/// phase handles: the loop drains `rx`, can `abort()` the `task` on Ctrl+C, and
/// uses the carried `plan` / `cycles_left` to re-arm [`ActivePlan`] and build
/// the retry prompt when the verdict is `Retry`. `response` / `tool_calls` are
/// the just-finished implement turn's payload, carried so the terminal-verdict
/// branch can run the idle finalization (persist + post-session review) that
/// `handle_done` deferred when it kept the loop busy for the reviewer.
pub(crate) struct ReviewPhaseHandle {
    pub rx: tokio::sync::mpsc::Receiver<ReviewPhaseEvent>,
    pub task: tokio::task::JoinHandle<()>,
    pub plan: String,
    pub cycles_left: usize,
    pub response: String,
    pub tool_calls: Vec<crate::session::ToolCallEntry>,
}

/// Drain a (write-disabled) reviewer runner OFF the UI thread and deliver the
/// parsed [`ReviewStep`] over a channel (dirge-4koy). The reviewer runs the
/// just-written code — tens of seconds to minutes — so awaiting it inline froze
/// the event loop. The caller builds the runner on-thread (it needs `&agent`),
/// then hands it here; the UI loop drives the returned handle from its
/// `select!` and applies the verdict (render / relaunch implement run) when it
/// lands.
pub(crate) fn spawn_review(
    runner: AgentRunner,
    plan: String,
    cycles_left: usize,
    response: String,
    tool_calls: Vec<crate::session::ToolCallEntry>,
) -> ReviewPhaseHandle {
    // Capacity 1: the task sends exactly one terminal event.
    let (tx, rx) = tokio::sync::mpsc::channel::<ReviewPhaseEvent>(1);
    let task = tokio::spawn(async move {
        let result = collect_runner_text(runner)
            .await
            .map(|review| next_review_step(&review, cycles_left));
        let _ = tx.send(ReviewPhaseEvent::Done { result }).await;
    });
    ReviewPhaseHandle {
        rx,
        task,
        plan,
        cycles_left,
        response,
        tool_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Build an `AgentRunner` whose event stream replays `events`, with the
    /// task already finished (so the abort guard's `abort()` is a harmless
    /// no-op, exactly as in production once the runner completes).
    fn runner_replaying(events: Vec<AgentEvent>) -> AgentRunner {
        let (tx, event_rx) = mpsc::channel(events.len().max(1));
        for e in events {
            tx.try_send(e).expect("test channel sized to fit events");
        }
        drop(tx); // close the channel so the drain loop terminates
        let (interject_tx, _) = mpsc::channel(1);
        let (cancel_tx, _) = mpsc::channel(1);
        let task = tokio::spawn(async {});
        AgentRunner {
            event_rx,
            task,
            interject_tx,
            cancel_tx,
        }
    }

    #[tokio::test]
    async fn accumulates_streamed_tokens_until_done() {
        let runner = runner_replaying(vec![
            AgentEvent::Token("hello ".into()),
            AgentEvent::Token("world".into()),
            AgentEvent::Done {
                response: "".into(),
                tokens: 0,
                cost: 0.0,
            },
        ]);
        // Empty Done payload → keep the streamed text.
        assert_eq!(collect_runner_text(runner).await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn prefers_authoritative_done_response() {
        let runner = runner_replaying(vec![
            AgentEvent::Token("partial".into()),
            AgentEvent::Done {
                response: "the full final report".into(),
                tokens: 10,
                cost: 0.01,
            },
        ]);
        assert_eq!(
            collect_runner_text(runner).await.unwrap(),
            "the full final report"
        );
    }

    #[tokio::test]
    async fn error_event_surfaces_as_err() {
        let runner = runner_replaying(vec![
            AgentEvent::Token("some work".into()),
            AgentEvent::Error("model exploded".into()),
        ]);
        assert_eq!(
            collect_runner_text(runner).await,
            Err("model exploded".to_string())
        );
    }

    #[tokio::test]
    async fn stream_closed_without_done_returns_what_streamed() {
        // Channel closes (runner task ended) before a Done — return the
        // accumulated text rather than hanging or erroring.
        let runner = runner_replaying(vec![AgentEvent::Token("orphaned".into())]);
        assert_eq!(collect_runner_text(runner).await.unwrap(), "orphaned");
    }

    /// dirge-4koy: `spawn_review` must drain the reviewer runner OFF the UI
    /// thread and deliver the parsed `ReviewStep` over its channel, so the loop
    /// stays responsive while the reviewer (which runs code) works.
    #[tokio::test]
    async fn spawn_review_emits_approved_step_off_thread() {
        let runner = runner_replaying(vec![AgentEvent::Done {
            response: "review done\n```json\n{\"verdict\":\"DONE\",\"missing\":\"\"}\n```".into(),
            tokens: 0,
            cost: 0.0,
        }]);
        let mut handle = spawn_review(runner, "the plan".to_string(), 3, String::new(), Vec::new());
        // Continuation inputs are carried for the UI arm.
        assert_eq!(handle.plan, "the plan");
        assert_eq!(handle.cycles_left, 3);
        match handle.rx.recv().await.expect("a terminal event") {
            ReviewPhaseEvent::Done { result } => {
                assert!(matches!(result, Ok(ReviewStep::Approved)));
            }
        }
    }

    #[tokio::test]
    async fn spawn_review_surfaces_reviewer_error() {
        let runner = runner_replaying(vec![AgentEvent::Error("reviewer exploded".into())]);
        let mut handle = spawn_review(runner, "p".to_string(), 1, String::new(), Vec::new());
        match handle.rx.recv().await.expect("a terminal event") {
            ReviewPhaseEvent::Done { result } => {
                assert_eq!(result, Err("reviewer exploded".to_string()));
            }
        }
    }
}
