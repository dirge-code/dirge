//! Compaction recall eval harness.
//!
//! Inspired by the snapcompact write-up (blog.can.ac/2026/06/10/snapcompact):
//! the sharpest finding there isn't the image trick, it's the *measurement* —
//! a verbatim-recall probe that exposes how badly lossy compaction drops
//! load-bearing facts (their prose-summary baseline scored "UNREADABLE"
//! 240/240). dirge's compaction is already structured and concreteness-forcing
//! ([`build_summary_prompt`] asks for "file paths, command outputs, error
//! messages, line numbers, and specific values"), but nothing measured whether
//! those facts actually survive.
//!
//! This harness plants a canonical set of facts in the region a session
//! compacts away, then scores how many survive:
//!
//!   * [`planted_facts_reach_the_summarizer`] (deterministic, CI): the part
//!     dirge *controls* — every planted fact must reach the prompt handed to
//!     the summarizer. Guards against a pre-LLM regression (truncation, window
//!     selection, serialization) silently starving the summarizer of facts.
//!   * `task_supersession_signal_reaches_the_summarizer` (deterministic, CI):
//!     guards #443 — a #443-shaped session (original task done, follow-up live)
//!     must carry BOTH the completion and follow-up signals into the summarizer
//!     prompt, so the prompt fix can mark the original complete and surface the
//!     follow-up as the active task.
//!   * [`run_recall_eval`]: the full article-style probe — compact through a
//!     [`SummarizeFn`] and score the *summary*. Driven by a mock here so it
//!     runs in CI; point it at a real model's `SummarizeFn` off-CI to measure
//!     and tune actual compaction fidelity.

use std::sync::Arc;

use serde_json::{Value, json};

use super::compression::{
    PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT, SummarizeFn, build_summary_prompt,
    compute_compress_window, estimate_messages_tokens, summary_budget,
};

/// A load-bearing detail planted in the to-be-compacted history. A faithful
/// compaction must keep `needle` verbatim; the article's data shows prose
/// summaries quietly drop exactly these.
pub(crate) struct PlantedFact {
    /// What kind of detail it is — only used to make a dropped-fact report
    /// legible ("dropped the error string", "dropped the config value").
    pub kind: &'static str,
    /// The exact substring that must survive compaction.
    pub needle: &'static str,
}

/// The canonical seed set: one of each category the article calls out as
/// commonly lost. Strings are deliberately distinctive so a substring match
/// can't be satisfied by coincidental filler text.
pub(crate) fn seed_facts() -> Vec<PlantedFact> {
    vec![
        PlantedFact {
            kind: "file path",
            needle: "src/widgets/aurora_panel.rs",
        },
        PlantedFact {
            kind: "code location",
            needle: "render_frame at line 287",
        },
        PlantedFact {
            kind: "error message",
            needle: "index out of bounds: the len is 4 but the index is 9",
        },
        PlantedFact {
            kind: "config value",
            needle: "AURORA_MAX_RETRIES=7",
        },
        PlantedFact {
            kind: "identifier",
            needle: "tok_9Q2x7Lp4dF",
        },
        PlantedFact {
            kind: "numeric value",
            needle: "timeout of 4500ms",
        },
    ]
}

/// Build a conversation long enough to compact, with every fact embedded in
/// the *middle* turns (so they land in the window between the protected head
/// and tail, not in the verbatim-preserved edges). The fact-bearing turns are
/// realistic tool results / assistant notes; filler pads them apart.
pub(crate) fn session_with_facts(facts: &[PlantedFact]) -> Vec<Value> {
    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "you are dirge, a coding agent"}),
        json!({"role": "user", "content": "fix the flaky aurora panel render"}),
    ];

    // Lead-in filler so the first fact is well past the protected head.
    for i in 0..4 {
        msgs.push(json!({"role": "assistant", "content": format!("looking into it (step {i})")}));
        msgs.push(json!({"role": "user", "content": format!("ok, continue {i}")}));
    }

    // Fact-bearing turns, each separated by a user turn so the window snaps
    // cleanly around them.
    for fact in facts {
        msgs.push(json!({
            "role": "assistant",
            "content": format!(
                "noted ({}): {} — keep this for later",
                fact.kind, fact.needle
            ),
        }));
        msgs.push(json!({"role": "user", "content": "got it, keep going"}));
    }

    // Trailing filler, then the protected tail ending on the latest request.
    for i in 0..4 {
        msgs.push(json!({"role": "assistant", "content": format!("almost there (step {i})")}));
        msgs.push(json!({"role": "user", "content": format!("keep going {i}")}));
    }
    msgs.push(json!({"role": "user", "content": "now write the regression test"}));
    msgs
}

