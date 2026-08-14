//! Compaction-schema bake-off (dirge-e31n.7).
//!
//! # What this measures, and why it is not the A/B harness
//!
//! `scripts/loop-ab.sh` measures whole runs against a live model. That is the
//! right instrument for "does this steering change how the agent behaves", and
//! it is expensive: the unresolved-effects round took 53 informative runs
//! across four model configs to conclude nothing.
//!
//! The compaction-schema question is narrower and does not need it. The claim
//! is that a labelled-slot schema preserves more load-bearing detail through a
//! fold than the current narrative sections do. That is a property of ONE model
//! call — transcript in, summary out — so it can be measured directly, with the
//! conversation held byte-identical across arms and the only difference being
//! the section template. A dozen calls settles it, and nothing about agent
//! behaviour, tool choice, or turn count is in the way.
//!
//! # Running it
//!
//! Off by default: it costs real model calls. Set `DIRGE_BAKEOFF=1`, plus:
//!
//!   DIRGE_BAKEOFF_PROVIDER   deepseek | glm | custom | ...  (default deepseek)
//!   DIRGE_BAKEOFF_MODEL      model id for that provider
//!   DIRGE_BAKEOFF_BASE_URL   for `custom` — e.g. a local llama.cpp server
//!   DIRGE_BAKEOFF_REPEATS    calls per arm (default 5)
//!
//!   cargo nextest run compaction_bakeoff --no-capture
//!
//! # Reading the result
//!
//! The score is verbatim recall of twenty planted facts, so it is a floor on
//! fidelity, not a grade: a summary can be excellent and still paraphrase a
//! path. What makes the comparison meaningful is that both arms face the same
//! transcript, the same budget, and the same scorer — the difference between
//! them is attributable to the template and nothing else.
//!
//! A null result here means the schema does not move recall, which is a real
//! answer and the one three other rounds of this epic came back with. Do not
//! reach for a bigger fixture to make it non-null.

use std::collections::HashMap;

use crate::agent::compression::{SummarizeFn, SummarySchema};

