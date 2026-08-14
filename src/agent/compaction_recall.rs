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

/// A harder seed set (dirge-e31n.7).
///
/// [`seed_facts`] + [`session_with_facts`] deliberately make the facts easy:
/// six of them, each announced by its own turn as `noted (file path): X — keep
/// this for later`. That is right for what those guard — that dirge's own
/// window/serialization carries facts to the summarizer — but useless for
/// comparing SCHEMAS, because any competent summarizer keeps all six and both
/// arms score 6/6. A null result there would say nothing about the schema, only
/// that the fixture was too easy.
///
/// So this set is twenty facts, none announced, each buried inside plausible
/// tool output with surrounding noise. The summarizer has to decide what is
/// load-bearing rather than copy a labelled list — which is the thing a schema
/// can plausibly change.
pub(crate) fn hard_facts() -> Vec<PlantedFact> {
    vec![
        PlantedFact {
            kind: "file path",
            needle: "crates/ingest/src/backfill/checkpoint.rs",
        },
        PlantedFact {
            kind: "file path",
            needle: "config/staging/ingest.toml",
        },
        PlantedFact {
            kind: "code location",
            needle: "resume_from_offset at line 412",
        },
        PlantedFact {
            kind: "code location",
            needle: "checkpoint.rs:88",
        },
        PlantedFact {
            kind: "error message",
            needle: "called `Option::unwrap()` on a `None` value",
        },
        PlantedFact {
            kind: "error message",
            needle: "connection pool exhausted after 30s",
        },
        PlantedFact {
            kind: "error message",
            needle: "checksum mismatch: expected 8f3a1c, got 2b90de",
        },
        PlantedFact {
            kind: "config value",
            needle: "INGEST_BATCH_SIZE=512",
        },
        PlantedFact {
            kind: "config value",
            needle: "pool.max_connections = 16",
        },
        PlantedFact {
            kind: "config value",
            needle: "retry.backoff_ms = 250",
        },
        PlantedFact {
            kind: "identifier",
            needle: "job_7fK2pQx9",
        },
        PlantedFact {
            kind: "identifier",
            needle: "shard-a4e1",
        },
        PlantedFact {
            kind: "identifier",
            needle: "migration 0042_add_offset_index",
        },
        PlantedFact {
            kind: "numeric value",
            needle: "1_048_576 rows",
        },
        PlantedFact {
            kind: "numeric value",
            needle: "offset 883421",
        },
        PlantedFact {
            kind: "numeric value",
            needle: "p99 of 4.7s",
        },
        PlantedFact {
            kind: "command",
            needle: "cargo test -p ingest --features backfill",
        },
        PlantedFact {
            kind: "command",
            needle: "psql -c 'select max(offset) from ingest_log'",
        },
        PlantedFact {
            kind: "user constraint",
            needle: "do not touch the production shard",
        },
        PlantedFact {
            kind: "rejected alternative",
            needle: "rejected the truncate-and-replay approach",
        },
    ]
}