/// How many planted facts survived in `text`.
pub(crate) struct RecallReport {
    pub total: usize,
    pub survived: usize,
    /// `(kind, needle)` for each fact NOT found — the legible failure list.
    pub dropped: Vec<(&'static str, &'static str)>,
}

impl RecallReport {
    pub fn all_survived(&self) -> bool {
        self.dropped.is_empty()
    }
}

/// Score verbatim recall: a fact survives iff its exact `needle` appears in
/// `text`. Verbatim by design — the whole point is that paraphrase loses the
/// detail (a path or error string is only useful exact).
pub(crate) fn score_recall(text: &str, facts: &[PlantedFact]) -> RecallReport {
    let dropped: Vec<(&'static str, &'static str)> = facts
        .iter()
        .filter(|f| !text.contains(f.needle))
        .map(|f| (f.kind, f.needle))
        .collect();
    RecallReport {
        total: facts.len(),
        survived: facts.len() - dropped.len(),
        dropped,
    }
}

/// Full article-style probe: build a seeded session, run it through dirge's
/// real compaction window + prompt builder, hand the prompt to `summarize`,
/// and score how many facts survive in the resulting summary. The summarizer
/// is the only pluggable piece — a mock for CI, a real model for measurement.
pub(crate) async fn run_recall_eval(summarize: SummarizeFn) -> RecallReport {
    let facts = seed_facts();
    let msgs = session_with_facts(&facts);
    let (start, end) = compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
    let middle = &msgs[start..end];
    let prompt = build_summary_prompt(
        middle,
        summary_budget(estimate_messages_tokens(middle)),
        None,
        None,
    );
    let summary = summarize(prompt).await.unwrap_or_default();
    score_recall(&summary, &facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The part dirge controls: every planted fact must reach the prompt the
    /// summarizer sees. If this fails, a window/truncation/serialization change
    /// is dropping facts BEFORE the model ever gets a chance to keep them.
    #[test]
    fn planted_facts_reach_the_summarizer() {
        let facts = seed_facts();
        let msgs = session_with_facts(&facts);
        let (start, end) =
            compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
        assert!(
            start < end,
            "session must produce a non-empty compaction window"
        );

        let middle = &msgs[start..end];
        let prompt = build_summary_prompt(
            middle,
            summary_budget(estimate_messages_tokens(middle)),
            None,
            None,
        );
        let report = score_recall(&prompt, &facts);
        assert!(
            report.all_survived(),
            "facts dropped before reaching the summarizer: {:?}",
            report.dropped
        );
    }

    /// Build a #443-shaped history: an early turn assigns the ORIGINAL task,
    /// a middle turn marks it DONE, and a later turn establishes a follow-up as
    /// the live work. Sized/interleaved like [`session_with_facts`] so
    /// `compute_compress_window` snaps a non-empty middle around all three (head
    /// snaps forward to the first user turn ≥ `PROTECT_HEAD_DEFAULT`, tail snaps
    /// backward to the last user turn ≤ `n - PROTECT_TAIL_DEFAULT`). Content is
    /// scalar-string and well under the 2000-char per-turn truncation in
    /// `serialize_turns_for_summary`, so the supersession signals survive.
    fn session_443_task_supersession() -> Vec<Value> {
        let mut msgs: Vec<Value> = vec![
            json!({"role": "system", "content": "you are dirge, a coding agent"}),
            json!({"role": "user", "content": "let's work on the chat server"}),
        ];

        // Lead-in filler so the first signal is well past the protected head.
        for i in 0..4 {
            msgs.push(json!({"role": "assistant", "content": format!("on it (step {i})")}));
            msgs.push(json!({"role": "user", "content": format!("ok, continue {i}")}));
        }

        // Original task assignment — lands in the compacted middle.
        msgs.push(json!({
            "role": "user",
            "content": "Convert the TCP chat server from tokio to stdlib and add an integration test",
        }));
        msgs.push(json!({"role": "assistant", "content": "starting the conversion"}));
        msgs.push(json!({"role": "user", "content": "go ahead"}));

        // Original task COMPLETED — the supersession completion signal.
        msgs.push(json!({
            "role": "assistant",
            "content": "stdlib conversion complete — no tokio remains; cargo build passes",
        }));
        msgs.push(json!({"role": "user", "content": "great, what now"}));

        // Follow-up becomes the live work — the supersession follow-up signal.
        msgs.push(json!({
            "role": "user",
            "content": "the integration test hangs — debugging the race in the accept loop",
        }));
        msgs.push(json!({"role": "assistant", "content": "looking at the accept loop"}));
        msgs.push(json!({"role": "user", "content": "keep going"}));

        // Trailing filler, then the protected tail ending on the latest request.
        for i in 0..4 {
            msgs.push(json!({"role": "assistant", "content": format!("still on it (step {i})")}));
            msgs.push(json!({"role": "user", "content": format!("keep going {i}")}));
        }
        msgs.push(json!({"role": "user", "content": "so where does the race actually come from?"}));
        msgs
    }

    /// #443: after compaction the model re-derived the ORIGINAL task ("convert
    /// to stdlib") as if still pending, when it was already DONE and the live
    /// work was a follow-up (debugging a hanging integration test). The summary
    /// PROMPT fix (sibling: `build_summary_prompt`/`SUMMARY_PREFIX`) can only
    /// mark the original complete and surface the follow-up if BOTH signals
    /// actually reach the summarizer. This guards the dirge-controlled part:
    /// window selection + serialization must carry the completion signal AND
    /// the follow-up signal into the prompt. A pre-LLM regression (a window that
    /// drops the "complete" turn, or truncation that eats the follow-up) would
    /// starve the summarizer of the supersession signal and reintroduce #443
    /// before the model is ever consulted.
    #[test]
    fn task_supersession_signal_reaches_the_summarizer() {
        let msgs = session_443_task_supersession();
        let (start, end) =
            compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
        assert!(
            start < end,
            "session must produce a non-empty compaction window"
        );

        let middle = &msgs[start..end];
        let prompt = build_summary_prompt(
            middle,
            summary_budget(estimate_messages_tokens(middle)),
            None,
            None,
        );
        assert!(
            prompt.contains("stdlib conversion complete") && prompt.contains("no tokio remains"),
            "completion signal (original task DONE) must reach the summarizer prompt"
        );
        assert!(
            prompt.contains("integration test hangs") && prompt.contains("debugging the race"),
            "follow-up signal (live work) must reach the summarizer prompt"
        );
    }

    /// The scorer must actually catch a lossy (paraphrasing) summary — the
    /// failure mode the article exposes.
    #[test]
    fn scorer_flags_a_lossy_summary() {
        let facts = seed_facts();
        let lossy = "## Active Task\nwrite a regression test\n\n\
                     ## Critical Context\nThe agent fixed a panic in the panel \
                     widget and tuned a retry config and a timeout.";
        let report = score_recall(lossy, &facts);
        assert!(
            report.survived < report.total,
            "a paraphrased summary must lose facts; survived {}/{}",
            report.survived,
            report.total
        );
        assert!(
            report
                .dropped
                .iter()
                .any(|(kind, _)| *kind == "error message"),
            "the verbatim error string should be among the dropped: {:?}",
            report.dropped
        );
    }

    /// End-to-end harness: a faithful summarizer (echoes the concrete facts)
    /// scores full recall. Proves the eval wiring works and is ready to be
    /// driven by a real model's `SummarizeFn`.
    #[tokio::test]
    async fn eval_credits_a_faithful_summarizer() {
        // A faithful summary mirrors what dirge's prompt asks for: it keeps the
        // concrete file paths, error strings, and values verbatim. Build it
        // from the facts directly (as a good model would) rather than echoing
        // the prompt, so the scorer is exercised over an independent string.
        let faithful: SummarizeFn = Arc::new(|_prompt: String| {
            let body = seed_facts()
                .iter()
                .map(|f| format!("- {}: {}", f.kind, f.needle))
                .collect::<Vec<_>>()
                .join("\n");
            Box::pin(async move { Ok(format!("## Critical Context\n{body}")) })
        });
        let report = run_recall_eval(faithful).await;
        assert!(
            report.all_survived(),
            "faithful summarizer should preserve all facts: {:?}",
            report.dropped
        );
    }

    /// End-to-end harness: a lossy summarizer is caught with a non-empty
    /// dropped list — what the eval would report for a model that paraphrases.
    #[tokio::test]
    async fn eval_catches_a_lossy_summarizer() {
        let lossy: SummarizeFn = Arc::new(|_prompt: String| {
            Box::pin(async move {
                Ok("## Active Task\nwrite the regression test\n\n\
                    ## Remaining Work\nthe agent investigated a rendering bug and \
                    adjusted some configuration."
                    .to_string())
            })
        });
        let report = run_recall_eval(lossy).await;
        assert!(
            !report.all_survived(),
            "a paraphrasing summarizer must be flagged"
        );
        assert_eq!(report.survived, 0, "this summary keeps none of the needles");
    }
}
