//! Phased plan workflow (Phase 3): the phase prompts + the machine-parsed
//! reviewer verdict. Ported from vix (`plan_workflow/*`, `implement_and_review/*`,
//! `agents/reviewer.md`) and adapted to dirge's tool names. The orchestration
//! (P3c, `dirge-rjmm`) and reviewer-runs-code loop (P3d, `dirge-rori`) fork
//! agents via [`crate::provider::AnyAgent::spawn_phase_runner`] with these
//! prompts + the matching tool allow-lists, and parse the reviewer's verdict
//! with [`parse_review_verdict`].

// Wired by the orchestrator (P3c) / reviewer loop (P3d); exercised by tests now.
#![allow(dead_code)]

/// Read-only tool allow-list for the explore + plan phases (no mutation).
pub const READONLY_PHASE_TOOLS: &[&str] = &[
    "read",
    "read_minified",
    "grep",
    "glob",
    "find_files",
    "list_dir",
    "lsp",
    "repo_overview",
    "list_symbols",
    "get_symbol_body",
    "find_definition",
    "find_callers",
    "find_callees",
];

/// Reviewer tool allow-list: read-only navigation PLUS `bash` so it can run the
/// code to gather first-hand evidence — but NO `write`/`edit`/`apply_patch`
/// (the reviewer cannot fix anything, only judge).
pub const REVIEWER_TOOLS: &[&str] = &[
    "read",
    "read_minified",
    "grep",
    "glob",
    "find_files",
    "list_dir",
    "lsp",
    "bash",
];

const EXPLORE_TEMPLATE: &str = "\
You are dirge in the **Explore** phase. Set aside any goals, plans, or assumptions \
from other phases — they no longer apply. Your ONLY objective is to build a \
thorough understanding of the codebase as grounding for the plan that follows. \
Do NOT write or modify any code, and do NOT produce a plan.

## User request

{{REQUEST}}

## Exploration discipline

**Minimize tool calls.** Every `read`, `grep`, `glob`, `list_dir`, or `lsp` call \
should answer a specific, targeted question. Only reach for source files when a \
specific question is otherwise unanswerable.

Legitimate reasons to use a tool:
- Inspecting a signature or implementation you intend to reference in the plan
- Verifying a utility/pattern you plan to rely on actually exists as described
- Resolving an ambiguity about how two components interact
- Confirming a file path exists before referencing it

Not legitimate: general orientation, re-reading anything already in context, or \
exploring to rediscover structure you already know. **Never call the same tool on \
the same file twice.** Be surgical.

## Output

Once exploration is complete, respond with a concise structured report of what you \
found relevant to the request — the files, functions, patterns, constraints, and \
reusable utilities that matter, with `path:line` references. No preamble, no \
markdown fences. This report is the ONLY thing passed to the Plan phase.";

const PLAN_TEMPLATE: &str = "\
You are dirge in the **Plan** phase. You have the exploration findings below; set \
aside the exploration mechanics. Produce a structured implementation plan for the \
user request. Do NOT write or modify any code.

## User request

{{REQUEST}}

## Exploration findings

{{FINDINGS}}

## Plan format

### Name
Short, specific label. 2-5 words. Not a sentence.

### Context
**Why** this change is needed — what problem it solves, what breaks/degrades \
without it. Explain motivation, not what the code will do.

### Architecture
Structural/design-level changes only (omit if purely self-contained): new \
abstractions, interfaces changed, data flow affected, new dependencies. For each \
decision, briefly state **why** that approach.

### Files
Exhaustive list of every file that will be **created** or **modified**. No \
directories, no read-only files. Verify uncertain paths with a tool before listing.

