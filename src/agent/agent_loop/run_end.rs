//! Every run's event stream ends with a terminal event.
//!
//! `AgentEvent::Done`, `Error`, `Interjected` and `ContextOverflow` are
//! the four ways a run can be over, and every consumer is written
//! against that: the TUI clears `is_running` on them, `--print` reports
//! the response it collected, ACP closes the turn. Until this module
//! existed, the guarantee was only ever *incidental* — it held because
//! `run_agent_loop` happened to emit `AgentEnd` on the paths anyone had
//! looked at.
//!
//! It does not hold when the task dies. `panic = "abort"` is not set,
//! so a panic anywhere in the run — a tool's `unwrap`, a slice index
//! off a char boundary, an overflow in a debug build — unwinds out of
//! the task, `tokio` catches it, and the event channel simply closes.
//! What each consumer does with a channel that closed mid-run is the
//! real bug: the TUI's `Some(event) = rx.recv()` select arm silently
//! disables itself and the run stays "running" forever, which is a hang
//! with nothing on screen to explain it; `--print` and ACP return the
//! partial answer as if it were the whole one.
//!
//! So the guarantee is made a property of the run's LIFETIME instead of
//! its control flow. [`RunEpitaph`] holds a sender for as long as the
//! task exists; if the task ends without a terminal event having gone
//! out, the epitaph sends one on its way down. Unwinding runs drop
//! glue, so this fires on the panic path too — which is the path that
//! needed it.
//!
//! The consumers do not change. A crash arrives as the error it always
//! should have been.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use compact_str::CompactString;
use tokio::sync::mpsc;

use crate::event::AgentEvent;
use crate::panic_report::PanicRecord;

/// Does this event end the run?
///
/// The four terminal variants, named in one place so the epitaph and
/// the consumers cannot disagree about what "the run finished" means.
pub(crate) fn is_terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Done { .. }
            | AgentEvent::Error(_)
            | AgentEvent::Interjected { .. }
            | AgentEvent::ContextOverflow { .. }
    )
}

/// Shared "a terminal event went out" flag, set by whoever forwards
/// events and read by the epitaph as it drops.
#[derive(Clone, Default)]
pub(crate) struct RunSettled(Arc<AtomicBool>);