/// Plausible filler that reads like real tool output, so a fact is one line
/// among many rather than the only content of its turn.
fn noise(seed: usize, lines: usize) -> String {
    (0..lines)
        .map(|i| {
            let n = seed * 31 + i * 7;
            format!(
                "  ingest::backfill::worker  batch {n} ok  \
                 rows={} lag_ms={} shard=b{}",
                200 + n % 97,
                12 + n % 43,
                n % 8
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A realistic debugging session carrying [`hard_facts`], each embedded in a
/// noisy tool result rather than announced.
///
/// Every fact-bearing turn stays well under
/// `compression::SUMMARY_TURN_CHARS` (2000) so the per-turn truncation is not
/// silently doing the dropping — that would measure the serializer, not the
/// schema. `planted_hard_facts_reach_the_summarizer` pins it.
pub(crate) fn noisy_session(facts: &[PlantedFact]) -> Vec<Value> {
    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "you are dirge, a coding agent"}),
        json!({"role": "user", "content": "the nightly ingest backfill is stalling — find out why"}),
    ];

    // Volume matters as much as the facts. `summary_budget` is 20% of the
    // material once past its 2000-token floor, so the session has to be big
    // enough that the floor is not what the summarizer is working against —
    // otherwise the "hard" fixture is only mildly compressed and both arms keep
    // everything. Volume comes from MORE turns, not longer ones:
    // `serialize_turns_for_summary` truncates a non-user turn at 2000 chars.
    for i in 0..8 {
        msgs.push(json!({"role": "assistant", "content": format!("pulling the worker logs (pass {i})\n{}", noise(i, 18))}));
        msgs.push(json!({"role": "user", "content": format!("keep digging {i}")}));
    }

    for (i, fact) in facts.iter().enumerate() {
        // The fact sits in the middle of ordinary output, phrased the way that
        // kind of detail actually shows up.
        let body = match fact.kind {
            "command" => format!("{}\n$ {}\n{}", noise(i, 4), fact.needle, noise(i + 50, 4)),
            "user constraint" | "rejected alternative" => {
                format!("{}\n{}\n{}", noise(i, 3), fact.needle, noise(i + 60, 3))
            }
            _ => format!("{}\n  {}\n{}", noise(i, 5), fact.needle, noise(i + 70, 5)),
        };
        let role = if fact.kind == "user constraint" {
            "user"
        } else {
            "assistant"
        };
        msgs.push(json!({"role": role, "content": body}));
        msgs.push(json!({"role": "user", "content": format!("ok, and then? ({i})")}));
        // Filler between facts, so they are spread through the material rather
        // than arriving as a dense run the summarizer can lift wholesale.
        msgs.push(json!({"role": "assistant", "content": format!("continuing the sweep ({i})\n{}", noise(i + 120, 18))}));
        msgs.push(json!({"role": "user", "content": format!("carry on ({i})")}));
    }

    for i in 0..8 {
        msgs.push(json!({"role": "assistant", "content": format!("narrowing it down (pass {i})\n{}", noise(i + 90, 18))}));
        msgs.push(json!({"role": "user", "content": format!("go on {i}")}));
    }
    msgs.push(json!({"role": "user", "content": "so what is the actual fix?"}));
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

/// Strip markdown presentation before matching (dirge-e31n.7).
///
/// A raw `contains` looked right and was not. Measured against a live model,
/// three of the "dropped" facts in one run were present and faithful:
///
///   needle  `called \`Option::unwrap()\` on a \`None\` value`
///   written `` `called Option::unwrap() on a None value` ``   (inner ticks eaten
///           by the outer code span)
///   written ``` `called \`Option::unwrap()\` on a \`None\` value` ``` (escaped)
///
///   needle  `offset 883421`
///   written ``offset `883421` ``    (the model code-formatted the number)
///
/// Wrapping an identifier in backticks is the natural thing for a model
/// writing markdown to do, so scoring it as a loss measures formatting habits
/// and reports them as fidelity. Worse, it would do so UNEVENLY — the arm that
/// writes more prose formats more — which is precisely the difference under
/// test.
///
/// Only presentation is removed: backticks, backslashes, and runs of
/// whitespace. Case, punctuation, and every character of the identifier itself
/// still have to match, so a paraphrase is still a miss —
/// `scorer_still_catches_a_paraphrase` pins that this did not turn into a
/// scorer that credits anything.
fn normalize_for_match(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| *c != '`' && *c != '\\').collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Score verbatim recall: a fact survives iff its `needle` appears in `text`,
/// compared after [`normalize_for_match`]. Verbatim by design — the whole point
/// is that paraphrase loses the detail (a path or error string is only useful
/// exact).
pub(crate) fn score_recall(text: &str, facts: &[PlantedFact]) -> RecallReport {
    let haystack = normalize_for_match(text);
    let dropped: Vec<(&'static str, &'static str)> = facts
        .iter()
        .filter(|f| !haystack.contains(&normalize_for_match(f.needle)))
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
    // dirge-tgb9: the fixture is dirge's own and contains no fence delimiter,
    // so a failure here means the fixture grew one — loud is correct.
    let prompt = build_summary_prompt(
        middle,
        summary_budget(estimate_messages_tokens(middle)),
        None,
        None,
    )
    .expect("recall fixture must not contain the reserved fence delimiter");
    let summary = summarize(prompt).await.unwrap_or_default();
    score_recall(&summary, &facts)
}

/// Run the hard fixture through `summarize` once, under `schema`, and score
/// the summary.
///
/// Everything except the section template is identical across schemas — same
/// transcript, same window, same budget, same scorer — so a difference in the
/// score is attributable to the template and nothing else.
/// Returns the report AND the summary text, so a caller can inspect what the
/// model actually wrote. Scoring is verbatim substring matching, which cannot
/// tell "the model dropped this fact" from "the model reformatted it" — the
/// summary is the only way to settle that, so the harness must hand it back.
pub(crate) async fn run_hard_recall_eval_with(
    summarize: SummarizeFn,
    schema: super::compression::SummarySchema,
) -> (RecallReport, String) {
    let facts = hard_facts();
    let msgs = noisy_session(&facts);
    let (start, end) = compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
    let middle = &msgs[start..end];
    let prompt = super::compression::build_summary_prompt_with(
        middle,
        summary_budget(estimate_messages_tokens(middle)),
        None,
        None,
        schema,
    )
    .expect("recall fixture must not contain the reserved fence delimiter");
    let summary = summarize(prompt).await.unwrap_or_default();
    let report = score_recall(&summary, &facts);
    (report, summary)
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
        )
        .expect("fixture is clean");
        let report = score_recall(&prompt, &facts);
        assert!(
            report.all_survived(),
            "facts dropped before reaching the summarizer: {:?}",
            report.dropped
        );
    }

    /// The same guard as [`planted_facts_reach_the_summarizer`], for the hard
    /// fixture. This one matters MORE, not less: the noisy turns are long, and
    /// `serialize_turns_for_summary` truncates a non-user turn at 2000 chars.
    /// If a fact fell past that cut, the bake-off would be measuring the
    /// serializer and reporting it as a schema difference.
    #[test]
    fn planted_hard_facts_reach_the_summarizer() {
        let facts = hard_facts();
        let msgs = noisy_session(&facts);
        let (start, end) =
            compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
        assert!(start < end, "session must produce a non-empty window");

        let middle = &msgs[start..end];
        let prompt = build_summary_prompt(
            middle,
            summary_budget(estimate_messages_tokens(middle)),
            None,
            None,
        )
        .expect("fixture is clean");
        let report = score_recall(&prompt, &facts);
        assert!(
            report.all_survived(),
            "facts dropped before reaching the summarizer: {:?}",
            report.dropped
        );
    }

    /// The hard fixture has to actually be hard: enough material that a
    /// summarizer must choose what to keep. If it shrinks to where everything
    /// fits comfortably, a null bake-off result would mean nothing.
    #[test]
    fn the_hard_fixture_forces_the_summarizer_to_choose() {
        let facts = hard_facts();
        assert!(
            facts.len() >= 20,
            "too few facts to force a choice: {}",
            facts.len()
        );
        let msgs = noisy_session(&facts);
        let (start, end) =
            compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
        let middle = &msgs[start..end];
        let material = estimate_messages_tokens(middle);
        let budget = summary_budget(material);
        // The real condition: the budget must be set by the RATIO, not clamped
        // up by `MIN_SUMMARY_TOKENS`. While the floor dominates, growing the
        // fixture does not tighten the squeeze, and "budget < material" stays
        // true while meaning nothing.
        assert!(
            budget > 2_000,
            "budget is still on the 2000-token floor (material {material} tokens) — \
             the fixture is not yet big enough for the ratio to bind"
        );
        assert!(
            budget * 4 < material,
            "budget {budget} vs material {material}: the summarizer is not being \
             asked to drop much"
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
        )
        .expect("fixture is clean");
        assert!(
            prompt.contains("stdlib conversion complete") && prompt.contains("no tokio remains"),
            "completion signal (original task DONE) must reach the summarizer prompt"
        );
        assert!(
            prompt.contains("integration test hangs") && prompt.contains("debugging the race"),
            "follow-up signal (live work) must reach the summarizer prompt"
        );
    }

    /// Real strings a live summarizer produced, which the raw `contains`
    /// scorer counted as losses. Every one of these preserves the fact.
    #[test]
    fn scorer_credits_a_fact_the_model_reformatted() {
        let facts = hard_facts();

        // Outer code span ate the inner backticks (deepseek, sections arm).
        let a = "- `called Option::unwrap() on a None value`";
        // Inner backticks escaped instead (deepseek, slots arm).
        let b = r"RISKS: `called \`Option::unwrap()\` on a \`None\` value`";
        // The model code-formatted a bare number.
        let c = "   - offset `883421`";
        // And a path wrapped mid-sentence.
        let d = "see `crates/ingest/src/backfill/checkpoint.rs` for the resume path";

        for (text, needle) in [
            (a, "called `Option::unwrap()` on a `None` value"),
            (b, "called `Option::unwrap()` on a `None` value"),
            (c, "offset 883421"),
            (d, "crates/ingest/src/backfill/checkpoint.rs"),
        ] {
            let f: Vec<&PlantedFact> = facts.iter().filter(|f| f.needle == needle).collect();
            assert_eq!(f.len(), 1, "needle not in the fact set: {needle}");
            let report = score_recall(
                text,
                &[PlantedFact {
                    kind: f[0].kind,
                    needle: f[0].needle,
                }],
            );
            assert!(
                report.all_survived(),
                "reformatted-but-faithful text scored as a loss\n  text:   {text}\n  needle: {needle}"
            );
        }
    }

    /// The other half, and the reason the normalisation is narrow: stripping
    /// presentation must NOT turn the scorer into one that credits anything.
    /// A paraphrase that keeps the meaning and loses the string is still a
    /// loss — that is the whole failure mode being measured.
    #[test]
    fn scorer_still_catches_a_paraphrase() {
        let facts = hard_facts();
        let paraphrased = "## Blocked\n\
            The worker hit an unwrap panic on a missing value, the connection \
            pool ran out after about half a minute, and a checksum did not \
            match. Config was adjusted (batch size, pool ceiling, backoff) and \
            the team ran the ingest tests plus a database query for the maximum \
            offset. A job on one shard is stuck partway through.";
        let report = score_recall(paraphrased, &facts);
        assert_eq!(
            report.survived,
            0,
            "a summary that keeps the meaning and drops every exact string must \
             score zero; survived {:?}",
            facts.len() - report.dropped.len()
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