### Steps
Ordered implementation steps. Each step must:
- Name **specific identifiers**: file path, function/method, type, interface
- Call out **existing utilities to reuse** rather than reimplementing
- **Flag risky steps** inline (e.g. \"⚠ changes a shared interface — all callers \
must be updated in later steps\")
- End with a **final Verify step** giving the exact build and test commands that \
confirm the whole change

**Step quality bar:** specific enough to execute without ambiguity but not \
dictating variable names; one coherent unit of work per step; ordered so no step \
depends on a later step's output; nothing beyond what the request asks.

**Anti-patterns:** vague verbs (*update/handle/improve* — use *add/replace/\
extract/delete/rename*); referencing code that may not exist; unrequested \
refactoring or speculative improvements.

## Output

Write the plan in full. Then, before finalising, review it against these questions:
- Does every step reference real, verified identifiers — no invented paths/names?
- Is every step ordered so no step depends on the output of a later step?
- Do any steps bundle unrelated changes?
- Any vague verbs that should be made specific?
- Does the Files list match exactly what the steps touch — nothing missing/extra?
- Does the final Verify step include exact commands?

If any answer reveals a problem, silently fix the plan, then output the final, \
corrected plan.";

const REVIEWER_TEMPLATE: &str = "\
You are dirge running as the **reviewer**. You are reviewing another agent's \
attempt at the task below — you are NOT the implementer. **Your write, edit, and \
delete tools are denied by design; you cannot fix anything.** Your job is to decide \
whether the task is actually complete, based on real evidence you gather yourself.

## Task

{{TASK}}

## How to review

Answer four questions, in order:
1. **What was requested** — restate the task concretely (deliverables, paths, \
formats, acceptance criteria).
2. **What was actually done** — inspect the filesystem and diffs with `glob`, \
`read`, `grep`, and `bash` (`git status`/`git diff`/`ls`). Don't trust the \
implementer's narrative.
3. **What evidence exists that it worked** — actually run the code. Compile it, \
execute it on an example, compare output to what the task demands. Cite the exact \
commands and their outputs.
4. **What is still missing** — gaps, mismatches, unverified claims. Be specific. If \
nothing is missing, say so and say *why*.

Your `bash`/`read`/`grep`/`glob`/`lsp` tools exist so you can gather real evidence. \
**Use them.** A review that only trusts the transcript is a rubber stamp.

## Verdict rules

- `DONE` — every concrete requirement is satisfied AND you have direct, first-hand \
evidence for each one.
- `NEEDS_FIX` — anything is missing, broken, or unverifiable. **If evidence is \
ambiguous, default to `NEEDS_FIX`.** A false `DONE` ships a broken result; a false \
`NEEDS_FIX` only costs one retry.

## Output format

After your narrative review, emit **exactly one** fenced JSON block as the LAST \
element of your response (anything after it, or a malformed block, breaks the loop):

```json
{
  \"verdict\": \"DONE\" | \"NEEDS_FIX\",
  \"checklist\": \"1. **Requested:** ...\\n2. **Done:** ...\\n3. **Evidence:** ...\\n4. **Missing:** ...\",
  \"missing\": \"- gap 1\\n- gap 2\"
}
```
`verdict` is the literal `DONE` or `NEEDS_FIX`. `checklist` is the full four-section \
review as one string. `missing` is a bulleted string of gaps (empty when `DONE`).";

const IMPLEMENT_RETRY_TEMPLATE: &str = "\
The reviewer inspected your previous attempt and reported gaps. Your full prior \
conversation — the task, every file you wrote, every command you ran — is still in \
your context.

## Reviewer feedback

{{FEEDBACK}}

## What to do

1. Read the reviewer's `missing` list — that is the authoritative punch list.
2. Diagnose each gap: a real mismatch, or the reviewer misread the state? Either \
way address it (for a misread, produce clearer evidence).
3. Make the **smallest** changes that close every gap. Do not rewrite the whole \
solution unless the underlying approach is actually wrong.
4. Re-run your own check with the changes applied; confirm each gap is closed.
5. Stop. The reviewer runs again with fresh feedback if gaps remain.

Do not argue with the review in prose — just fix the gaps.";

/// System prompt for the **explore** phase fork. `request` is the user's task.
pub fn explore_prompt(request: &str) -> String {
    EXPLORE_TEMPLATE.replace("{{REQUEST}}", request)
}

/// System prompt for the **plan** phase fork. `findings` is the explore phase's
/// structured report (handed off via the fork).
pub fn plan_prompt(request: &str, findings: &str) -> String {
    PLAN_TEMPLATE
        .replace("{{REQUEST}}", request)
        .replace("{{FINDINGS}}", findings)
}

/// System prompt for the **reviewer** fork (P3d): run-the-code, asymmetric
/// `NEEDS_FIX`, machine-parsed JSON verdict.
pub fn reviewer_prompt(task: &str) -> String {
    REVIEWER_TEMPLATE.replace("{{TASK}}", task)
}

/// Follow-up prompt fed to the implementer on a `NEEDS_FIX` verdict.
pub fn implement_retry_prompt(feedback: &str) -> String {
    IMPLEMENT_RETRY_TEMPLATE.replace("{{FEEDBACK}}", feedback)
}

/// The reviewer's machine-parsed verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Done,
    NeedsFix,
}