impl RunSettled {
    pub(crate) fn mark(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Sends the run's terminal event when nothing else did.
///
/// Held by the spawned run task and dropped with it — on a clean
/// return, on an unwind, and when the future is dropped by
/// `JoinHandle::abort`.
pub(crate) struct RunEpitaph {
    tx: mpsc::Sender<AgentEvent>,
    settled: RunSettled,
    /// Where the panic text comes from. A field, not a direct call, so
    /// a test can plant a record without racing every other test in the
    /// binary over the process-wide slot — and so "only claim it while
    /// unwinding" is a rule that can be shown to hold.
    claim: fn() -> Option<PanicRecord>,
}

impl RunEpitaph {
    pub(crate) fn new(tx: mpsc::Sender<AgentEvent>, settled: RunSettled) -> Self {
        RunEpitaph {
            tx,
            settled,
            claim: crate::panic_report::take,
        }
    }

    #[cfg(test)]
    fn claiming(
        tx: mpsc::Sender<AgentEvent>,
        settled: RunSettled,
        claim: fn() -> Option<PanicRecord>,
    ) -> Self {
        RunEpitaph { tx, settled, claim }
    }
}

impl Drop for RunEpitaph {
    fn drop(&mut self) {
        if self.settled.is_set() {
            return;
        }
        // Claim the panic record only while actually unwinding. An
        // aborted run must not adopt some unrelated caught panic's
        // message, and must not consume the record that panic's own
        // survivor (or the terminal teardown) is going to report.
        let panic = if std::thread::panicking() {
            (self.claim)()
        } else {
            None
        };
        // `try_send`, because this runs in drop glue and cannot await.
        // A full channel means the consumer is not keeping up, which is
        // its own problem; a closed one means it already stopped
        // listening, which is the normal shape of an abort.
        let _ = self
            .tx
            .try_send(AgentEvent::Error(CompactString::from(epitaph_message(
                panic,
            ))));
    }
}

/// What the user is told when a run ends with no result.
///
/// The panic text is the useful half — "the agent crashed" alone sends
/// someone to the log for a message the hook already captured.
pub(crate) fn epitaph_message(panic: Option<PanicRecord>) -> String {
    match panic {
        Some(p) => format!("the agent run crashed: {}", p.describe()),
        None => "the agent run ended without a result".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> AgentEvent {
        AgentEvent::Done {
            response: CompactString::from("hi"),
            tokens: 0,
            cost: 0.0,
        }
    }

    #[test]
    fn the_four_events_that_end_a_run_are_terminal() {
        assert!(is_terminal(&done()));
        assert!(is_terminal(&AgentEvent::Error(CompactString::from("x"))));
        assert!(is_terminal(&AgentEvent::Interjected {
            partial_response: CompactString::from(""),
            tokens: 0,
        }));
        assert!(is_terminal(&AgentEvent::ContextOverflow {
            prompt: CompactString::from("p"),
            error: CompactString::from("e"),
        }));
    }

    /// The negative half: mid-run events must not settle the run, or a
    /// crash after the first token would go unreported.
    #[test]
    fn mid_run_events_are_not_terminal() {
        assert!(!is_terminal(&AgentEvent::Token(CompactString::from("a"))));
        assert!(!is_terminal(&AgentEvent::Reasoning(CompactString::from(
            "a"
        ))));
        assert!(!is_terminal(&AgentEvent::ToolCall {
            id: CompactString::from("1"),
            name: CompactString::from("bash"),
            args: serde_json::json!({}),
        }));
        assert!(!is_terminal(&AgentEvent::TurnEnd { index: 0 }));
        assert!(!is_terminal(&AgentEvent::SystemNotice {
            content: CompactString::from("note"),
        }));
    }

    #[tokio::test]
    async fn a_run_that_ends_with_no_terminal_event_gets_one() {
        let (tx, mut rx) = mpsc::channel(8);
        drop(RunEpitaph::new(tx, RunSettled::default()));
        let ev = rx.recv().await.expect("the epitaph must speak");
        assert!(is_terminal(&ev), "what it sends must end the run: {ev:?}");
        match ev {
            AgentEvent::Error(msg) => assert!(msg.contains("without a result"), "{msg}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_run_that_reported_its_own_end_gets_nothing_added() {
        let (tx, mut rx) = mpsc::channel(8);
        let settled = RunSettled::default();
        settled.mark();
        drop(RunEpitaph::new(tx, settled));
        assert!(
            rx.try_recv().is_err(),
            "a run that already emitted Done must not also emit an error"
        );
    }

    /// The case this exists for: the task unwinds, `tokio` catches it,
    /// and the consumer would otherwise see nothing but a closed
    /// channel.
    ///
    /// The assertion is on the shape, not the text — whether the
    /// message names the panic depends on the process-wide recording
    /// hook, which a test binary shares with every other test. What
    /// must hold unconditionally is that a crashed run still ends its
    /// stream with a terminal event, in order, before the close.
    #[tokio::test]
    async fn a_crashed_run_still_ends_its_stream() {
        let (tx, mut rx) = mpsc::channel(8);
        let settled = RunSettled::default();
        let task = tokio::spawn(async move {
            let _epitaph = RunEpitaph::new(tx, settled);
            panic!("tool exploded");
        });
        assert!(task.await.is_err(), "the task must have panicked");

        let ev = rx
            .recv()
            .await
            .expect("a crashed run still ends the stream");
        assert!(is_terminal(&ev), "{ev:?}");
        // The epitaph's sender is the last one alive, so the consumer
        // sees the end before the close rather than instead of it.
        assert!(rx.recv().await.is_none());
    }

    fn a_planted_record() -> Option<PanicRecord> {
        Some(PanicRecord {
            message: "somebody else's problem".to_string(),
            location: None,
        })
    }

    /// A run whose future is dropped without panicking (`abort`) must
    /// not adopt a record left by some other, already-handled panic
    /// elsewhere in the process — nor consume it, since that panic's
    /// own survivor or the terminal teardown is going to report it.
    /// There IS a record waiting here; the epitaph must leave it alone.
    #[tokio::test]
    async fn an_aborted_run_does_not_adopt_someone_elses_panic() {
        let (tx, mut rx) = mpsc::channel(8);
        drop(RunEpitaph::claiming(
            tx,
            RunSettled::default(),
            a_planted_record,
        ));
        match rx.recv().await.expect("still ends the stream") {
            AgentEvent::Error(msg) => {
                assert!(!msg.contains("crashed"), "{msg}");
                assert!(!msg.contains("somebody else"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// The other half: while it IS unwinding, the record is exactly
    /// what the epitaph should be reporting.
    #[tokio::test]
    async fn a_run_that_is_unwinding_does_claim_the_panic() {
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let _epitaph = RunEpitaph::claiming(tx, RunSettled::default(), a_planted_record);
            panic!("down we go");
        });
        assert!(task.await.is_err());
        match rx.recv().await.expect("still ends the stream") {
            AgentEvent::Error(msg) => {
                assert!(msg.contains("crashed"), "{msg}");
                assert!(msg.contains("somebody else"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn the_message_carries_the_panic_when_there_was_one() {
        let msg = epitaph_message(Some(PanicRecord {
            message: "boom".to_string(),
            location: Some("src/x.rs:1:1".to_string()),
        }));
        assert!(msg.contains("boom"));
        assert!(msg.contains("src/x.rs:1:1"));
    }
}
