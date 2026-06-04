//! Phase 3 (P3e): the runtime glue that turns a forked phase runner into final
//! text. The phase *logic* (which prompt + tools each phase gets, the
//! explore→plan handoff, the reviewer-runs-code loop) lives in
//! [`crate::agent::plan::workflow`] and is unit-tested there without a runtime.
//!
//! This module supplies the missing half: [`collect_runner_text`] drains a
//! real [`AgentRunner`]'s event stream into the final `String`, and
//! [`review_once`] forks a write-disabled reviewer and turns its verdict into a
//! [`ReviewStep`]. It also defines the live `/plan` workflow state
//! ([`ActivePlan`] / [`PlanKickoff`]). The interactive entry is the `/plan`
//! slash command (`ui/slash/cmd_plan.rs`): it runs the explore→plan forks via
//! `collect_runner_text` + `agent.spawn_phase_runner(..)`, then the UI loop
//! launches the streamed implement run and `run_handlers/done.rs` drives the
//! reviewer loop with `review_once`.

use crate::agent::plan::workflow::{
    PhaseOutput, REVIEWER_TOOLS, ReviewStep, next_review_step, reviewer_prompt,
};
use crate::agent::runner::{AbortRunnerOnDrop, AgentRunner};
use crate::event::AgentEvent;
use crate::provider::AnyAgent;

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

/// Fork a write-disabled reviewer that runs the just-written code and decide
/// the next step. `transcript` must reflect the implementation that just
/// completed (build it from the post-implement session). On a reviewer fork
/// error the error surfaces as `Err` — the caller ends the loop rather than
/// silently shipping.
pub(crate) async fn review_once(
    agent: &AnyAgent,
    plan: &str,
    transcript: String,
    cycles_left: usize,
) -> Result<ReviewStep, String> {
    let runner = agent.spawn_phase_runner(reviewer_prompt(plan), transcript, REVIEWER_TOOLS);
    let review = collect_runner_text(runner).await?;
    Ok(next_review_step(&review, cycles_left))
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
}
