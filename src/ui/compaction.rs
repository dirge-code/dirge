//! Non-blocking compaction (dirge-tv3p / dirge-dtyn).
//!
//! Compaction's slow part is the summarizer LLM call. Running it inline in the
//! UI event loop froze rendering, input, and Ctrl+C for 10–60s. This module is
//! the off-thread half, mirroring the `/plan` phase machinery
//! ([`crate::agent::plan::runtime`]): the loop builds the prompt + resolves the
//! model on-thread ([`crate::ui::slash::prepare_compaction`]), [`spawn`]s the
//! summarizer as a task that streams a terminal event back, and a dedicated
//! `select!` arm installs the result on-thread
//! ([`crate::ui::slash::install_compaction`]) and runs the continuation.
//!
//! The session is loop-owned and is NOT touched while the task runs — the loop
//! gates new prompts/commands until the phase resolves — so the `cut_idx` /
//! `tokens_before` captured at prepare time are still valid at install.

use crate::ui::slash::CompactionRequest;

/// What to do once compaction installs — the three off-thread trigger sites
/// (explicit `/compress`, preemptive, reactive overflow) differ only here. The
/// post-turn auto-compact in `done.rs` is still synchronous and does NOT route
/// through this module (tracked as a follow-up; see dirge-21sb).
pub(crate) enum CompactionThen {
    /// Explicit `/compress`: nothing follows. (A prompt queued while the
    /// summarizer ran is drained into the next turn by the `Finish` arm.)
    Nothing,
    /// Preemptive (pre-prompt) compaction: after install, run a NEW streamed
    /// turn. `run_prompt` is what the runner receives (may be plugin-rewritten);
    /// `record_text` is recorded in the session as the user message (matching
    /// the inline submit path). `last_user_prompt` is already set at submit, so
    /// the arm leaves it. Resent on success AND on compaction failure (the
    /// estimate may have been pessimistic; reactive recovery is the backstop).
    SendPrompt {
        run_prompt: String,
        record_text: String,
    },
    /// Reactive overflow recovery: the prompt already overflowed and is ALREADY
    /// in the session. After a successful compaction we RESUME the task rather
    /// than stranding it (dirge-b899):
    ///   - `made_progress` (streamed text and/or completed tool calls on the
    ///     failed turn): the partial assistant turn was recorded into
    ///     `session.messages` before compacting, so the compacted history
    ///     carries the tool RESULTS. The arm spawns a CONTINUATION ("continue
    ///     from where you left off") — no prompt re-send, so side-effecting
    ///     tools don't re-run.
    ///   - otherwise (pure first-token overflow, nothing streamed): re-send the
    ///     `prompt` against the compacted history (the trailing user message is
    ///     dropped + not re-recorded).
    ///
    /// If compaction made no progress, the user is told to recover manually.
    RetryAfterOverflow { prompt: String, made_progress: bool },
}

/// What the `compaction_phase` arm does after a reactive-overflow compaction
/// finishes installing. Extracted as a pure decision so the regression
/// (dirge-b899: stranding a made-progress turn instead of resuming) is
/// directly testable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OverflowRecovery {
    /// Resume the task as a continuation against the compacted history (the
    /// partial assistant turn + tool results are already recorded).
    Continue,
    /// Re-send the original prompt (nothing streamed before the overflow).
    Resend,
    /// Compaction made no progress — leave the session as-is.
    GiveUp,
}

/// Decide how to recover from a reactive context overflow once compaction
/// has run. `made_progress` is true when the failed turn streamed text and/or
/// completed tool calls (so its partial turn was recorded into the history
/// before compacting).
pub(crate) fn overflow_recovery(compacted: bool, made_progress: bool) -> OverflowRecovery {
    match (compacted, made_progress) {
        (true, true) => OverflowRecovery::Continue,
        (true, false) => OverflowRecovery::Resend,
        (false, _) => OverflowRecovery::GiveUp,
    }
}

/// Terminal event from the spawned summarizer task. (There's no `Progress` —
/// the loop already printed "compressing…" and the spinner animates on-loop.)
pub(crate) enum CompactionPhaseEvent {
    /// The summarizer returned, or a deterministic local fallback summary was
    /// produced; install this summary on the UI thread.
    Done { summary: String },
    /// The summarizer errored (or the injection guard tripped on the prompt
    /// build — though that's caught earlier, on-thread).
    Failed { error: String },
}

/// Handle to the spawned compaction task: the terminal-event channel the loop
/// drains, the task (so Ctrl+C can `abort()` it), the install inputs captured
/// on-thread, and the continuation.
pub(crate) struct CompactionPhaseHandle {
    pub rx: tokio::sync::mpsc::Receiver<CompactionPhaseEvent>,
    pub task: tokio::task::JoinHandle<()>,
    pub cut_idx: usize,
    pub tokens_before: u64,
    pub then: CompactionThen,
}

/// Spawn the summarizer LLM off-thread and return the handle the UI loop drives
/// from its `select!`. `req` carries the model + prebuilt prompt produced by
/// `prepare_compaction` on the UI thread.
pub(crate) fn spawn_local(
    summary: String,
    cut_idx: usize,
    tokens_before: u64,
    then: CompactionThen,
) -> CompactionPhaseHandle {
    let (tx, rx) = tokio::sync::mpsc::channel::<CompactionPhaseEvent>(1);
    let task = tokio::spawn(async move {
        let _ = tx.send(CompactionPhaseEvent::Done { summary }).await;
    });
    CompactionPhaseHandle {
        rx,
        task,
        cut_idx,
        tokens_before,
        then,
    }
}

pub(crate) fn spawn(req: CompactionRequest, then: CompactionThen) -> CompactionPhaseHandle {
    let CompactionRequest {
        model,
        prompt,
        cut_idx,
        tokens_before,
    } = req;
    // Capacity 1: the task sends exactly one terminal event.
    let (tx, rx) = tokio::sync::mpsc::channel::<CompactionPhaseEvent>(1);
    let task = tokio::spawn(async move {
        let event = match crate::provider::run_compaction(model, prompt).await {
            Ok(summary) => CompactionPhaseEvent::Done { summary },
            Err(e) => CompactionPhaseEvent::Failed {
                error: e.to_string(),
            },
        };
        let _ = tx.send(event).await;
    });
    CompactionPhaseHandle {
        rx,
        task,
        cut_idx,
        tokens_before,
        then,
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::{OverflowRecovery, overflow_recovery};

    #[test]
    fn made_progress_resumes_as_continuation() {
        // dirge-b899: the failed turn ran tools / streamed text — resume the
        // task, do NOT strand it (the old code refused when tools had run).
        assert_eq!(overflow_recovery(true, true), OverflowRecovery::Continue);
    }

    #[test]
    fn no_progress_resends_prompt() {
        // Pure first-token overflow: nothing streamed, safe to re-send.
        assert_eq!(overflow_recovery(true, false), OverflowRecovery::Resend);
    }

    #[test]
    fn uncompacted_gives_up_regardless_of_progress() {
        assert_eq!(overflow_recovery(false, true), OverflowRecovery::GiveUp);
        assert_eq!(overflow_recovery(false, false), OverflowRecovery::GiveUp);
    }
}