/// Build a `SummarizeFn` against a real provider from the env, or `None` when
/// the bake-off is not enabled or no credentials are present.
pub(crate) fn bakeoff_summarizer() -> Option<(SummarizeFn, String)> {
    if std::env::var("DIRGE_BAKEOFF").ok().as_deref() != Some("1") {
        return None;
    }
    let provider = std::env::var("DIRGE_BAKEOFF_PROVIDER").unwrap_or_else(|_| "deepseek".into());
    let model_name = std::env::var("DIRGE_BAKEOFF_MODEL").ok()?;

    // A `custom` provider needs an explicit base_url; http:// is refused
    // without allow_insecure, which a local server needs.
    let mut providers: HashMap<String, crate::config::ProviderEntry> = HashMap::new();
    if let Ok(base_url) = std::env::var("DIRGE_BAKEOFF_BASE_URL") {
        providers.insert(
            provider.clone(),
            crate::config::ProviderEntry {
                provider_type: Some("custom".into()),
                base_url: Some(base_url),
                api_key: Some("not-used".into()),
                allow_insecure: true,
                ..Default::default()
            },
        );
    }

    let client = match crate::provider::create_client(&provider, None, &providers) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[bakeoff] could not build {provider} client: {e}");
            return None;
        }
    };
    let model = client.completion_model(model_name.clone());
    let sfn: SummarizeFn = std::sync::Arc::new(move |prompt: String| {
        let m = model.clone();
        Box::pin(async move { crate::provider::summarize::summarize_with_model(m, prompt).await })
    });
    Some((sfn, format!("{provider}/{model_name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compaction_recall::{
        hard_facts, run_coverage_probe_with, run_hard_recall_eval_with, run_tool_call_probe_with,
    };

    fn schema_label(s: SummarySchema) -> &'static str {
        match s {
            SummarySchema::Sections => "sections",
            SummarySchema::Slots => "slots",
            SummarySchema::SectionsWithoutCoverage => "no-cov",
        }
    }

    fn repeats() -> usize {
        std::env::var("DIRGE_BAKEOFF_REPEATS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5)
    }

    /// The coverage probe (dirge-5zca): clip the assembled prompt the way the
    /// one-shot does, and see whether the summary admits it saw partial
    /// material.
    ///
    /// Separate from the recall bake-off because it asks a different question.
    /// Recall asks how much detail survives, and both schemas came back level.
    /// This asks whether the reader is TOLD the record is partial — and only
    /// one schema has anywhere to say so.
    #[tokio::test]
    async fn compaction_coverage_probe() {
        let Some((sfn, who)) = bakeoff_summarizer() else {
            eprintln!("[coverage] skipped (set DIRGE_BAKEOFF=1 and DIRGE_BAKEOFF_MODEL)");
            return;
        };
        let n = repeats();
        eprintln!("[coverage] model={who} repeats={n}");
        for arm in [
            SummarySchema::SectionsWithoutCoverage,
            SummarySchema::Sections,
            SummarySchema::Slots,
        ] {
            let mut declared = 0usize;
            let mut errors = 0usize;
            for i in 0..n {
                let (says_partial, summary) = match run_coverage_probe_with(sfn.clone(), arm).await
                {
                    Ok(v) => v,
                    Err(e) => {
                        errors += 1;
                        eprintln!(
                            "[coverage] {} run {}/{}: SUMMARIZER FAILED — {e}",
                            schema_label(arm),
                            i + 1,
                            n
                        );
                        continue;
                    }
                };
                if says_partial {
                    declared += 1;
                }
                eprintln!(
                    "[coverage] {} run {}/{}: declares-partial={}",
                    schema_label(arm),
                    i + 1,
                    n,
                    says_partial,
                );
                if std::env::var("DIRGE_BAKEOFF_DUMP").is_ok() {
                    let path = format!(
                        "{}coverage-{}-{}.md",
                        std::env::temp_dir().display(),
                        schema_label(arm),
                        i + 1
                    );
                    let _ = std::fs::write(&path, &summary);
                }
            }
            eprintln!(
                "[coverage] === {:<9} declared the gap in {}/{} runs ({} failed)",
                schema_label(arm),
                declared,
                n - errors,
                errors,
            );
        }
    }

    /// dirge-czg9: do facts that exist only in tool-call arguments survive a
    /// fold? Reports two numbers — what dirge's serializer handed the
    /// summarizer, and what came back — because a change that lifts the first
    /// without the second is prompt bloat, not fidelity.
    #[tokio::test]
    async fn compaction_tool_call_probe() {
        let Some((sfn, who)) = bakeoff_summarizer() else {
            eprintln!("[toolcall] skipped (set DIRGE_BAKEOFF=1 and DIRGE_BAKEOFF_MODEL)");
            return;
        };
        let n = repeats();
        let total = crate::agent::compaction_recall::tool_call_facts().len();
        eprintln!("[toolcall] model={who} repeats={n} facts={total}");
        for arm in [SummarySchema::Sections] {
            let (mut prompt_sum, mut summary_sum) = (0usize, 0usize);
            let mut errors = 0usize;
            for i in 0..n {
                let (in_prompt, in_summary, _) =
                    match run_tool_call_probe_with(sfn.clone(), arm).await {
                        Ok(v) => v,
                        Err(e) => {
                            errors += 1;
                            eprintln!(
                                "[toolcall] {} run {}/{}: SUMMARIZER FAILED — {e}",
                                schema_label(arm),
                                i + 1,
                                n
                            );
                            continue;
                        }
                    };
                prompt_sum += in_prompt.survived;
                summary_sum += in_summary.survived;
                eprintln!(
                    "[toolcall] {} run {}/{}: reached prompt {}/{}, kept in summary {}/{}",
                    schema_label(arm),
                    i + 1,
                    n,
                    in_prompt.survived,
                    total,
                    in_summary.survived,
                    total,
                );
            }
            eprintln!(
                "[toolcall] === {:<9} prompt {:.1}/{}  summary {:.1}/{}  ({} failed)",
                schema_label(arm),
                prompt_sum as f64 / (n - errors).max(1) as f64,
                total,
                summary_sum as f64 / (n - errors).max(1) as f64,
                total,
                errors,
            );
        }
    }

    /// The bake-off. Auto-skips unless `DIRGE_BAKEOFF=1`.
    #[tokio::test]
    async fn compaction_schema_bakeoff() {
        let Some((sfn, who)) = bakeoff_summarizer() else {
            eprintln!("[bakeoff] skipped (set DIRGE_BAKEOFF=1 and DIRGE_BAKEOFF_MODEL)");
            return;
        };
        let n = repeats();
        let total_facts = hard_facts().len();
        eprintln!("[bakeoff] model={who} repeats={n} facts={total_facts}");

        let mut rows: Vec<(SummarySchema, Vec<usize>)> = Vec::new();
        for arm in [
            SummarySchema::SectionsWithoutCoverage,
            SummarySchema::Sections,
            SummarySchema::Slots,
        ] {
            let mut scores = Vec::new();
            let mut errors = 0usize;
            for i in 0..n {
                let (report, summary) = match run_hard_recall_eval_with(sfn.clone(), arm).await {
                    Ok(v) => v,
                    Err(e) => {
                        // A failed CALL is not a lossy SUMMARY. Scoring it as
                        // 0/20 once made one arm read 17.4 against another's
                        // 19.9 for no reason but a transient provider error.
                        errors += 1;
                        eprintln!(
                            "[bakeoff] {} run {}/{}: SUMMARIZER FAILED — {e}",
                            schema_label(arm),
                            i + 1,
                            n
                        );
                        continue;
                    }
                };
                // Print the NEEDLE, not just its kind. The first run had the
                // sections arm dropping "error message" four times out of
                // five, which reads like signal — but one of the three error
                // needles contains backticks, and a model reformatting
                // markdown would fail a verbatim match while being perfectly
                // faithful. Which string it is decides whether that is a
                // fidelity difference or a scoring artifact.
                eprintln!(
                    "[bakeoff] {} run {}/{}: {}/{} — dropped {:?}",
                    schema_label(arm),
                    i + 1,
                    n,
                    report.survived,
                    report.total,
                    report
                        .dropped
                        .iter()
                        .map(|(_, ndl)| *ndl)
                        .collect::<Vec<_>>(),
                );
                if std::env::var("DIRGE_BAKEOFF_DUMP").is_ok() {
                    let path = format!(
                        "{}bakeoff-{}-{}.md",
                        std::env::temp_dir().display(),
                        schema_label(arm),
                        i + 1
                    );
                    let _ = std::fs::write(&path, &summary);
                    eprintln!("[bakeoff]   summary -> {path}");
                }
                scores.push(report.survived);
            }
            if errors > 0 {
                eprintln!(
                    "[bakeoff] {} — {errors} run(s) excluded: the summarizer failed",
                    schema_label(arm)
                );
            }
            rows.push((arm, scores));
        }

        eprintln!("\n[bakeoff] === {who}, {n} runs per arm, {total_facts} facts ===");
        for (arm, scores) in &rows {
            let sum: usize = scores.iter().sum();
            let mean = sum as f64 / scores.len() as f64;
            let lo = scores.iter().min().copied().unwrap_or(0);
            let hi = scores.iter().max().copied().unwrap_or(0);
            // The mean hides the thing that matters. At n=8 both arms averaged
            // ~19/20, but the sections arm got there with two lossy runs (18
            // and 15) among six perfect ones, while the slots arm never dropped
            // more than one fact. For compaction the occasional run that loses
            // five facts IS the failure mode — a mean of 19 describes neither
            // arm's behaviour. So report the tail: total facts lost, and how
            // many runs were bad.
            let lost: usize = scores.iter().map(|s| total_facts - s).sum();
            let bad = scores.iter().filter(|s| **s + 1 < total_facts).count();
            eprintln!(
                "[bakeoff] {:<9} mean {:.1}/{} ({:.0}%)  range {}..{}  lost {}/{}  runs losing 2+: {}/{}  {:?}",
                schema_label(*arm),
                mean,
                total_facts,
                100.0 * mean / total_facts as f64,
                lo,
                hi,
                lost,
                total_facts * scores.len(),
                bad,
                scores.len(),
                scores,
            );
        }
    }
}
