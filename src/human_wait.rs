//! Time a run spends waiting on a person.
//!
//! Several tools stop mid-call and wait for the user: every tool that
//! routes through the permission prompt, the `question` tool (whose
//! whole job is to ask), and `/plan` approval. All of them do it from
//! *inside* `LoopTool::execute`, which is also the window the dispatch
//! watchdog bounds (`timeouts.tool_call`, dirge-9tl3).
//!
//! So the watchdog has to be able to tell the two apart. A tool that
//! has been running for ten minutes is pathological; a tool that has
//! been *waiting* for ten minutes means the user is reading the command
//! they were asked to approve. Cutting the second is worse than the
//! stall the watchdog exists to catch — it kills a correct call, and it
//! does it precisely when the user is being careful.
//!
//! A budget bounds work, not a person. [`HumanWait`] marks the stretches
//! that are not work; the watchdog re-arms instead of firing while any
//! are open.
//!
//! The count is process-wide rather than per-call, because tools
//! dispatched in parallel share one task and there is nothing finer to
//! hang it on. One tool's prompt therefore also holds off another tool's
//! watchdog. That errs toward not cutting, which is the direction to err
//! in: the cost is a stuck call surviving until the prompt is answered,
//! against the cost of killing a call somebody was in the middle of
//! approving.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How many waits-on-a-person are open right now.
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);

/// Marks a stretch of a tool call that is spent waiting for the user.
///
/// Hold it across the `.await` that waits for the answer:
///
/// ```ignore
/// let decision = {
///     let _waiting = HumanWait::begin();
///     reply_rx.await
/// };
/// ```
///
/// RAII so the count cannot leak: the guard is released when the answer
/// arrives, when the channel is dropped, and when the future is
/// cancelled mid-wait.
pub struct HumanWait {
    _private: (),
}

impl HumanWait {
    pub fn begin() -> Self {
        OUTSTANDING.fetch_add(1, Ordering::SeqCst);
        HumanWait { _private: () }
    }
}

impl Drop for HumanWait {
    fn drop(&mut self) {
        OUTSTANDING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Is anything currently waiting on the user?
pub fn anyone_waiting() -> bool {
    OUTSTANDING.load(Ordering::SeqCst) > 0
}

/// Serializes every test that reads or writes the process-wide count.
///
/// The counter is one global, so two tests running in parallel see each
/// other's prompts: a test asserting "nobody is waiting" fails when a
/// concurrent one holds a guard, and a test asserting "the watchdog
/// fires" spins forever if one does. Async because the dispatch tests
/// hold it across `.await`.
#[cfg(test)]
pub(crate) static TEST_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nobody_is_waiting_by_default() {
        let _gate = TEST_GATE.lock().await;
        assert!(!anyone_waiting());
    }

    #[tokio::test]
    async fn a_guard_marks_a_wait_and_releases_it() {
        let _gate = TEST_GATE.lock().await;
        {
            let _waiting = HumanWait::begin();
            assert!(anyone_waiting());
        }
        assert!(!anyone_waiting());
    }

    /// Parallel tool dispatch can have several prompts open at once, so
    /// the count has to nest — releasing one must not clear the rest.
    #[tokio::test]
    async fn waits_nest_and_only_the_last_release_clears_it() {
        let _gate = TEST_GATE.lock().await;
        let first = HumanWait::begin();
        let second = HumanWait::begin();
        drop(second);
        assert!(anyone_waiting(), "one prompt is still open");
        drop(first);
        assert!(!anyone_waiting());
    }

    /// A cancelled wait releases too — the guard is dropped whether the
    /// answer arrived, the channel died, or the future was dropped
    /// mid-await. A leaked count would disable the watchdog for the
    /// rest of the session.
    #[tokio::test]
    async fn a_wait_dropped_mid_await_still_releases() {
        let _gate = TEST_GATE.lock().await;
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let waiting = async {
            let _waiting = HumanWait::begin();
            let _ = rx.await;
        };
        // Drop the future while it is parked on the answer.
        tokio::select! {
            _ = waiting => unreachable!("nobody answers"),
            _ = std::future::ready(()) => {}
        }
        assert!(!anyone_waiting());
    }

    /// Every place a tool parks on an answer has to be marked, and the
    /// per-site tests can only cover the sites somebody remembered to
    /// write a test for. This covers the ones nobody has written yet:
    /// a new `reply_rx.await` in the tools tree without a guard beside
    /// it fails here rather than silently handing the watchdog a
    /// deliberating human to cut.
    ///
    /// Deliberately dumb — it counts, it does not parse. That makes it
    /// possible to fool, and it still catches the mistake anyone
    /// actually makes: adding the wait and forgetting the mark.
    #[test]
    fn every_wait_on_an_answer_in_the_tools_tree_is_marked() {
        let sources = [
            ("agent/tools/mod.rs", include_str!("agent/tools/mod.rs")),
            (
                "agent/tools/question.rs",
                include_str!("agent/tools/question.rs"),
            ),
            ("agent/tools/plan.rs", include_str!("agent/tools/plan.rs")),
        ];
        let mut total_waits = 0;
        for (name, src) in sources {
            // Whole file, not a prefix. `#[cfg(test)]` sits on
            // individual items all through these modules, so splitting
            // on it truncates most of the production code — that is how
            // this check first passed while seeing three of four waits.
            // Tests here drive the wait from the answering side
            // (`req.reply.send`), so they add no `reply_rx.await` of
            // their own to confuse the count.
            let waits = src.matches("reply_rx.await").count();
            let marks = src.matches("HumanWait::begin()").count();
            assert!(
                marks >= waits,
                "{name} parks on an answer {waits} time(s) but marks only {marks} of them — \
                 an unmarked wait lets the dispatch watchdog cut a call while the user decides",
            );
            total_waits += waits;
        }
        // If the pattern this scans for ever stops appearing, the check
        // above passes for free. Pin that it is still finding them.
        assert!(
            total_waits >= 4,
            "expected to find the known waits; found {total_waits} — has the shape changed?",
        );
    }
}
