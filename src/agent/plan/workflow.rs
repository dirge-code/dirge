//! Phased plan workflow (Phase 3): the phase prompts, the tool allow-lists, the
//! machine-parsed reviewer verdict, and the per-step review policy
//! ([`next_review_step`]). Ported from vix (`plan_workflow/*`,
//! `implement_and_review/*`, `agents/reviewer.md`) and adapted to dirge's tool
//! names. This module is pure logic (no runtime); the `/plan` command wires it
//! up — `ui/slash/cmd_plan.rs` runs the explore→plan forks with these prompts
//! via [`crate::provider::AnyAgent::spawn_phase_runner`], and the reviewer loop
//! in `ui/run_handlers` drives [`next_review_step`] after each implement turn.
//! The runtime glue (runner drain, reviewer fork) lives in
//! [`crate::agent::plan::runtime`].

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

/// Substitute `{{KEY}}` placeholders in ONE left-to-right pass. A sequence of
/// `.replace()` calls is unsafe here because the inputs are user-controlled: if
/// the `request` contains the literal `{{FINDINGS}}`, a later
/// `.replace("{{FINDINGS}}", …)` would clobber the user's own text. This pass
/// never re-scans substituted values. Unknown / unclosed `{{…}}` are emitted
/// verbatim.
fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        match after.find("}}") {
            Some(close) => {
                let key = &after[..close];
                match vars.iter().find(|(k, _)| *k == key) {
                    Some((_, val)) => out.push_str(val),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                rest = &after[close + 2..];
            }
            None => {
                // Unclosed `{{` — emit the marker + remainder verbatim.
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// System prompt for the **explore** phase fork. `request` is the user's task.
pub fn explore_prompt(request: &str) -> String {
    render_template(EXPLORE_TEMPLATE, &[("REQUEST", request)])
}

/// System prompt for the **plan** phase fork. `findings` is the explore phase's
/// structured report (handed off via the fork).
pub fn plan_prompt(request: &str, findings: &str) -> String {
    render_template(
        PLAN_TEMPLATE,
        &[("REQUEST", request), ("FINDINGS", findings)],
    )
}

/// System prompt for the **reviewer** fork (P3d): run-the-code, asymmetric
/// `NEEDS_FIX`, machine-parsed JSON verdict.
pub fn reviewer_prompt(task: &str) -> String {
    render_template(REVIEWER_TEMPLATE, &[("TASK", task)])
}

/// Follow-up prompt fed to the implementer on a `NEEDS_FIX` verdict.
pub fn implement_retry_prompt(feedback: &str) -> String {
    render_template(IMPLEMENT_RETRY_TEMPLATE, &[("FEEDBACK", feedback)])
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

/// One step of the reviewer-runs-code policy. The interactive `/plan` command
/// can't block on its streamed implement run, so the live reviewer loop in
/// `ui/run_handlers` drives the policy event-by-event through this single
/// function. Given the reviewer's raw output and how many fix cycles remain,
/// decide what happens next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStep {
    /// Reviewer confirmed `DONE`.
    Approved,
    /// Not done and a cycle remains: feed `feedback` (the punch-list, or the
    /// raw review when the verdict was unparseable) to the implementer.
    Retry { feedback: String },
    /// Not done and no cycles remain.
    Exhausted,
}

/// Decide the next move from a reviewer's output. Asymmetric-caution bias (from
/// vix): anything that isn't a parseable `DONE` is treated as not-done, so an
/// ambiguous or malformed review keeps the loop going rather than shipping.
pub fn next_review_step(review_text: &str, cycles_left: usize) -> ReviewStep {
    let verdict = parse_review_verdict(review_text);
    if matches!(&verdict, Some(v) if v.verdict == Verdict::Done) {
        return ReviewStep::Approved;
    }
    if cycles_left == 0 {
        return ReviewStep::Exhausted;
    }
    let feedback = verdict
        .map(|v| v.missing)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| review_text.to_string());
    ReviewStep::Retry { feedback }
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

    fn done_review() -> String {
        "looks complete\n```json\n{\"verdict\":\"DONE\",\"missing\":\"\"}\n```".to_string()
    }
    fn needs_fix_review(missing: &str) -> String {
        format!("review\n```json\n{{\"verdict\":\"NEEDS_FIX\",\"missing\":\"{missing}\"}}\n```")
    }

    #[test]
    fn next_review_step_policy() {
        // DONE → Approved regardless of remaining cycles.
        assert_eq!(next_review_step(&done_review(), 0), ReviewStep::Approved);
        assert_eq!(next_review_step(&done_review(), 3), ReviewStep::Approved);

        // NEEDS_FIX with budget → Retry carrying the punch-list.
        assert_eq!(
            next_review_step(&needs_fix_review("- add tests"), 2),
            ReviewStep::Retry {
                feedback: "- add tests".to_string()
            }
        );
        // NEEDS_FIX with no budget → Exhausted.
        assert_eq!(
            next_review_step(&needs_fix_review("- add tests"), 0),
            ReviewStep::Exhausted
        );

        // Unparseable never approves: with budget the raw text is the feedback,
        // with none it exhausts.
        match next_review_step("no json here", 1) {
            ReviewStep::Retry { feedback } => assert!(feedback.contains("no json here")),
            other => panic!("expected Retry, got {other:?}"),
        }
        assert_eq!(next_review_step("no json here", 0), ReviewStep::Exhausted);
    }

    #[test]
    fn plan_prompt_substitution_is_injection_safe() {
        // A request that contains a literal `{{FINDINGS}}` must NOT be clobbered
        // by the findings substitution — single-pass render never re-scans
        // substituted text.
        let p = plan_prompt("add caching, then {{FINDINGS}}", "core.rs:42 is the map");
        assert!(
            p.contains("add caching, then {{FINDINGS}}"),
            "user text preserved verbatim, not treated as a placeholder",
        );
        assert!(
            p.contains("core.rs:42 is the map"),
            "real findings still substituted at the template's placeholder",
        );
    }

    #[test]
    fn render_template_leaves_unknown_placeholders_verbatim() {
        assert_eq!(
            render_template("a {{X}} b {{Y}}", &[("X", "1")]),
            "a 1 b {{Y}}",
        );
        // Unclosed marker is emitted verbatim, not dropped.
        assert_eq!(render_template("a {{X", &[("X", "1")]), "a {{X");
    }
}