/// Parsed reviewer verdict (the fenced JSON block at the end of a review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    pub verdict: Verdict,
    pub checklist: String,
    pub missing: String,
}

/// Parse the reviewer's verdict from its response. Extracts the LAST fenced
/// ```json block (the reviewer is instructed to make it the final element) and
/// parses it. Returns `None` when no parseable block is found or the verdict
/// string is neither `DONE` nor `NEEDS_FIX`.
///
/// Safety bias mirrors vix: a verdict that can't be parsed is NOT treated as
/// `DONE` by callers — `None` means "couldn't confirm done", so the loop should
/// keep going rather than ship.
pub fn parse_review_verdict(text: &str) -> Option<ReviewVerdict> {
    let json = last_json_block(text)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let verdict = match v.get("verdict").and_then(|x| x.as_str())? {
        "DONE" => Verdict::Done,
        "NEEDS_FIX" => Verdict::NeedsFix,
        _ => return None,
    };
    Some(ReviewVerdict {
        verdict,
        checklist: v
            .get("checklist")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        missing: v
            .get("missing")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Extract the body of the LAST ```json … ``` fenced block in `text`.
fn last_json_block(text: &str) -> Option<String> {
    let open = text.rfind("```json")?;
    let after = &text[open + "```json".len()..];
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

/// Final output of a phase fork: the assistant's final text, or an error.
pub type PhaseOutput = Result<String, String>;

/// Orchestrate the **explore → plan** phases. Each phase is run by `run_phase`,
/// which the runtime (P3e) implements by forking a *fresh* agent via
/// [`crate::provider::AnyAgent::spawn_phase_runner`] with the given system
/// prompt + tool allow-list — a genuine context reset per phase. The explore
/// phase's structured report is handed into the plan phase's prompt (the only
/// thing carried across the reset). Returns the plan text for the review gate,
/// or an error if a phase failed or explore produced nothing.
///
/// Parameterized by the runner closure so the orchestration is unit-testable
/// without a real agent/runtime.
pub async fn run_explore_plan<R, Fut>(request: &str, run_phase: R) -> PhaseOutput
where
    R: Fn(String, &'static [&'static str]) -> Fut,
    Fut: std::future::Future<Output = PhaseOutput>,
{
    let report = run_phase(explore_prompt(request), READONLY_PHASE_TOOLS).await?;
    if report.trim().is_empty() {
        return Err("explore phase produced no findings".to_string());
    }
    run_phase(plan_prompt(request, &report), READONLY_PHASE_TOOLS).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_embed_inputs_and_key_directives() {
        let p = explore_prompt("Add an LRU cache");
        assert!(p.contains("Add an LRU cache"));
        assert!(p.contains("**Explore**") && p.contains("Minimize tool calls"));
        assert!(p.contains("do NOT produce a plan") || p.contains("not produce a plan"));

        let p = plan_prompt("Add an LRU cache", "core.rs:42 has the cache map");
        assert!(p.contains("Add an LRU cache") && p.contains("core.rs:42"));
        assert!(p.contains("final Verify step") && p.contains("Anti-patterns"));

        let p = reviewer_prompt("Add an LRU cache");
        assert!(p.contains("Add an LRU cache"));
        assert!(p.contains("default to `NEEDS_FIX`") && p.contains("denied by design"));

        let p = implement_retry_prompt("- cache eviction not tested");
        assert!(p.contains("cache eviction not tested") && p.contains("smallest"));
    }

    #[test]
    fn parses_done_verdict() {
        let resp = "Narrative review here...\n\n```json\n{\"verdict\": \"DONE\", \"checklist\": \"all good\", \"missing\": \"\"}\n```";
        let v = parse_review_verdict(resp).expect("parses");
        assert_eq!(v.verdict, Verdict::Done);
        assert_eq!(v.missing, "");
    }

    #[test]
    fn parses_needs_fix_with_punch_list() {
        let resp = "review...\n```json\n{\"verdict\":\"NEEDS_FIX\",\"checklist\":\"c\",\"missing\":\"- no tests\\n- panics on empty\"}\n```\n";
        let v = parse_review_verdict(resp).expect("parses");
        assert_eq!(v.verdict, Verdict::NeedsFix);
        assert!(v.missing.contains("no tests") && v.missing.contains("panics"));
    }

    #[test]
    fn takes_the_last_json_block() {
        // An earlier JSON sample (e.g. the model echoing the format) must not
        // shadow the real verdict at the end.
        let resp = "```json\n{\"verdict\":\"DONE\"}\n```\nactually wait, re-reviewing...\n```json\n{\"verdict\":\"NEEDS_FIX\",\"missing\":\"- x\"}\n```";
        assert_eq!(
            parse_review_verdict(resp).unwrap().verdict,
            Verdict::NeedsFix
        );
    }

    #[test]
    fn unparseable_is_none_not_done() {
        assert!(parse_review_verdict("no json here").is_none());
        assert!(parse_review_verdict("```json\n{not valid json}\n```").is_none());
        // Unknown verdict value → None (caller must not treat as DONE).
        assert!(parse_review_verdict("```json\n{\"verdict\":\"MAYBE\"}\n```").is_none());
    }

    use std::sync::{Arc, Mutex};

    /// The orchestrator runs explore then plan, gives each the right prompt +
    /// read-only tool allow-list, and hands the explore report into the plan.
    #[tokio::test]
    async fn orchestrates_explore_then_plan_with_handoff() {
        let calls: Arc<Mutex<Vec<(String, Vec<&'static str>)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let run_phase = move |prompt: String, tools: &'static [&'static str]| {
            let calls = calls2.clone();
            async move {
                let n = {
                    let mut c = calls.lock().unwrap();
                    c.push((prompt, tools.to_vec()));
                    c.len()
                };
                Ok(if n == 1 {
                    "core.rs:42 holds the cache map".to_string() // explore report
                } else {
                    "### Name\nLRU cache\n### Steps\n...".to_string() // plan
                })
            }
        };

        let plan = run_explore_plan("Add an LRU cache", run_phase)
            .await
            .expect("orchestration succeeds");
        assert!(plan.contains("LRU cache"));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "explore then plan");
        // Explore: explore prompt + read-only tools (no write).
        assert!(calls[0].0.contains("**Explore**") && calls[0].0.contains("Add an LRU cache"));
        assert!(calls[0].1.contains(&"read") && !calls[0].1.contains(&"write"));
        // Plan: plan prompt WITH the explore report handed off.
        assert!(
            calls[1].0.contains("**Plan**")
                && calls[1].0.contains("core.rs:42 holds the cache map")
        );
        assert!(calls[1].1.contains(&"read") && !calls[1].1.contains(&"edit"));
    }

    #[tokio::test]
    async fn explore_failure_aborts_before_plan() {
        let count = Arc::new(Mutex::new(0));
        let count2 = count.clone();
        let run_phase = move |_p: String, _t: &'static [&'static str]| {
            let count = count2.clone();
            async move {
                *count.lock().unwrap() += 1;
                Err::<String, String>("explore failed".to_string())
            }
        };
        assert!(run_explore_plan("x", run_phase).await.is_err());
        assert_eq!(
            *count.lock().unwrap(),
            1,
            "plan phase must not run after explore fails"
        );
    }

    #[tokio::test]
    async fn empty_explore_report_is_rejected_before_plan() {
        let count = Arc::new(Mutex::new(0));
        let count2 = count.clone();
        let run_phase = move |_p: String, _t: &'static [&'static str]| {
            let count = count2.clone();
            async move {
                *count.lock().unwrap() += 1;
                Ok::<String, String>("   ".to_string())
            }
        };
        assert!(run_explore_plan("x", run_phase).await.is_err());
        assert_eq!(
            *count.lock().unwrap(),
            1,
            "empty explore report → no plan phase"
        );
    }
}
