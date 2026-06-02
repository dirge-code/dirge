//! Phase 3 (P3e): the runtime glue that turns a forked phase runner into final
//! text. The phase *logic* (which prompt + tools each phase gets, the
//! explore→plan handoff, the reviewer-runs-code loop) lives in
//! [`crate::agent::plan_workflow`] and is unit-tested there without a runtime.
//!
//! This module supplies the missing half: draining a real [`AgentRunner`]'s
//! event stream into the `String` those orchestration cores expect from their
//! `run_phase` / `run_reviewer` / `run_implement_retry` closures. The
//! UI-side `run_phased_workflow` entry + call-site wiring (behind
//! `phased_workflow_enabled`) is P3e-b — it composes `collect_runner_text`
//! below with `agent.spawn_phase_runner(..)` and a live session.

use crate::agent::plan_workflow::PhaseOutput;
use crate::agent::runner::AgentRunner;
use crate::event::AgentEvent;

/// Drop guard so a cancelled `collect_runner_text` future (an orchestrator
/// timeout or a caller abort) actually stops the forked runner rather than
/// orphaning a task that keeps calling the model in the background. Mirrors
/// the background-review guard in `review.rs`.
struct AbortRunnerOnDrop {
    task: tokio::task::JoinHandle<()>,
    cancel_tx: tokio::sync::mpsc::Sender<()>,
}

impl Drop for AbortRunnerOnDrop {
    fn drop(&mut self) {
        // Cooperative cancel first (lets an in-flight consumer surface a clean
        // cancelled event), then hard abort at the next `.await`.
        let _ = self.cancel_tx.try_send(());
        self.task.abort();
    }
}

/// Drain a forked phase runner to completion and return its final assistant
/// text. `Token`s accumulate; the authoritative `Done { response }` payload is
/// preferred once it arrives (but an empty `Done` keeps the streamed text); the
/// first `Error` surfaces as `Err`. Everything else (tool calls/results, turn
/// boundaries, reasoning) is consumed silently — phases communicate through
/// their final report, not their intermediate chatter.
#[allow(dead_code)] // composed into the UI session loop in P3e-b
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
