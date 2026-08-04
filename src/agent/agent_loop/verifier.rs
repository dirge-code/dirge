//! Pre-finalization verifier gate (F6).
//!
//! Backs the "verify before done" discipline with a *mechanism*, not just
//! prose. It watches the run for two things — did the agent edit a CODE
//! file, and did it run a build/test command and did that **pass or
//! fail** — and at the finalization boundary injects one soft nudge when
//! the work looks unverified or broken:
//!
//!   - edited code + a build/test command **failed**  → "fix the red build"
//!   - edited code + **no** build/test command ran    → "verify it works"
//!   - edited code + a build/test command **passed**  → silent (confident)
//!
//! Tiered verification (dirge-uw2l.2, RAX fidelity pyramid): when
//! `LoopConfig.verification_tiers_mode` engages, each verification
//! command is classified Fast (typecheck/lint/single named test — the
//! cheap tier that runs during integration) or Slow (full suite/full
//! build — run once, at the boundary), mirroring the DS1 Remote Agent
//! split that put hundreds of failure contexts on the cheap testbeds and
//! only nominal scenarios on flight hardware. Two signals change:
//!
//!   - mid-run: enough code edits pile up with no verification since →
//!     one nudge to run a FAST check now (front-line testing during
//!     integration, not deferred to the end)
//!   - finalization: fast-green but slow never ran → escalate for the
//!     full suite (advisory: once; blocking: up to MAX_TIER_ESCALATIONS)
//!
//! Per-run message ceiling: `off` 1 (the legacy one-shot), `advisory` 2
//! (legacy nudge + one escalation), `blocking` 3 (legacy nudge + two
//! escalations). The legacy nudge and the escalation have SEPARATE
//! budgets because they answer different questions — "did anything run?"
//! vs "did the full suite run?" — and the states are mutually exclusive
//! at any instant, so the run hears each as it crosses into it. With
//! `off` every tiered path is unreachable: behavior is byte-identical to
//! the untiered gate.
//! Project gate (dirge-w2de): `LoopConfig.verifier` may be built with a
//! `verification_command` — the build/test command CI actually runs
//! (e.g. `RUSTFLAGS="-D warnings" cargo clippy --all-targets`). A green
//! weaker command is a FALSE green: it says nothing about the tree CI
//! builds. So when a project gate is configured, `status()` only reports
//! [`VerificationStatus::VerifiedGreen`] after a command with the same
//! (program, subcommand) signature passed — matching is signature-based,
//! not string-identical, so env prefixes and flag placement don't matter.
//! Until then an otherwise-green result reports FastGreenOnly and the
//! existing full-suite escalation carries it at finalization. Configuring
//! the gate is an explicit opt-in: it may change off-mode behaviour for
//! that user, while users who never set the key keep byte-identical
//! behaviour (off mode never returns FastGreenOnly).
//!
//! Cheap and signal-based: no extra LLM call. Outcome is read from the
//! tool result post-execution (bash appends `Exit code: N` on non-zero
//! exit), so a failing test/build is detected without parsing semantics.
//! The legacy red/unverified nudge is bounded to fire at most once per
//! run (can't loop). Self-contained;
//! lives behind `LoopConfig.verifier` (None = off, byte-identical).

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::{Arc, Mutex};

use super::message::{LoopMessage, UserMessage};
use super::result::LoopToolResult;
use super::types::GateMode;

/// A read-only snapshot of what the run did toward verifying its code
/// changes, derived from the same signals that drive the cheap nudge.
/// Fed to the LLM critic so it can be pickier about compile/lint/test
/// without re-deriving the signal (dirge-6q3w). The `edited_code`
/// precondition is baked in: [`VerificationStatus::NoCodeEdited`] means
/// "verification not applicable this run" so the critic adds no pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    /// No code file was edited this run — verification is N/A.
    NoCodeEdited,
    /// Code was edited and the most recent build/test command passed.
    VerifiedGreen,
    /// Code was edited and the most recent build/test command failed.
    VerifiedRed,
    /// Code was edited but no build/test/lint command was detected.
    Unverified,
    /// Code was edited, fast-tier checks (typecheck/lint/targeted test)
    /// passed, but the slow tier (full suite/full build) never ran.
    /// Only reachable when verification tiers are engaged
    /// (`verification_tiers` ≠ off) or when a project gate is configured
    /// (`verification_command`, dirge-w2de): off-mode `status()` never
    /// returns it for users who did not opt in — fast-only coverage
    /// collapses to [`VerificationStatus::VerifiedGreen`] (dirge-uw2l.2).
    /// The project gate is an explicit opt-in, so it may return here in
    /// off mode for that user — intended.
    FastGreenOnly,
}

/// Display tag prefixing both verifier nudges. The UI keys on this to attribute
/// the message to the system/critic rather than the user (it's injected as a
/// user-role message so the model responds) [dirge-i75f]. The `*_NUDGE`
/// constants below embed it literally.
pub const VERIFY_TAG: &str = "[verify-before-done]";

/// Nudge when code was edited but no build/test command ran.
const VERIFY_NUDGE: &str = "[verify-before-done] You changed code this run but didn't run the tests or build to check it. Verify it works before reporting done — or, if there's nothing to run or you verified another way, say so briefly and finish. Don't re-edit just to look busy.";

/// Nudge when a build/test command failed after a code change.
const FAILED_NUDGE: &str = "[verify-before-done] Your last build or test command failed after you changed code. Don't report done on a red build — fix the failure. If it's pre-existing or expected, say so explicitly before finishing.";

/// Escalation when fast-tier checks passed but the full suite never ran
/// (dirge-uw2l.2). Only reachable with tiers engaged; carries the same
/// calibrated escape hatch as the legacy nudges.
const FULL_SUITE_NUDGE: &str = "[verify-before-done] Fast checks passed but the full test suite never ran this run. Run it once before reporting done — or, if there is no broader suite or you verified end-to-end another way, say so briefly and finish.";

/// Cap on full-suite escalations in `blocking` mode (advisory fires
/// once). Mirrors `MAX_OPEN_ISSUES_NUDGES` — bounded repeat, never a loop.
const MAX_TIER_ESCALATIONS: u8 = 2;

/// Which fidelity tier a verification command belongs to (dirge-uw2l.2).
///
/// `Ord` is meaningful: `Fast < Slow`, so folding a command chain with
/// `max()` yields its strongest segment — if the full suite ran anywhere
/// in `cargo check && cargo test`, the run is slow-covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerificationTier {
    /// Typecheck, lint, or a single targeted test — cheap enough to run
    /// during integration, repeatedly.
    Fast,
    /// Full suite or full build — run once, at the boundary.
    Slow,
}

/// Cross-boundary handoff of a run's final [`VerificationStatus`]
/// (dirge-1elu.7).
///
/// # Why a process-global, and why keyed by session
///
/// The status is computed inside `run_loop` from a [`VerifierGate`] the loop
/// owns; the consumer is the post-session pass in `ui::run_handlers::done`,
/// which runs on the UI side off an `AgentEvent`. Nothing carries a value
/// between those two points today, and the obvious routes all cost more than
/// the signal is worth:
///
/// - returning it from `run_agent_loop` does not reach `handle_done` at all,
///   which fires on the EVENT, not the return;
/// - carrying it on `AgentEnd` -> `AgentEvent::Done` puts a verifier-internal
///   type into the UI/stream event contract and touches ~15 construction sites
///   across subagents, plan runtime, and background review, none of which have
///   anything to do with verification;
/// - "persist it on the session" does not avoid the problem, it relocates it:
///   the session is owned by the UI, so the value still has to travel first.
///
/// So this is the same shape as [`crate::agent::tools::modified`] and
/// `crate::agent::tools::snapshots`, which are process-globals for the same
/// structural reason. The difference is the key: those are single-slot, and a
/// single slot here would let a subagent or MCP-delegate run, which share the
/// process, hand its status to the wrong session. Keying by session id makes a
/// wrong-session read impossible by construction rather than by convention.
///
/// Staleness is handled by taking on read AND recording unconditionally
/// (including `None`): a run that verified nothing clears its entry rather
/// than letting an earlier green be re-read.
mod run_status {
    use super::VerificationStatus;
    use crate::sync_util::LockExt;
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    /// Bound on retained entries. Only a session that never reads its own
    /// status leaks one, so this is a backstop, not a working limit.
    const MAX_RETAINED: usize = 64;

    static LAST_RUN: LazyLock<Mutex<HashMap<String, VerificationStatus>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Record the status a run finished with. Called once per run from
    /// `finish_tally`. Recording `None` REMOVES any prior entry: a run that
    /// ran no verification must not leave an earlier run's green behind.
    pub fn record(session_id: Option<&str>, status: Option<VerificationStatus>) {
        let Some(id) = session_id else {
            return;
        };
        let mut map = LAST_RUN.lock_ignore_poison();
        match status {
            Some(s) => {
                if map.len() >= MAX_RETAINED && !map.contains_key(id) {
                    map.clear();
                }
                map.insert(id.to_string(), s);
            }
            None => {
                map.remove(id);
            }
        }
    }

    /// Take this session's status, clearing it. Consuming, so a later run that
    /// recorded nothing cannot re-read a stale value.
    pub fn take(session_id: &str) -> Option<VerificationStatus> {
        LAST_RUN.lock_ignore_poison().remove(session_id)
    }
}

pub use run_status::{record as record_run_verification, take as take_run_verification};

/// Per-run verifier gate. See module docs.
#[derive(Debug)]
pub struct VerifierGate {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// A mutating file tool touched a code-extension path this run.
    edited_code: bool,
    /// A build/test command ran this run (any of them).
    ran_verification: bool,
    /// Outcome of the MOST RECENT build/test command (latest wins, so a
    /// fix-then-rerun-green sequence clears an earlier failure).
    verification_failed: bool,
    /// A nudge has already fired — never fire again (bounds the loop).
    fired: bool,
    /// A fast-tier command has PASSED this run. Tier flags record green
    /// *coverage*, not mere invocation: a red `cargo test` followed by a
    /// green `cargo check` must still read as fast-green-only, so the
    /// escalation asks for the suite the run never actually saw pass.
    /// (`ran_verification` keeps its invocation semantics — off-mode
    /// behaviour depends on it.)
    ran_fast: bool,
    /// A slow-tier command has PASSED this run. See [`Inner::ran_fast`].
    ran_slow: bool,
    /// Code edits since the last verification command of any tier. Drives
    /// the mid-run "run a fast check now" nudge.
    edits_since_verify: u32,
    /// Full-suite escalations already spent. Separate budget from
    /// [`Inner::fired`] — see the module docs.
    escalations: u8,
    /// Build/test commands this project's CI runs, resolved once at
    /// construction. Empty when there is no CI or nothing recognizable.
    /// Advisory text only — never consulted for a verdict.
    ci_commands: Vec<String>,
    /// dirge-w2de: the configured project gate's signature. `None` when
    /// the user did not opt in (no `verification_command` config key) —
    /// behaviour is byte-identical to before.
    project_gate: Option<GateSignature>,
    /// The project gate has been seen PASS this run. A failing gate run
    /// must not set this — the red dominates downstream.
    ran_project_gate: bool,
    /// When this gate was built — the run start. A script file whose mtime
    /// is at-or-after this is treated as authored by the agent THIS run
    /// (dirge-1elu.2). `None` only in the derived default; every
    /// constructor stamps it.
    run_started_at: Option<std::time::SystemTime>,
}

impl Inner {
    /// Fast-tier checks are green and the slow tier has not been seen
    /// green. The precondition for the full-suite escalation.
    fn is_fast_green_only(&self) -> bool {
        self.ran_fast && !self.ran_slow && !self.verification_failed
    }
}

/// Escalation budget for `mode`. Advisory says it once; blocking repeats
/// up to [`MAX_TIER_ESCALATIONS`]; off never escalates.
fn escalation_cap(mode: GateMode) -> u8 {
    match mode {
        GateMode::Off => 0,
        GateMode::Advisory => 1,
        GateMode::Blocking => MAX_TIER_ESCALATIONS,
    }
}

impl VerifierGate {
    // `new` and `with_project_gate` are the plain constructors, used
    // throughout the tests; production goes through
    // `with_project_gate_and_ci` so the CI advisory is populated.
    #[allow(dead_code)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                run_started_at: Some(std::time::SystemTime::now()),
                ..Inner::default()
            }),
        })
    }

    /// dirge-w2de: opt-in project gate (`verification_command` config).
    /// `Some` sets the command whose PASS is the only honest green — a
    /// green weaker command downgrades to
    /// [`VerificationStatus::FastGreenOnly`] until the gate itself
    /// passes. `None` (default) keeps behaviour byte-identical to
    /// [`VerifierGate::new`].
    #[allow(dead_code)]
    pub fn with_project_gate(project_gate: Option<String>) -> Arc<Self> {
        Self::with_project_gate_and_ci(project_gate, Vec::new())
    }

    /// As [`Self::with_project_gate`], plus the CI command list used for the
    /// advisory hint (dirge-w2de part 2). Resolved once by the caller so the
    /// filesystem is read at construction, not on every nudge.
    pub fn with_project_gate_and_ci(
        project_gate: Option<String>,
        ci_commands: Vec<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                project_gate: project_gate.as_deref().and_then(gate_signature),
                ci_commands,
                run_started_at: Some(std::time::SystemTime::now()),
                ..Inner::default()
            }),
        })
    }

    /// Record a finished tool call (called post-execution with the
    /// result). Flags a code edit when a mutating file tool touched a
    /// code-extension path; for a `bash` build/test command, records
    /// whether it passed or failed.
    pub fn record_outcome(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        result: &LoopToolResult,
        is_error: bool,
    ) {
        let mut inner = self.inner.lock_ignore_poison();
        match tool_name {
            // `edit_minified` is a real source-mutating tool (dirge-b1rr) —
            // without it here, an agent that edits only via edit_minified
            // never sets `edited_code` and the verify-before-done gate stays
            // silent on unverified changes.
            "write" | "edit" | "apply_patch" | "edit_minified" if touches_code_file(args) => {
                inner.edited_code = true;
                // Count code FILES, not tool calls (dirge-uw2l.7). A single
                // `apply_patch` routinely carries several operations, and a
                // model that batches four files into one call has left four
                // files unverified, not one. Counting calls undercounted the
                // unverified surface badly enough that the mid-run threshold
                // was near-unreachable for exactly the models that batch — as
                // an end-to-end run against a real model showed.
                inner.edits_since_verify = inner
                    .edits_since_verify
                    .saturating_add(code_paths_touched(args).max(1));
            }
            "bash" => {
                let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if is_verification_command(command) {
                    // Latest outcome wins.
                    let failed = is_error || result_indicates_failure(result);
                    // A masked command whose reported status is SUCCESS proves
                    // nothing: the zero belongs to `tail`, or to `true`, or to
                    // the `echo` after the semicolon. Decline to record it at
                    // all, so the status stays Unverified and the gate asks
                    // again — "we don't know" is the honest answer, and it
                    // fails toward nagging rather than toward a false green.
                    //
                    // A masked command that still reports FAILURE is
                    // trustworthy in that direction: something in the chain
                    // genuinely failed. Record the red.
                    if masks_failure(command) && !failed {
                        return;
                    }
                    // dirge-1elu.2: decline a green that rests SOLELY on a
                    // script-name match when the script was authored THIS
                    // run. A self-written validator proves nothing — the
                    // generator and the validator can share the same wrong
                    // assumptions — so leave `ran_verification` false, keep
                    // `edits_since_verify` counting, and let the gate ask
                    // again. A self-authored script that reports FAILURE is
                    // still trustworthy in that direction: the red is
                    // recorded below. A command that also carries a real
                    // word marker (`./check.sh && cargo test`) stays
                    // recognized — that's a wrapper, not a proxy.
                    if !failed
                        && is_script_name_only(command)
                        && script_name_paths(command)
                            .iter()
                            .any(|p| script_is_agent_authored(command, p, inner.run_started_at))
                    {
                        return;
                    }
                    inner.ran_verification = true;
                    inner.verification_failed = failed;
                    // Any verification attempt clears the mid-run counter —
                    // the model did go and check, whatever the outcome.
                    inner.edits_since_verify = 0;
                    // Tier coverage only counts when the command PASSED.
                    if !failed {
                        // dirge-w2de: a PASSED command whose signature
                        // matches the configured project gate satisfies
                        // it. A FAILING gate run must not set this — the
                        // red dominates downstream.
                        if let Some(gate) = &inner.project_gate
                            && gate_signatures(command).iter().any(|s| s == gate)
                        {
                            inner.ran_project_gate = true;
                        }
                        match verification_tier(command) {
                            Some(VerificationTier::Fast) => inner.ran_fast = true,
                            Some(VerificationTier::Slow) => inner.ran_slow = true,
                            None => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Read-only verification snapshot for the LLM critic (dirge-6q3w).
    /// Unlike [`check_before_finalize`], this never mutates the gate (it
    /// doesn't spend the one-shot nudge), so the cheap nudge and the
    /// pickier critic can both consult it in the same finalization.
    ///
    /// `mode` selects the vocabulary: [`GateMode::Off`] returns the legacy
    /// four variants (fast-only coverage collapses to
    /// [`VerificationStatus::VerifiedGreen`], exactly as before tiers
    /// existed); tiered modes can additionally return
    /// [`VerificationStatus::FastGreenOnly`].
    pub fn status(&self, mode: GateMode) -> VerificationStatus {
        let inner = self.inner.lock_ignore_poison();
        if !inner.edited_code {
            return VerificationStatus::NoCodeEdited;
        }
        if inner.ran_verification && inner.verification_failed {
            return VerificationStatus::VerifiedRed;
        }
        if !inner.ran_verification {
            return VerificationStatus::Unverified;
        }
        // Green — but possibly STALE (dirge-uw2l.3). Code edited after the
        // last green check isn't covered by it, so the green says nothing
        // about the current tree. Tiered modes only: off mode has always
        // reported a latched green and must stay byte-identical.
        if mode != GateMode::Off && inner.edits_since_verify > 0 {
            return VerificationStatus::Unverified;
        }
        // dirge-w2de: a project gate was configured but never ran green.
        // A green weaker command is a FALSE green — it says nothing about
        // the tree CI actually builds. Explicit opt-in
        // (`verification_command` set), so it applies in ALL modes,
        // including off, for that user; users who never set the key keep
        // byte-identical behaviour (off mode never returns FastGreenOnly).
        if inner.project_gate.is_some() && !inner.ran_project_gate {
            return VerificationStatus::FastGreenOnly;
        }
        // Tiered modes distinguish "the suite passed" from "only the cheap
        // checks passed"; off mode cannot tell them apart.
        if mode != GateMode::Off && inner.is_fast_green_only() {
            return VerificationStatus::FastGreenOnly;
        }
        VerificationStatus::VerifiedGreen
    }

    /// Code edits since the last verification command of any tier. Feeds
    /// the mid-run nudge; never mutates the gate (same read-only contract
    /// as [`VerifierGate::status`]).
    pub fn edits_since_verify(&self) -> u32 {
        self.inner.lock_ignore_poison().edits_since_verify
    }

    /// True when the working tree is currently at a verified-green point: a
    /// build/test command ran AND passed AND no code edit has landed since
    /// (dirge-uw2l.4). This is the safe-state abort's "last green" signal —
    /// mode-independent (unlike [`status`], it does NOT latch green in off
    /// mode), because what matters for a restore target is the actual
    /// verified point, not the reported vocabulary. Read-only; never mutates
    /// the gate.
    pub fn is_fresh_green(&self) -> bool {
        let inner = self.inner.lock_ignore_poison();
        inner.ran_verification && !inner.verification_failed && inner.edits_since_verify == 0
    }

    /// Finalization seam. Two independent gates, in order:
    ///
    /// 1. the legacy one-shot — a build/test failed, or none ran at all;
    /// 2. the tier escalation — fast checks are green but the full suite
    ///    never ran (tiered modes only).
    ///
    /// They hold SEPARATE budgets: a run that spends the legacy nudge while
    /// nothing had run must still be able to escalate once it reaches
    /// fast-green. See the module docs for the per-mode message ceiling.
    pub fn check_before_finalize(&self, mode: GateMode) -> Vec<LoopMessage> {
        let mut inner = self.inner.lock_ignore_poison();
        if !inner.edited_code {
            return Vec::new();
        }
        if !inner.fired {
            let nudge = if inner.verification_failed {
                Some(FAILED_NUDGE)
            } else if !inner.ran_verification {
                Some(VERIFY_NUDGE)
            } else {
                None // ran a build/test and it passed → confident, stay silent
            };
            if let Some(text) = nudge {
                inner.fired = true;
                // dirge-w2de part 2: name what CI actually enforces, at the
                // moment the model is being told to verify. Information only —
                // it changes no verdict, so it cannot cause a false green.
                let hint = ci_hint(&inner.ci_commands);
                return vec![LoopMessage::User(UserMessage::text(format!(
                    "{text}{hint}"
                )))];
            }
        }
        if inner.is_fast_green_only() && inner.escalations < escalation_cap(mode) {
            inner.escalations += 1;
            return vec![LoopMessage::User(UserMessage::text(FULL_SUITE_NUDGE))];
        }
        Vec::new()
    }
}

/// The build/test commands this project's CI actually runs (dirge-w2de part 2).
///
/// # Why this is a LIST and not a gate
///
/// Part 1 lets a user name one authoritative command via `verification_command`.
/// The obvious follow-up was to auto-detect that command from CI so a default
/// config is protected too. That does not work, and the evidence is this very
/// repo: its `ci.yml` yields four distinct recognized signatures — `cargo fmt`
/// and `cargo clippy` (Fast), `cargo build` and `cargo nextest` (Slow). Two are
/// equally "strongest", so any rule that picks one is guessing, and a WRONG
/// auto-gate is worse than none: it downgrades every honest green to
/// fast-green-only and nags forever. Refusing to guess means returning nothing
/// on exactly the repo the feature was written for.
///
/// The premise was wrong. Real CI does not have one gate; it has several, all
/// required. So this returns the recognized set as INFORMATION, and the
/// verifier folds it into its nudge text. That addresses the actual failure —
/// an agent ran `cargo test`, saw it pass, and reported success without knowing
/// clippy was what CI enforced — without touching any verdict. It cannot
/// produce a false green or a false nag, because it changes no decision.
///
/// Commands carrying `${{ ... }}` are skipped: the expansion is unknown, and a
/// half-substituted command is not something to hand a model as fact.
pub fn ci_verification_commands(repo_root: &std::path::Path) -> Vec<String> {
    let dir = repo_root.join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    // Sort so the result never depends on readdir order — advice that changes
    // between runs reads as noise.
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("yml") | Some("yaml")
            )
        })
        .collect();
    files.sort();

    let mut out: Vec<String> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for cmd in run_step_commands(&text) {
            if cmd.contains("${{") || !is_verification_command(&cmd) {
                continue;
            }
            // Dedupe by SIGNATURE, not by string: three clippy invocations with
            // different feature flags are one instruction to the reader.
            let sig = gate_signature(&cmd);
            if out.iter().any(|existing| gate_signature(existing) == sig) {
                continue;
            }
            out.push(cmd);
        }
    }
    out
}

/// Shell commands from YAML `run:` steps. Handles both `run: cmd` and the block
/// scalar `run: |` followed by indented lines, since real workflows use both.
/// Deliberately line-oriented — a YAML dependency to read four lines would be a
/// poor trade, and a mis-parse here degrades to "no advice", never to a wrong
/// verdict.
fn run_step_commands(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut block: Option<usize> = None; // indent of an open `run: |` block
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if let Some(block_indent) = block {
            // A block continues while indented deeper than the `run:` key.
            if trimmed.is_empty() {
                continue;
            }
            if indent > block_indent {
                out.push(trimmed.to_string());
                continue;
            }
            block = None;
        }
        // A step may be a list item (`- run: cmd`) or a plain key under one
        // (`run: cmd`). Both occur in the same real workflow, and missing the
        // list form silently drops every step written that way — which is most
        // of them in most repos.
        let key = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let Some(rest) = key.strip_prefix("run:") else {
            continue;
        };
        let rest = rest.trim();
        if rest == "|" || rest == ">" || rest == "|-" || rest == ">-" {
            block = Some(indent);
        } else if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    out
}

/// Render the CI command list as a sentence appended to a verifier nudge.
/// Empty list → empty string, so the nudge is byte-identical when there is
/// nothing to say.
pub fn ci_hint(commands: &[String]) -> String {
    if commands.is_empty() {
        return String::new();
    }
    let list = commands
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        " This project's CI runs: {list} — a green check that isn't one of those may not be what gets enforced."
    )
}

/// Does this command's shell shape MASK a non-zero exit from the verification
/// step, so a reported success proves nothing?
///
/// The verifier reads pass/fail from bash's `Exit code: N` line, which carries
/// the exit status of the whole command. Several ordinary shapes make that
/// status belong to something other than the check:
///
/// - `cargo clippy | tail -2` — the status is `tail`'s. Without `pipefail` a
///   red build exits 0.
/// - `cargo test || true` — explicitly discards the failure.
/// - `cargo test; echo done` — the status is `echo`'s.
/// - `cargo test &` — backgrounded; nothing is waited on.
///
/// `&&` is NOT masking: it short-circuits, so a failing left side is the exit
/// status. Redirections like `2>&1` contain no pipe.
///
/// This exists because it is the exact failure that kept recurring while
/// building this epic: an agent ran `cargo clippy --all-targets | tail -2`,
/// read `$?`, saw zero, and reported success over six hard errors. The verifier
/// would have believed it too — measured, `cargo test || true` latched
/// VerifiedGreen. A gate that cannot fail for the reason that matters is worse
/// than no gate, because it is trusted.
fn masks_failure(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'|' => {
                // `||` discards a failure; a lone `|` hands the status to the
                // next stage. Both mask.
                return true;
            }
            b';' => {
                // Only masks when something follows — a trailing `;` does not.
                if command[i + 1..].trim().is_empty() {
                    return false;
                }
                return true;
            }
            b'\n' => {
                // A newline between commands is an exact synonym for `;`
                // (dirge-1elu.3): the exit status belongs to the LAST
                // command, so an earlier failure is discarded. Only masks
                // when something follows — a trailing newline does not.
                //
                // A backslash immediately before the newline is a line
                // continuation, not a separator — `cargo test && \<newline>
                // echo done` short-circuits and its status is honest.
                if i > 0 && bytes[i - 1] == b'\\' {
                    i += 1;
                    continue;
                }
                if command[i + 1..].trim().is_empty() {
                    return false;
                }
                return true;
            }
            b'&' => {
                // `&&` is fine (short-circuits); a lone `&` backgrounds.
                if bytes.get(i + 1) == Some(&b'&') {
                    i += 2;
                    continue;
                }
                // `2>&1` and friends: `&` preceded by `>` is a redirection.
                if i > 0 && bytes[i - 1] == b'>' {
                    i += 1;
                    continue;
                }
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Concatenate the text blocks of a tool result for failure scanning.
fn result_text(result: &LoopToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bash result indicates failure when the harness appended its
/// `Exit code: N` line — bash emits that line (as a standalone line)
/// ONLY on a non-zero exit (`bash.rs`). Match it anchored to the start
/// of a line and require N to parse to a non-zero integer, so a green
/// run whose own output merely contains the text `Exit code: 0` or
/// mentions `Exit code:` in prose isn't misread as red (dirge-fc40).
fn result_indicates_failure(result: &LoopToolResult) -> bool {
    result_text(result).lines().any(exit_code_line_is_failure)
}

/// True iff `line` is the harness's non-zero exit marker: it begins
/// (after trimming) with `Exit code:` and the remainder parses to a
/// non-zero integer. `Exit code: 0` and non-numeric remainders are not
/// failures.
fn exit_code_line_is_failure(line: &str) -> bool {
    line.trim()
        .strip_prefix("Exit code:")
        .and_then(|rest| rest.trim().parse::<i64>().ok())
        .is_some_and(|code| code != 0)
}

/// Heuristic: does this shell command look like a build/test/check?
/// Broad on purpose — recognizing more commands as "verification" means
/// the gate stays silent rather than nagging.
///
/// Markers are matched on whole shell WORDS, not as substrings, so
/// `git checkout` no longer matches `check` and `ls tests/` no longer
/// matches `test`. A segment carrying a non-building subcommand
/// (`npm install`, `cargo add`) is disqualified outright even though its
/// tool name is a marker (dirge-eg37). Splitting on `&& || ; |` and
/// newlines means one real build in a chain still counts.
fn is_verification_command(command: &str) -> bool {
    command
        .split(['&', '|', ';', '\n'])
        .any(segment_is_verification)
}

/// Build/test/lint tool + subcommand words. Includes linters/formatters
/// invoked by bare name (`eslint .`, `golangci-lint run`).
const WORD_MARKERS: &[&str] = &[
    "test",
    "build",
    "check",
    "lint",
    "compile",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "pytest",
    "tox",
    "make",
    "gradle",
    "mvn",
    "ctest",
    "cmake",
    "rustc",
    "tsc",
    "jest",
    "vitest",
    "mocha",
    "clippy",
    "eslint",
    "golangci-lint",
    "prettier",
    "ruff",
    "flake8",
    "mypy",
    "shellcheck",
    "rubocop",
];

/// Subcommands that do no building/testing. Their presence disqualifies
/// the segment even when the tool name (npm/cargo/yarn) is a marker.
const NON_VERIFY: &[&str] = &["checkout", "install", "add", "remove", "uninstall"];

/// Linters/typecheckers that are cheap by construction — invoking one at
/// all is a fast-tier check regardless of its arguments.
const FAST_LINTERS: &[&str] = &[
    "eslint",
    "ruff",
    "mypy",
    "flake8",
    "shellcheck",
    "rubocop",
    "golangci-lint",
    "prettier",
    "clippy",
];

/// `npm run <script>` names that read as a cheap check. Matched as
/// substrings so `typecheck`, `lint:fix`, and `format-check` all land.
const FAST_SCRIPT_WORDS: &[&str] = &["lint", "check", "typecheck", "tsc", "format", "fmt"];

/// `cargo` flags that consume the following token as their VALUE. Without
/// this list `cargo test -p mycrate` reads `mycrate` as a test filter and
/// mis-tiers a whole package suite as Fast — the one place a
/// misclassification could produce a wrong nag.
const CARGO_VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "--features",
    "-j",
    "--jobs",
    "--target",
    "--manifest-path",
    "--message-format",
    "--profile",
    "--bin",
    "--test",
    "--example",
];

/// Which fidelity tier `command` exercises, or `None` when it isn't a
/// verification command at all (dirge-uw2l.2).
///
/// Layers strictly on top of [`is_verification_command`] — the recognition
/// set is untouched, so tiering can never widen what counts as
/// verification (dirge-eg37 holds). A chain takes its strongest segment.
///
/// **Unknown tiers default to `Slow`.** The tier signal only ever *adds*
/// nudges, so defaulting an unrecognized command to Slow errs toward
/// silence — a missed escalation, never a false nag. That matches the
/// recognition heuristic's own stated bias. Defaulting to Fast would
/// invent escalation pressure from commands we know nothing about.
pub fn verification_tier(command: &str) -> Option<VerificationTier> {
    command
        .split(['&', '|', ';', '\n'])
        .filter(|segment| segment_is_verification(segment))
        .map(segment_tier)
        .max()
}

/// Tier for a single already-recognized verification segment.
fn segment_tier(segment: &str) -> VerificationTier {
    let owned: Vec<String> = segment
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    // Drop `VAR=value` prefixes so `RUST_LOG=debug cargo clippy` tiers as cargo.
    let tokens: Vec<&str> = owned
        .iter()
        .map(String::as_str)
        .skip_while(|t| t.contains('='))
        .collect();
    let Some((command, args)) = tokens.split_first() else {
        return VerificationTier::Slow;
    };
    // A path-shaped command word tiers on its basename (`scripts/lint.sh`).
    let base = command.rsplit('/').next().unwrap_or(command);
    match base {
        "cargo" => cargo_tier(args),
        "pytest" => pytest_tier(args),
        "npm" | "pnpm" | "yarn" => node_script_tier(args),
        "go" => go_tier(args),
        "jest" | "vitest" | "mocha" => js_runner_tier(args),
        // Typecheck only — never runs a test.
        "tsc" | "rustc" => VerificationTier::Fast,
        _ if FAST_LINTERS.contains(&base) => VerificationTier::Fast,
        // `make` included deliberately: by automake convention `make check`
        // IS the full suite, not a lint.
        _ => VerificationTier::Slow,
    }
}

fn cargo_tier(args: &[&str]) -> VerificationTier {
    let Some(sub) = args.iter().find(|t| !t.starts_with('-')) else {
        return VerificationTier::Slow;
    };
    match *sub {
        "check" | "clippy" | "fmt" => VerificationTier::Fast,
        "test" | "bench" if cargo_has_filter(args) => VerificationTier::Fast,
        _ => VerificationTier::Slow,
    }
}

/// dirge-w2de: the project's real gate command, normalized to
/// (program, subcommand). Two textual forms of the same gate
/// (`RUSTFLAGS="-D warnings" cargo clippy --all-targets` vs
/// `cargo clippy --all-targets -- -D warnings`) normalize to the same
/// signature, so matching is robust to env prefixes and flag placement
/// without being string-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateSignature {
    program: String,
    subcommand: Option<String>,
}

impl GateSignature {
    /// dirge-1elu.5: wire form for the `command_ran:` memory expectation.
    /// `<program> <subcommand>` — a program is a single shell word, so a
    /// first-space split is unambiguous.
    pub(crate) fn to_wire(&self) -> String {
        match &self.subcommand {
            Some(s) => format!("{} {}", self.program, s),
            None => self.program.clone(),
        }
    }

    pub(crate) fn from_wire(s: &str) -> Option<Self> {
        let mut it = s.splitn(2, ' ');
        let program = it.next()?.trim().to_string();
        if program.is_empty() {
            return None;
        }
        let subcommand = it
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        Some(GateSignature {
            program,
            subcommand,
        })
    }
}

/// Tokenize a shell command keeping quoted values as ONE word
/// (`RUSTFLAGS="-D warnings"` is a single env assignment, not three
/// words). Quote characters are dropped; `None` when the segment has no
/// words at all.
fn shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    in_word = true;
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                c => {
                    current.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(current);
    }
    (!words.is_empty()).then_some(words)
}

/// Extract the (program, subcommand) signature of ONE shell command
/// segment: skip leading `VAR=value` env assignments, take the first bare
/// word as the program and the next non-flag word as the subcommand.
fn segment_signature(segment: &str) -> Option<GateSignature> {
    let words = shell_words(segment)?;
    let mut rest = words.iter().skip_while(|w| w.contains('='));
    let program = rest.next()?.clone();
    let subcommand = rest.find(|w| !w.starts_with('-')).cloned();
    Some(GateSignature {
        program,
        subcommand,
    })
}

/// Every (program, subcommand) signature in a command, splitting chains on
/// `& | ; \n` — the same separators [`is_verification_command`] uses.
///
/// ALL segments, not just the last. This matters for satisfying the gate:
/// the caller only consults this for a command that PASSED, and under `&&`
/// a passing chain means every segment passed. So `cargo clippy && cargo
/// test` genuinely satisfies a `cargo clippy` gate — taking only the last
/// segment would miss it and leave the run reporting fast-green-only after
/// the real gate had in fact run clean.
fn gate_signatures(command: &str) -> Vec<GateSignature> {
    command
        .split(['&', '|', ';', '\n'])
        .filter_map(segment_signature)
        .collect()
}

/// The single signature of a command treated as a gate SPECIFICATION (the
/// `verification_command` config value). A spec naming a chain is taken by
/// its last segment: `cargo fmt && cargo clippy` specifies the clippy gate.
pub(crate) fn gate_signature(command: &str) -> Option<GateSignature> {
    gate_signatures(command).pop()
}

/// True when a positional test-name filter survives after the subcommand,
/// skipping flags and the values of [`CARGO_VALUE_FLAGS`]. Everything past
/// a bare `--` is harness arguments (`cargo test foo -- --exact`), not a
/// filter.
fn cargo_has_filter(args: &[&str]) -> bool {
    let head = match args.iter().position(|t| *t == "--") {
        Some(i) => &args[..i],
        None => args,
    };
    let mut rest = head.iter();
    let mut seen_subcommand = false;
    while let Some(token) = rest.next() {
        if token.starts_with('-') {
            if CARGO_VALUE_FLAGS.contains(token) {
                rest.next();
            }
            continue;
        }
        if !seen_subcommand {
            seen_subcommand = true;
            continue;
        }
        return true;
    }
    false
}

fn pytest_tier(args: &[&str]) -> VerificationTier {
    let targeted = args
        .iter()
        .any(|t| *t == "-k" || *t == "-m" || t.contains("::") || t.ends_with(".py"));
    if targeted {
        VerificationTier::Fast
    } else {
        VerificationTier::Slow
    }
}

/// `npm`/`pnpm`/`yarn`: `test` is the suite; a `run <script>` (or bare
/// `yarn <script>`) tiers on whether the script name reads as a check.
fn node_script_tier(args: &[&str]) -> VerificationTier {
    let Some(first) = args.iter().find(|t| !t.starts_with('-')) else {
        return VerificationTier::Slow;
    };
    if *first == "test" {
        return VerificationTier::Slow;
    }
    let script = if *first == "run" {
        args.iter()
            .skip_while(|t| **t != "run")
            .nth(1)
            .copied()
            .unwrap_or("")
    } else {
        *first
    };
    if FAST_SCRIPT_WORDS.iter().any(|w| script.contains(w)) {
        VerificationTier::Fast
    } else {
        VerificationTier::Slow
    }
}

fn go_tier(args: &[&str]) -> VerificationTier {
    let Some(sub) = args.iter().find(|t| !t.starts_with('-')) else {
        return VerificationTier::Slow;
    };
    match *sub {
        "vet" => VerificationTier::Fast,
        "run" => VerificationTier::Fast,
        "test" if args.iter().any(|t| *t == "-run" || t.starts_with("-run=")) => {
            VerificationTier::Fast
        }
        _ => VerificationTier::Slow,
    }
}

/// `jest`/`vitest`/`mocha`: a positional path or pattern narrows the run.
/// `vitest`'s `run` subcommand isn't a target.
fn js_runner_tier(args: &[&str]) -> VerificationTier {
    let targeted = args.iter().any(|t| !t.starts_with('-') && *t != "run");
    if targeted {
        VerificationTier::Fast
    } else {
        VerificationTier::Slow
    }
}

/// Two-word markers whose leading word isn't a marker on its own.
const PAIR_MARKERS: &[(&str, &str)] = &[("go", "vet"), ("go", "run"), ("go", "test")];

/// Why a segment counted as verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentMatch {
    /// No marker and no script-name match.
    None,
    /// A word or pair marker matched (`cargo test`, `pytest`, `make check`).
    Word,
    /// Only the path-shaped command word matched (`./check.sh`).
    ScriptName,
}

fn segment_match(segment: &str) -> SegmentMatch {
    let tokens: Vec<String> = segment
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    if tokens.iter().any(|t| NON_VERIFY.contains(&t.as_str())) {
        return SegmentMatch::None;
    }
    // Whole-word markers. A `--check`-style flag is its dash-stripped
    // word, so `prettier --check .` and `cmake --build` register.
    if tokens
        .iter()
        .any(|t| WORD_MARKERS.contains(&t.trim_start_matches('-')))
    {
        return SegmentMatch::Word;
    }
    if tokens
        .windows(2)
        .any(|w| PAIR_MARKERS.contains(&(w[0].as_str(), w[1].as_str())))
    {
        return SegmentMatch::Word;
    }
    // The command word may be a path to a repo script: match markers
    // inside its basename, split on `-`/`_`/`.`, accepting a plural form,
    // so `./run-tests.sh` and `scripts/lint.sh` register. Only the
    // executed word gets this treatment — an argument like `ls tests/`
    // must not count (dirge-eg37).
    if command_word(&tokens).is_some_and(script_name_is_verification) {
        return SegmentMatch::ScriptName;
    }
    SegmentMatch::None
}

/// Behavioural wrapper for the call sites that only need the bool.
fn segment_is_verification(segment: &str) -> bool {
    !matches!(segment_match(segment), SegmentMatch::None)
}

/// True when a command is recognized as verification ONLY through the
/// script-name path — no word/pair marker appears in any segment
/// (dirge-1elu.2). A command that also carries a real marker stays
/// recognized regardless of provenance: a self-written `check.sh` that
/// invokes `cargo test` is a wrapper, not a proxy.
fn is_script_name_only(command: &str) -> bool {
    let mut script = false;
    for seg in command.split(['&', '|', ';', '\n']) {
        match segment_match(seg) {
            SegmentMatch::Word => return false,
            SegmentMatch::ScriptName => script = true,
            SegmentMatch::None => {}
        }
    }
    script
}

/// The path-shaped command words of the segments that matched only by
/// script name, in their ORIGINAL case — paths are case-sensitive, so the
/// lowercased tokens used for marker matching can't be reused here.
fn script_name_paths(command: &str) -> Vec<std::path::PathBuf> {
    command
        .split(['&', '|', ';', '\n'])
        .filter(|seg| matches!(segment_match(seg), SegmentMatch::ScriptName))
        .filter_map(|seg| {
            let tokens: Vec<String> = seg.split_whitespace().map(str::to_string).collect();
            command_word(&tokens).map(std::path::PathBuf::from)
        })
        .collect()
}

/// Did the agent author `path` THIS run? Two OR'd sources — the
/// modified-files registry (write/edit/apply_patch) and the file's mtime
/// at-or-after run start (a bash heredoc the registry never sees). If
/// neither can answer (missing file, unreadable mtime, no run start),
/// treat it as NOT authored — degrade toward today's behaviour, never
/// toward a new false nag (dirge-1elu.2).
/// Resolve a command's script path the way bash would: fold the command's
/// leading `cd`/`pushd` segments onto the run cwd — the SAME lexical folder
/// the permission layer uses (`fold_cd_dirs`), not a second implementation
/// (dirge-1elu.2 follow-up). `cd sub && ./check.sh` resolves `./check.sh`
/// against `sub/`. Without the semantic-bash feature the path resolves
/// against the run cwd exactly as before.
#[cfg(feature = "semantic-bash")]
fn resolve_script_path(command: &str, path: &std::path::Path) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let segments: Vec<String> = command
        .split(['&', '|', ';', '\n'])
        .map(str::to_string)
        .collect();
    let effective =
        crate::agent::tools::bash::check::fold_cd_dirs(&cwd.to_string_lossy(), &segments);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::Path::new(&effective).join(path)
    }
}

/// Non-semantic-bash builds keep today's behaviour: no cd folding.
#[cfg(not(feature = "semantic-bash"))]
fn resolve_script_path(_command: &str, path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

fn script_is_agent_authored(
    command: &str,
    path: &std::path::Path,
    run_started_at: Option<std::time::SystemTime>,
) -> bool {
    // Registry entries are canonical absolute paths, so a relative command
    // word (`./check.sh`) must canonicalize against the effective run cwd to
    // match — which is also the cwd bash inherits, since the tool spawns
    // without a `current_dir` override. `cd`/`pushd` segments are folded in
    // first (dirge-1elu.2 follow-up). The raw spelling is checked too, for
    // entries recorded before canonicalization succeeded.
    let resolved = resolve_script_path(command, path);
    let canonical = crate::permission::path::canonical_or_self(&resolved);
    let in_registry = crate::agent::tools::modified::MODIFIED_FILES
        .lock()
        .is_ok_and(|s| s.contains(&canonical) || s.contains(&resolved));
    if in_registry {
        return true;
    }
    let Some(run_start) = run_started_at else {
        return false;
    };
    std::fs::metadata(&canonical)
        .and_then(|m| m.modified())
        .is_ok_and(|mtime| mtime >= run_start)
}

/// First token that isn't a `VAR=value` environment prefix.
fn command_word(tokens: &[String]) -> Option<&str> {
    tokens.iter().map(|t| t.as_str()).find(|t| !t.contains('='))
}

/// True when a path-shaped command word (`./run-tests.sh`,
/// `scripts/lint.sh`) names a verification script: its basename, split on
/// `-`/`_`/`.`, carries a marker word (singular or plural).
fn script_name_is_verification(token: &str) -> bool {
    if !token.contains('/') {
        return false;
    }
    let basename = token.rsplit('/').next().unwrap_or(token);
    basename.split(['-', '_', '.']).any(|piece| {
        WORD_MARKERS.contains(&piece)
            || piece
                .strip_suffix('s')
                .is_some_and(|p| WORD_MARKERS.contains(&p))
    })
}

/// True if any path argument names a source-code file (by extension).
/// Looks at top-level `path` / `file_path` / `file` and `apply_patch`'s
/// `operations[].path`.
fn touches_code_file(args: &serde_json::Value) -> bool {
    code_paths_touched(args) > 0
}

/// How many DISTINCT code paths this mutating call touches (dirge-uw2l.7).
/// `apply_patch` carries an `operations` array, so one call can edit many
/// files; the mid-run verify counter needs the file count, not the call
/// count. Deduplicated — patching the same file twice in one call is one
/// unverified file.
fn code_paths_touched(args: &serde_json::Value) -> u32 {
    let Some(obj) = args.as_object() else {
        return 0;
    };
    let mut paths: Vec<&str> = Vec::new();
    for key in ["path", "file_path", "file"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            paths.push(s);
        }
    }
    if let Some(ops) = obj.get("operations").and_then(|v| v.as_array()) {
        for op in ops {
            for key in ["path", "new_path"] {
                if let Some(s) = op.get(key).and_then(|v| v.as_str()) {
                    paths.push(s);
                }
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths.iter().filter(|p| is_code_path(p)).count() as u32
}

/// Source-code file extensions. A change to one of these is "editing
/// code"; docs/config (md, txt, json, toml, …) deliberately don't count,
/// so a doc-only edit never triggers the verify nudge.
const CODE_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "rb", "java", "kt", "kts", "c", "h",
    "cc", "cpp", "hpp", "cxx", "cs", "swift", "php", "scala", "clj", "cljs", "cljc", "ex", "exs",
    "sh", "bash", "lua", "pl", "hs", "ml", "sql", "vue", "svelte",
];

fn is_code_path(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => CODE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_result() -> LoopToolResult {
        LoopToolResult {
            content: vec![json!({"type": "text", "text": "ok"})],
            details: json!(null),
            terminate: None,
        }
    }

    fn failed_result() -> LoopToolResult {
        // Mirrors bash's non-zero-exit output: the harness appends an
        // "Exit code: N" line.
        LoopToolResult {
            content: vec![json!({"type": "text", "text": "test failed\nExit code: 101"})],
            details: json!(null),
            terminate: None,
        }
    }

    fn nudge(gate: &VerifierGate) -> Option<String> {
        gate.check_before_finalize(GateMode::Off)
            .into_iter()
            .next()
            .map(|m| match m {
                LoopMessage::User(u) => u.text_joined(),
                _ => panic!("expected user message"),
            })
    }

    #[test]
    fn edited_code_without_running_nudges_to_verify() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        let n = nudge(&g).expect("should nudge");
        assert!(n.contains("didn't run the tests"), "verify nudge: {n}");
    }

    /// dirge-b1rr: an `edit_minified` change to a code file must count as
    /// a code edit so the verify-before-done gate fires.
    #[test]
    fn edit_minified_counts_as_a_code_edit() {
        let g = VerifierGate::new();
        g.record_outcome(
            "edit_minified",
            &json!({"path": "src/auth.rs"}),
            &ok_result(),
            false,
        );
        let n = nudge(&g).expect("edit_minified should arm the verify nudge");
        assert!(n.contains("didn't run the tests"), "verify nudge: {n}");
    }

    #[test]
    fn edited_code_then_passing_test_is_silent() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert!(
            nudge(&g).is_none(),
            "passing verification should stay silent"
        );
    }

    #[test]
    fn edited_code_then_failing_test_nudges_to_fix() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        let n = nudge(&g).expect("should nudge on red build");
        assert!(n.contains("failed"), "fix-it nudge: {n}");
        assert!(
            n.contains("red build"),
            "should mention not finishing on red: {n}"
        );
    }

    #[test]
    fn rerun_green_after_failure_clears_the_nudge() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        // Fix, re-run, now green — latest outcome wins.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert!(
            nudge(&g).is_none(),
            "a subsequent green run should clear the failure"
        );
    }

    #[test]
    fn non_verification_command_does_not_count_as_verified() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        // `ls` is not a build/test command → still unverified.
        g.record_outcome("bash", &json!({"command": "ls -la"}), &ok_result(), false);
        let n = nudge(&g).expect("ls is not verification");
        assert!(n.contains("didn't run the tests"));
    }

    #[test]
    fn tool_execution_error_counts_as_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        // is_error=true (tool blew up) on a verification command → failed.
        g.record_outcome("bash", &json!({"command": "make test"}), &ok_result(), true);
        let n = nudge(&g).expect("errored verification is a failure");
        assert!(n.contains("failed"));
    }

    #[test]
    fn doc_only_edit_never_nudges() {
        let g = VerifierGate::new();
        g.record_outcome("write", &json!({"path": "README.md"}), &ok_result(), false);
        assert!(nudge(&g).is_none());
    }

    #[test]
    fn no_edits_never_nudges() {
        let g = VerifierGate::new();
        g.record_outcome("read", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        assert!(nudge(&g).is_none());
    }

    #[test]
    fn nudge_fires_at_most_once() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        assert!(nudge(&g).is_some());
        assert!(nudge(&g).is_none(), "bounded to once per run");
    }

    #[test]
    fn apply_patch_with_code_operation_counts_as_edit() {
        let g = VerifierGate::new();
        g.record_outcome(
            "apply_patch",
            &json!({"operations": [{"type": "update", "path": "src/lib.rs"}]}),
            &ok_result(),
            false,
        );
        assert!(nudge(&g).is_some());
    }

    #[test]
    fn status_reflects_run_signals() {
        let g = VerifierGate::new();
        assert_eq!(g.status(GateMode::Off), VerificationStatus::NoCodeEdited);

        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert_eq!(g.status(GateMode::Off), VerificationStatus::Unverified);

        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedRed);

        // Latest outcome wins — fix then re-run green.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    /// `status()` must NOT spend the one-shot nudge — the cheap gate and
    /// the pickier critic both read it in the same finalization.
    #[test]
    fn status_does_not_consume_the_nudge() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert_eq!(g.status(GateMode::Off), VerificationStatus::Unverified);
        // Reading status repeatedly leaves the nudge intact.
        let _ = g.status(GateMode::Off);
        assert!(nudge(&g).is_some(), "status() must not arm `fired`");
    }

    #[test]
    fn is_code_path_recognizes_common_extensions() {
        assert!(is_code_path("src/main.rs"));
        assert!(is_code_path("app/Foo.TS"));
        assert!(!is_code_path("README.md"));
        assert!(!is_code_path("Makefile"));
    }

    /// Build a bash result with an arbitrary text body.
    fn bash_result(text: &str) -> LoopToolResult {
        LoopToolResult {
            content: vec![json!({"type": "text", "text": text})],
            details: json!(null),
            terminate: None,
        }
    }

    /// dirge-fc40: a green run whose own output contains "Exit code: 0"
    /// (a wrapper, a status echo) must NOT be read as a red build. Only
    /// the harness's non-zero marker counts.
    #[test]
    fn echoed_exit_code_zero_is_not_a_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "make test"}),
            &bash_result("make test\nall passed\nExit code: 0"),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
        assert!(nudge(&g).is_none(), "echoed 'Exit code: 0' must stay green");
    }

    /// dirge-fc40: "Exit code:" in prose (not the harness's standalone
    /// non-zero marker) must not fabricate a failure.
    #[test]
    fn exit_code_in_prose_is_not_a_failure() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &bash_result("the wrapper prints 'Exit code: N' on error\ndone"),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    /// The genuine harness marker (standalone non-zero line) is still a
    /// failure regardless of where it lands in the buffer (inline appends
    /// it last; the output relay prepends it first).
    #[test]
    fn harness_nonzero_marker_is_a_failure_anywhere() {
        for text in ["boom\nExit code: 101", "Exit code: 137\nhead\ntail"] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome(
                "bash",
                &json!({"command": "cargo test"}),
                &bash_result(text),
                false,
            );
            assert_eq!(
                g.status(GateMode::Off),
                VerificationStatus::VerifiedRed,
                "non-zero marker in {text:?} should be red"
            );
        }
    }

    /// dirge-eg37: `git checkout` / `npm install` / `cargo add` / `ls
    /// tests/` must not be mistaken for a build/test because a marker
    /// appears as a substring or as the tool name of a non-building
    /// subcommand. A code edit followed only by these stays Unverified.
    #[test]
    fn non_build_subcommands_are_not_verification() {
        for cmd in [
            "git checkout main",
            "npm install",
            "cargo add serde",
            "ls tests/",
            "yarn add left-pad",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(GateMode::Off),
                VerificationStatus::Unverified,
                "`{cmd}` must not count as verification"
            );
        }
    }

    /// Linters/formatters invoked by name and repo test scripts are
    /// verification even though no marker appears as a standalone word:
    /// `eslint .`, `golangci-lint run`, `prettier --check .`,
    /// `./run-tests.sh`.
    #[test]
    fn linters_and_scripts_count_as_verification() {
        for cmd in [
            "eslint .",
            "golangci-lint run",
            "prettier --check .",
            "./run-tests.sh",
            "scripts/lint.sh --fast",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(GateMode::Off),
                VerificationStatus::VerifiedGreen,
                "`{cmd}` should register as verification"
            );
        }
    }

    /// dirge-eg37: real build/test/lint commands still register.
    #[test]
    fn real_build_commands_still_count() {
        for cmd in [
            "cargo test",
            "make check",
            "npm run build",
            "go vet ./...",
            "pytest -q",
            "RUST_LOG=debug cargo clippy",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(GateMode::Off),
                VerificationStatus::VerifiedGreen,
                "`{cmd}` should register as verification"
            );
        }
    }

    // --- dirge-uw2l.2: tiered verification (RAX fidelity pyramid, R1) ---

    #[test]
    fn cargo_check_clippy_are_fast() {
        for cmd in ["cargo check", "cargo clippy", "RUST_LOG=debug cargo clippy"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn cargo_test_with_filter_is_fast() {
        for cmd in [
            "cargo test my_test",
            "cargo test verifier::tests::status_reflects_run_signals",
            "cargo test foo -- --exact",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn bare_cargo_test_is_slow() {
        for cmd in [
            "cargo test",
            "cargo test --workspace",
            "cargo test --all-features",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn cargo_test_package_flag_value_is_not_a_filter() {
        for cmd in [
            "cargo test -p mycrate",
            "cargo test --package mycrate",
            "cargo test --features foo",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn cargo_build_is_slow() {
        for cmd in ["cargo build", "cargo build --release"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn pytest_targeting() {
        for cmd in [
            "pytest tests/foo.py::test_bar",
            "pytest tests/foo.py",
            "pytest -k bar",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
        for cmd in ["pytest", "pytest -q", "pytest tests/"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn npm_tiers() {
        assert_eq!(verification_tier("npm test"), Some(VerificationTier::Slow));
        for cmd in [
            "npm run lint",
            "npm run typecheck",
            "npm run check",
            "pnpm run lint",
            "yarn lint",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
        for cmd in ["npm run build", "npm run deploy-docs"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn bare_linters_are_fast() {
        for cmd in [
            "eslint .",
            "ruff check .",
            "mypy src/",
            "prettier --check .",
            "shellcheck x.sh",
            "golangci-lint run",
            "flake8 src/",
            "rubocop",
        ] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn tsc_and_rustc_are_fast() {
        for cmd in ["tsc --noEmit", "tsc"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn make_is_always_slow() {
        for cmd in ["make", "make check", "make test"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn go_tiers() {
        assert_eq!(
            verification_tier("go vet ./..."),
            Some(VerificationTier::Fast)
        );
        assert_eq!(
            verification_tier("go test ./..."),
            Some(VerificationTier::Slow)
        );
        assert_eq!(
            verification_tier("go test -run TestFoo ./..."),
            Some(VerificationTier::Fast)
        );
    }

    #[test]
    fn jest_vitest_mocha_tiers() {
        for cmd in ["jest src/foo.test.ts", "vitest run foo"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Fast),
                "`{cmd}`"
            );
        }
        for cmd in ["jest", "vitest run", "mocha"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn script_paths_default_slow() {
        for cmd in ["./run-tests.sh", "scripts/lint.sh"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    /// When the tier genuinely can't be determined we default **Slow**: the
    /// tier signal only ever *adds* nudges, so an unknown command errs
    /// toward silence (a missed escalation), never a false nag.
    #[test]
    fn ambiguous_verification_defaults_slow() {
        for cmd in ["ctest", "gradle test", "mvn test", "tox", "make check"] {
            assert_eq!(
                verification_tier(cmd),
                Some(VerificationTier::Slow),
                "`{cmd}`"
            );
        }
    }

    #[test]
    fn non_verification_has_no_tier() {
        for cmd in [
            "ls tests/",
            "git checkout main",
            "npm install",
            "cargo add serde",
            "yarn add left-pad",
        ] {
            assert_eq!(verification_tier(cmd), None, "`{cmd}`");
        }
    }

    #[test]
    fn chain_tier_is_strongest_segment() {
        assert_eq!(
            verification_tier("cargo check && cargo test"),
            Some(VerificationTier::Slow)
        );
        assert_eq!(
            verification_tier("cargo check && cargo clippy"),
            Some(VerificationTier::Fast)
        );
        assert_eq!(
            verification_tier("cargo test || echo nope"),
            Some(VerificationTier::Slow)
        );
    }

    #[test]
    fn off_mode_status_is_legacy() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    #[test]
    fn fast_only_is_fast_green_only_when_tiered() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::FastGreenOnly
        );
        assert_eq!(
            g.status(GateMode::Blocking),
            VerificationStatus::FastGreenOnly
        );
    }

    #[test]
    fn slow_green_is_verified_green_when_tiered() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedGreen
        );
    }

    #[test]
    fn red_any_tier_is_verified_red() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy"}),
            &failed_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedRed
        );

        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy"}),
            &failed_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedRed
        );
    }

    #[test]
    fn rerun_slow_green_after_fast_red_clears() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy"}),
            &failed_result(),
            false,
        );
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedGreen
        );
    }

    #[test]
    fn unverified_unchanged_in_all_modes() {
        for mode in [GateMode::Off, GateMode::Advisory, GateMode::Blocking] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            assert_eq!(g.status(mode), VerificationStatus::Unverified);
        }
    }

    /// dirge-uw2l.3: a green check says nothing about code edited AFTER
    /// it. In tiered modes that reads as unverified again; off mode keeps
    /// its latched green (byte-identical).
    #[test]
    fn green_goes_stale_when_edits_follow_it() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedGreen
        );

        // One more code edit — the green no longer covers the tree.
        g.record_outcome("edit", &json!({"path": "src/b.rs"}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::Unverified,
            "a post-green edit is uncovered"
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedGreen,
            "off mode keeps the legacy latched green"
        );

        // Re-running the suite clears it again.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedGreen
        );
    }

    /// A doc-only edit after a green check must NOT invalidate it — only
    /// code counts, same rule as the `edited_code` precondition.
    #[test]
    fn doc_edit_after_green_does_not_go_stale() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        g.record_outcome("write", &json!({"path": "README.md"}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Advisory),
            VerificationStatus::VerifiedGreen
        );
    }

    #[test]
    fn edits_since_verify_counts_code_edits_only() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome("write", &json!({"path": "src/b.rs"}), &ok_result(), false);
        g.record_outcome(
            "apply_patch",
            &json!({"operations": [{"type": "update", "path": "src/c.rs"}]}),
            &ok_result(),
            false,
        );
        g.record_outcome(
            "edit_minified",
            &json!({"path": "src/d.rs"}),
            &ok_result(),
            false,
        );
        g.record_outcome("write", &json!({"path": "README.md"}), &ok_result(), false);
        g.record_outcome("read", &json!({"path": "src/e.rs"}), &ok_result(), false);
        assert_eq!(g.edits_since_verify(), 4);
    }

    /// dirge-uw2l.7: a batched multi-file `apply_patch` counts once PER CODE
    /// FILE, not once per call. Found by an end-to-end run against a real
    /// model: asked to change four files, it emitted a single `apply_patch`
    /// with four operations, which registered as one edit — so the mid-run
    /// threshold of three was effectively unreachable for any model that
    /// batches, which is most of the good ones.
    #[test]
    fn batched_apply_patch_counts_each_code_file() {
        let g = VerifierGate::new();
        g.record_outcome(
            "apply_patch",
            &json!({"operations": [
                {"type": "update", "path": "src/alpha.rs"},
                {"type": "update", "path": "src/beta.rs"},
                {"type": "update", "path": "src/gamma.rs"},
                {"type": "update", "path": "src/delta.rs"},
            ]}),
            &ok_result(),
            false,
        );
        assert_eq!(g.edits_since_verify(), 4, "four files, not one call");
    }

    /// Non-code operations in the same batch don't inflate the count, and a
    /// file patched twice in one call is one unverified file.
    #[test]
    fn batched_patch_ignores_docs_and_dedupes() {
        let g = VerifierGate::new();
        g.record_outcome(
            "apply_patch",
            &json!({"operations": [
                {"type": "update", "path": "src/alpha.rs"},
                {"type": "update", "path": "src/alpha.rs"},
                {"type": "update", "path": "README.md"},
                {"type": "update", "path": "Cargo.toml"},
            ]}),
            &ok_result(),
            false,
        );
        assert_eq!(g.edits_since_verify(), 1, "one distinct code file");
    }

    /// A single-path tool still counts as exactly one.
    #[test]
    fn single_file_edit_counts_once() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert_eq!(g.edits_since_verify(), 1);
    }

    #[test]
    fn edits_since_verify_resets_on_any_verification() {
        let g = VerifierGate::new();
        for _ in 0..3 {
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        }
        assert_eq!(g.edits_since_verify(), 3);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.edits_since_verify(), 0);
    }

    #[test]
    fn edits_since_verify_not_reset_by_non_verification_bash() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome("bash", &json!({"command": "ls -la"}), &ok_result(), false);
        assert_eq!(g.edits_since_verify(), 2);
    }

    // dirge-uw2l.4: the safe-state abort's "last green" marker keys off a
    // verified point that no later edit has invalidated. Mode-independent —
    // it must not latch in off mode the way `status(Off)` does.
    #[test]
    fn is_fresh_green_only_when_verified_and_unedited_since() {
        let g = VerifierGate::new();
        // An edit alone is not green — no verification has run.
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert!(!g.is_fresh_green(), "edit alone is not green");
        // A passing test reaches fresh green (edits_since_verify reset to 0).
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert!(
            g.is_fresh_green(),
            "passing test with no edits since is fresh green"
        );
        // An edit after green makes it stale — NOT fresh green.
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert!(!g.is_fresh_green(), "edit after green makes it stale");
        // A failing verification is not green either.
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &failed_result(),
            false,
        );
        assert!(!g.is_fresh_green(), "failing verification is not green");
    }

    /// Tiered variant of `nudge()` — passes the gate mode explicitly.
    fn tiered_nudge(gate: &VerifierGate, mode: GateMode) -> Option<String> {
        gate.check_before_finalize(mode)
            .into_iter()
            .next()
            .map(|m| match m {
                LoopMessage::User(u) => u.text_joined(),
                _ => panic!("expected user message"),
            })
    }

    #[test]
    fn off_mode_fast_only_finalize_is_silent() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        assert!(tiered_nudge(&g, GateMode::Off).is_none());
    }

    #[test]
    fn advisory_fast_only_finalize_escalates_once() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        let n = tiered_nudge(&g, GateMode::Advisory).expect("fast-only should escalate");
        assert!(n.contains(VERIFY_TAG), "escalation carries the tag: {n}");
        assert!(n.contains("full test suite"), "names the full suite: {n}");
        assert!(
            tiered_nudge(&g, GateMode::Advisory).is_none(),
            "advisory escalation is one-shot"
        );
    }

    #[test]
    fn blocking_fast_only_finalize_escalates_up_to_cap() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        assert!(tiered_nudge(&g, GateMode::Blocking).is_some());
        assert!(tiered_nudge(&g, GateMode::Blocking).is_some());
        assert!(
            tiered_nudge(&g, GateMode::Blocking).is_none(),
            "bounded by MAX_TIER_ESCALATIONS"
        );
    }

    #[test]
    fn slow_green_finalize_silent_all_modes() {
        for mode in [GateMode::Advisory, GateMode::Blocking] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome(
                "bash",
                &json!({"command": "cargo test"}),
                &ok_result(),
                false,
            );
            assert!(tiered_nudge(&g, mode).is_none());
        }
    }

    #[test]
    fn red_and_unverified_nudges_unchanged_by_mode() {
        for mode in [GateMode::Off, GateMode::Advisory, GateMode::Blocking] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome(
                "bash",
                &json!({"command": "cargo test"}),
                &failed_result(),
                false,
            );
            assert!(
                tiered_nudge(&g, mode).is_some_and(|n| n.contains("failed")),
                "red nudge in {mode:?}"
            );

            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            assert!(
                tiered_nudge(&g, mode).is_some_and(|n| n.contains("didn't run the tests")),
                "unverified nudge in {mode:?}"
            );
        }
    }

    /// The legacy one-shot nudge and the tier escalation have separate
    /// budgets: an unverified nudge spent earlier must not consume the
    /// full-suite escalation when the run later reaches fast-green.
    #[test]
    fn unverified_nudge_does_not_spend_the_escalation_budget() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        assert!(tiered_nudge(&g, GateMode::Advisory).is_some());
        g.record_outcome(
            "bash",
            &json!({"command": "cargo check"}),
            &ok_result(),
            false,
        );
        let n = tiered_nudge(&g, GateMode::Advisory)
            .expect("fast-green escalation still fires after the legacy nudge");
        assert!(n.contains("full test suite"), "{n}");
        assert!(tiered_nudge(&g, GateMode::Advisory).is_none());
    }

    // ---- dirge-w2de: project gate (config-driven real CI command) ----

    /// The same gate written two ways — env-prefixed with a quoted value
    /// vs flag placement after the subcommand — must normalize to the
    /// same ("cargo", "clippy") signature.
    #[test]
    fn gate_signature_handles_env_prefix_and_flag_placement() {
        assert_eq!(
            gate_signature(r#"RUSTFLAGS="-D warnings" cargo clippy --all-targets"#),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
        assert_eq!(
            gate_signature("cargo clippy --all-targets -- -D warnings"),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
    }

    /// Quoted env values stay ONE token (`CC="ccache gcc"` is an env
    /// assignment, not three words), and flags before the subcommand are
    /// skipped.
    #[test]
    fn gate_signature_quoted_env_values_and_flags_before_subcommand() {
        assert_eq!(
            gate_signature(r#"CC="ccache gcc" RUSTFLAGS="-D warnings" cargo clippy"#),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
        assert_eq!(
            gate_signature("cargo --locked --offline clippy --all-targets"),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
    }

    /// A bare program has no subcommand; empty input has no signature.
    #[test]
    fn gate_signature_bare_program_and_empty() {
        assert_eq!(
            gate_signature("make"),
            Some(GateSignature {
                program: "make".into(),
                subcommand: None,
            })
        );
        assert_eq!(gate_signature(""), None);
        assert_eq!(gate_signature("   "), None);
    }

    /// A gate SPECIFICATION naming a chain takes its last segment:
    /// `cargo check && cargo clippy` specifies the clippy gate.
    #[test]
    fn gate_signature_spec_chain_takes_last_segment() {
        assert_eq!(
            gate_signature("cargo check && cargo clippy --all-targets"),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
        assert_eq!(
            gate_signature("cargo fmt\ncargo clippy --all-targets"),
            Some(GateSignature {
                program: "cargo".into(),
                subcommand: Some("clippy".into()),
            })
        );
    }

    /// An OBSERVED command yields every segment's signature, not just the
    /// last. The caller only consults this for a command that passed, and
    /// under `&&` that means every segment passed — so a gate satisfied by
    /// an early segment must still count. Taking only the last segment
    /// left a run reporting fast-green-only after the real gate had run
    /// clean.
    #[test]
    fn observed_chain_yields_every_segment_signature() {
        let sigs = gate_signatures("cargo clippy --all-targets && cargo test");
        assert_eq!(
            sigs,
            vec![
                GateSignature {
                    program: "cargo".into(),
                    subcommand: Some("clippy".into()),
                },
                GateSignature {
                    program: "cargo".into(),
                    subcommand: Some("test".into()),
                },
            ]
        );
    }

    /// End to end: the gate ran green as the FIRST link of a chain. The
    /// last-segment reading would have downgraded this to FastGreenOnly.
    #[test]
    fn gate_green_in_first_chain_segment_is_verified_green() {
        let g = VerifierGate::with_project_gate(Some("cargo clippy".into()));
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy --all-targets && cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    /// The happy path: the configured gate command ran and PASSED →
    /// VerifiedGreen.
    #[test]
    fn configured_gate_run_green_is_verified_green() {
        let g = VerifierGate::with_project_gate(Some(
            r#"RUSTFLAGS="-D warnings" cargo clippy --all-targets"#.into(),
        ));
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": r#"RUSTFLAGS="-D warnings" cargo clippy --all-targets"#}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    /// The bug this fixes: only a weaker command (`cargo test`) ran green,
    /// never the gate. The green is a FALSE green — it says nothing about
    /// the tree CI actually builds. Opt-in (`verification_command` set),
    /// so this applies even in off mode; unconfigured users are untouched.
    #[test]
    fn configured_gate_not_run_downgrades_green_to_fast_only() {
        let g = VerifierGate::with_project_gate(Some("cargo clippy --all-targets".into()));
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::FastGreenOnly);
    }

    /// A FAILING gate run must not set `ran_project_gate`; the red
    /// dominates, and a later lesser green cannot resurrect a fake green.
    #[test]
    fn failing_gate_run_never_sets_ran_project_gate() {
        let g = VerifierGate::with_project_gate(Some("cargo clippy --all-targets".into()));
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy --all-targets"}),
            &failed_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedRed);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            true,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedRed);
    }

    /// No `verification_command` configured: off-mode status is
    /// byte-identical to before — a passed `cargo test` is VerifiedGreen.
    #[test]
    fn unconfigured_gate_keeps_off_mode_byte_identical() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/auth.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    // ── dirge-w2de part 2: what CI actually runs, as advice ────────────────

    /// Per-test scratch dir, following the convention in worktree_probe.rs
    /// (process + thread id) rather than adding a dev-dependency for it.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dirge-ci-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wf(dir: &std::path::Path, name: &str, body: &str) {
        let w = dir.join(".github").join("workflows");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join(name), body).unwrap();
    }

    #[test]
    fn no_github_dir_yields_nothing() {
        let t = scratch("nogh");
        assert!(ci_verification_commands(t.as_path()).is_empty());
    }

    #[test]
    fn one_line_and_block_scalar_run_steps_both_parse() {
        let t = scratch("block");
        wf(
            t.as_path(),
            "ci.yml",
            "jobs:\n  a:\n    steps:\n      - run: cargo clippy --all-targets\n             \n  b:\n    steps:\n      - run: |\n          cargo nextest run\n",
        );
        let got = ci_verification_commands(t.as_path());
        assert!(got.iter().any(|c| c.contains("clippy")), "{got:?}");
        assert!(got.iter().any(|c| c.contains("nextest")), "{got:?}");
    }

    #[test]
    fn non_verification_steps_are_ignored() {
        let t = scratch("nonverif");
        wf(
            t.as_path(),
            "ci.yml",
            "steps:\n  - run: actions/checkout@v4\n  - run: echo hello\n  - run: cargo clippy\n",
        );
        let got = ci_verification_commands(t.as_path());
        assert_eq!(got.len(), 1, "only the real check survives: {got:?}");
        assert!(got[0].contains("clippy"));
    }

    /// Interpolated commands are skipped — the expansion is unknown, and a
    /// half-substituted command is not something to hand a model as fact.
    #[test]
    fn interpolated_commands_are_skipped() {
        let t = scratch("interp");
        wf(
            t.as_path(),
            "ci.yml",
            "steps:\n  - run: cargo build ${{ matrix.features }}\n  - run: cargo clippy\n",
        );
        let got = ci_verification_commands(t.as_path());
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("clippy"));
    }

    /// Deduped by SIGNATURE: three clippy invocations differing only in
    /// feature flags are one instruction to the reader, not three.
    #[test]
    fn same_signature_is_reported_once() {
        let t = scratch("samesig");
        wf(
            t.as_path(),
            "ci.yml",
            "steps:\n  - run: cargo clippy --all-targets -- -D warnings\n             \n  - run: cargo clippy --features sandbox-microvm --all-targets -- -D warnings\n",
        );
        assert_eq!(ci_verification_commands(t.as_path()).len(), 1);
    }

    /// The motivating case. This repo's own CI is what the original incident
    /// happened against: an agent ran `cargo test`, saw it pass, reported
    /// success, and clippy had six hard errors. The advisory has to name
    /// clippy, or it does not address the thing it was written for.
    #[test]
    fn this_repos_real_ci_names_clippy() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let got = ci_verification_commands(root);
        assert!(
            got.iter().any(|c| c.contains("clippy")),
            "must surface the gate the motivating incident missed: {got:?}"
        );
        // And the hint reads as a sentence naming it.
        let hint = ci_hint(&got);
        assert!(hint.contains("clippy"), "{hint}");
        assert!(hint.contains("CI runs"), "{hint}");
    }

    /// Empty list → byte-identical nudge. A project with no CI must not get a
    /// trailing fragment.
    #[test]
    fn no_ci_commands_leaves_the_nudge_unchanged() {
        assert_eq!(ci_hint(&[]), "");
        let g = VerifierGate::with_project_gate_and_ci(None, Vec::new());
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        let n = nudge(&g).expect("nudge fires");
        assert_eq!(n, VERIFY_NUDGE, "no CI → the original text, exactly");
    }

    /// With CI commands, the nudge names them — this is the whole payload.
    #[test]
    fn nudge_names_the_ci_commands() {
        let g = VerifierGate::with_project_gate_and_ci(
            None,
            vec!["cargo clippy --all-targets -- -D warnings".to_string()],
        );
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        let n = nudge(&g).expect("nudge fires");
        assert!(n.starts_with(VERIFY_NUDGE), "original text is preserved");
        assert!(n.contains("cargo clippy"), "and CI is named: {n}");
    }

    // ── A gate that cannot fail for the reason that matters ────────────────
    //
    // Measured before the fix: `cargo test || true`, `cargo clippy | tail -2`
    // and `cargo test; echo done` ALL latched VerifiedGreen. The verifier reads
    // the exit status of the whole command, and in each of those the status
    // belongs to something other than the check.

    #[test]
    fn masking_shapes_are_detected() {
        for cmd in [
            "cargo clippy --all-targets | tail -2",
            "cargo test || true",
            "cargo test || echo ignored",
            "cargo test; echo done",
            "cargo test &",
            "cargo clippy 2>&1 | head -20",
            "cargo test\necho done",
        ] {
            assert!(masks_failure(cmd), "should be detected as masking: {cmd}");
        }
    }

    /// `&&` short-circuits, so a failing left side IS the exit status — not
    /// masking. Redirections carry no pipe. Over-detecting here would decline
    /// perfectly good verifications and nag forever, which is the failure mode
    /// this whole area is trying to avoid.
    #[test]
    fn non_masking_shapes_are_not_flagged() {
        for cmd in [
            "cargo clippy --all-targets -- -D warnings",
            "cargo fmt --all --check && cargo clippy --all-targets",
            "cargo test 2>&1",
            "RUSTFLAGS=\"-D warnings\" cargo clippy --all-targets",
            "cargo test;",
            "cargo test\n",
            "cargo test && \\\necho done",
        ] {
            assert!(!masks_failure(cmd), "must not be flagged: {cmd}");
        }
    }

    /// The bug, end to end: a masked command reporting success must NOT latch
    /// green. Status stays Unverified so the gate asks again.
    #[test]
    fn masked_success_does_not_latch_green() {
        for cmd in [
            "cargo test || true",
            "cargo clippy --all-targets | tail -2",
            "cargo test; echo done",
        ] {
            let g = VerifierGate::new();
            g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
            g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
            assert_eq!(
                g.status(GateMode::Off),
                VerificationStatus::Unverified,
                "a masked success proves nothing and must not read as green: {cmd}"
            );
        }
    }

    /// The bug, end to end (dirge-1elu.3): a newline-chained validation
    /// block whose exit status belongs to a trailing `echo` must NOT latch
    /// green — every assertion may have failed while the status is honestly
    /// 0. Same treatment as the `;` shape: success is not recorded.
    #[test]
    fn newline_chained_validation_block_does_not_latch_green() {
        let cmd = "diff expected.txt actual.txt\ncmp -s a.bin b.bin\ntest -f out/report.json\necho \"all checks passed\"";
        assert!(masks_failure(cmd), "the block's status is the echo's");
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path":"src/a.rs"}), &ok_result(), false);
        g.record_outcome("bash", &json!({"command": cmd}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "a masked success proves nothing and must not read as green"
        );
    }

    /// A masked command that still reports FAILURE is trustworthy in that
    /// direction — something in the chain genuinely failed — so the red is
    /// recorded. Declining it would let a real failure go unreported.
    #[test]
    fn masked_failure_is_still_recorded_as_red() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test | tail -2"}),
            &failed_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedRed);
    }

    /// A masked multi-line command that still reports FAILURE is
    /// trustworthy in that direction — something in the chain genuinely
    /// failed — so the red is recorded (dirge-1elu.3, same asymmetry as the
    /// `;` shape).
    #[test]
    fn masked_newline_failure_is_still_recorded_as_red() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path":"src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command":"cargo test\necho done"}),
            &ok_result(),
            true,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedRed);
    }

    /// An unmasked command is unaffected — the common path stays green.
    #[test]
    fn unmasked_success_still_latches_green() {
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": "cargo clippy --all-targets -- -D warnings"}),
            &ok_result(),
            false,
        );
        assert_eq!(g.status(GateMode::Off), VerificationStatus::VerifiedGreen);
    }

    /// A masked command must not reset the mid-run edit counter either — the
    /// model did not actually establish anything, so the fast-verify reminder
    /// should still be on its way.
    #[test]
    fn masked_command_does_not_clear_edits_since_verify() {
        let g = VerifierGate::new();
        for i in 0..3 {
            g.record_outcome(
                "edit",
                &json!({ "path": format!("src/f{i}.rs") }),
                &ok_result(),
                false,
            );
        }
        let before = g.edits_since_verify();
        g.record_outcome(
            "bash",
            &json!({"command": "cargo test || true"}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.edits_since_verify(),
            before,
            "a masked check did not verify anything, so the counter must stand"
        );
    }

    // ── dirge-1elu.2: decline green from agent-authored scripts ────────────
    // A model can write its own validator, run it, watch it exit 0, and
    // satisfy the gate without ever running the project's tests. A script
    // authored THIS RUN proves nothing (generator and validator can share
    // the same wrong assumptions), so the green is declined — except a
    // self-authored script reporting FAILURE, which is still trustworthy
    // in that direction.

    static SCRIPT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A unique temp script path (per test) so parallel tests never share
    /// a file. The basename carries a marker word so the command registers
    /// as verification via the script-name path.
    fn script_path(marker: &str) -> std::path::PathBuf {
        let n = SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("dirge-elu2-{}-{n}-{marker}.sh", std::process::id()))
    }

    /// Pin a file's mtime to before this gate's run start, so the
    /// modified-files registry is the ONLY source that can answer
    /// "authored".
    fn predate(file: &std::path::Path) {
        std::fs::File::open(file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }

    /// The bug, reproduced: the agent writes `check.sh` THIS run (in the
    /// modified-files registry) and runs it; exit 0 must NOT read as green.
    #[test]
    fn agent_authored_script_does_not_latch_green() {
        let script = script_path("check");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        predate(&script); // only the registry can answer "authored"
        crate::agent::tools::modified::mark_modified(&script);

        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "a script authored this run proves nothing — must not read as green"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// The bash-authored path: the file is only detectable by mtime — a
    /// `cat > check.sh` heredoc the registry never sees.
    #[test]
    fn bash_authored_script_does_not_latch_green() {
        let script = script_path("validate");
        let g = VerifierGate::new(); // gate FIRST: mtime must be >= run start
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(
            crate::agent::tools::modified::MODIFIED_FILES
                .lock()
                .is_ok_and(|s| !s.contains(&script))
        );

        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "a script created by bash this run must not read as green"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// Over-detection guard: a legitimate repo script that PREDATES the
    /// run (old mtime, not in the registry) still counts.
    #[test]
    fn pre_existing_repo_script_still_latches_green() {
        let script = script_path("run-tests");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        predate(&script);
        let g = VerifierGate::new(); // gate AFTER: the script predates the run

        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedGreen,
            "a repo script that predates the run must still count"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// Wrapper, not proxy: a self-authored script whose command line ALSO
    /// carries a real word marker stays recognized regardless of who wrote
    /// it.
    #[test]
    fn self_authored_script_with_real_marker_still_counts() {
        let script = script_path("check");
        std::fs::write(&script, "#!/bin/sh\ncargo test\nexit 0\n").unwrap();
        predate(&script);
        crate::agent::tools::modified::mark_modified(&script);

        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        let command = format!("{} && cargo test", script.to_string_lossy());
        g.record_outcome("bash", &json!({"command": command}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedGreen,
            "a real word marker is present — wrapper, not proxy"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// The asymmetry: a self-authored script reporting FAILURE is still
    /// trustworthy in that direction — record the red.
    #[test]
    fn agent_authored_script_failure_is_still_red() {
        let script = script_path("check");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        crate::agent::tools::modified::mark_modified(&script);

        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            true,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedRed,
            "a self-authored script reporting failure is trustworthy"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// Unknown provenance: the path doesn't exist on disk and is not in the
    /// registry — behave exactly as today.
    #[test]
    fn script_with_unknown_provenance_behaves_as_today() {
        let script = script_path("check"); // never written to disk
        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedGreen,
            "can't tell who wrote it — degrade toward today's behaviour"
        );
    }

    /// The same-call heredoc+exec shape: `cat > check.sh <<'EOF'` writes the
    /// script and a later line of the SAME bash call runs it. The newline
    /// split lands the exec word in its own segment, so it must be caught
    /// like any other script-name run — the mtime witness (gate first) does
    /// the detecting.
    #[test]
    fn heredoc_then_execute_in_one_bash_call_is_caught() {
        let script = script_path("check");
        let g = VerifierGate::new(); // gate FIRST: mtime is the only witness
        let command = format!(
            "cat > {} <<'EOF'\nexit 0\nEOF\n{}",
            script.to_string_lossy(),
            script.to_string_lossy()
        );
        // The tool call wrote the file; the registry never sees it.
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome("bash", &json!({"command": command}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "heredoc-write + same-call execute must not read as green"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// A RELATIVE script word (`../check.sh`) resolves against the run's
    /// cwd — the same cwd bash inherits — so the registry entry, recorded
    /// under the same relative spelling, must match. Pins the
    /// canonicalize-against-run-cwd behaviour.
    #[test]
    fn relative_script_path_resolves_against_the_run_cwd() {
        let rel = std::path::Path::new("..").join(format!(
            "dirge-elu2-{}-{}.sh",
            std::process::id(),
            SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&rel, "#!/bin/sh\nexit 0\n").unwrap();
        predate(&rel); // only the registry can answer "authored"
        crate::agent::tools::modified::mark_modified(&rel);

        let g = VerifierGate::new();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": rel.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "a relative-path script authored this run must not read as green"
        );
        let _ = std::fs::remove_file(&rel);
    }

    /// The accepted over-detection trade: a pre-existing script whose mtime
    /// is refreshed DURING the run (touch, chmod, a build regenerating it)
    /// is indistinguishable from one bash created this run, so its green is
    /// declined too. Fail-safe direction — a missed decline would let a
    /// proxy validator through, which is the worse failure.
    #[test]
    fn touched_during_run_script_is_treated_as_authored() {
        let script = script_path("run-tests");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let g = VerifierGate::new(); // run starts
        predate(&script); // the script predates the run…
        // …but something touches it mid-run (touch / build regen).
        std::fs::File::open(&script)
            .unwrap()
            .set_modified(std::time::SystemTime::now())
            .unwrap();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        g.record_outcome(
            "bash",
            &json!({"command": script.to_string_lossy().to_string()}),
            &ok_result(),
            false,
        );
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "mtime at-or-after run start reads as authored this run"
        );
        let _ = std::fs::remove_file(&script);
    }

    /// dirge-1elu.2 follow-up (cd-fold): `cd <dir> && ./check.sh` resolves the
    /// script against the cd'd directory — a proxy validator authored this
    /// run under that directory must NOT latch green. The run cwd alone would
    /// miss it (under-detection, the failure direction that matters).
    #[test]
    fn cd_folded_script_authored_this_run_does_not_latch_green() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-elu2-cd-{}-{}",
            std::process::id(),
            SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("check.sh");
        let g = VerifierGate::new(); // gate FIRST: mtime is the only witness
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        let command = format!("cd {} && ./check.sh", dir.to_string_lossy());
        g.record_outcome("bash", &json!({"command": command}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::Unverified,
            "a cd'd-into proxy validator authored this run must not read as green"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Over-detection guard for the cd-fold: the same shape with a script
    /// that predates the run still latches green.
    #[test]
    fn cd_folded_pre_existing_script_still_latches_green() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-elu2-cd2-{}-{}",
            std::process::id(),
            SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("run-tests.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        predate(&script);
        let g = VerifierGate::new(); // gate AFTER: the script predates the run
        g.record_outcome("edit", &json!({"path": "src/a.rs"}), &ok_result(), false);
        let command = format!("cd {} && ./run-tests.sh", dir.to_string_lossy());
        g.record_outcome("bash", &json!({"command": command}), &ok_result(), false);
        assert_eq!(
            g.status(GateMode::Off),
            VerificationStatus::VerifiedGreen,
            "a pre-existing repo script behind a cd must still count"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    // ── dirge-1elu.7: session-keyed run-status handoff ──────────────

    /// The producer/consumer pair actually connects, and the value that
    /// comes out is the one that went in.
    #[test]
    fn run_status_round_trips_for_its_own_session() {
        record_run_verification(
            Some("s-round-trip"),
            Some(VerificationStatus::VerifiedGreen),
        );
        assert_eq!(
            take_run_verification("s-round-trip"),
            Some(VerificationStatus::VerifiedGreen)
        );
    }

    /// The property the whole session-keyed design exists for: a subagent or
    /// MCP-delegate run sharing the process must not hand its status to a
    /// different session.
    #[test]
    fn run_status_never_leaks_across_sessions() {
        record_run_verification(Some("s-owner"), Some(VerificationStatus::VerifiedRed));
        assert_eq!(take_run_verification("s-other"), None);
        // The owner's entry is untouched by the foreign read.
        assert_eq!(
            take_run_verification("s-owner"),
            Some(VerificationStatus::VerifiedRed)
        );
    }

    /// Taking is consuming, so a second post-session pass in the same session
    /// reads "could not tell" rather than re-crediting a stale green.
    #[test]
    fn run_status_is_taken_not_copied() {
        record_run_verification(Some("s-once"), Some(VerificationStatus::VerifiedGreen));
        assert!(take_run_verification("s-once").is_some());
        assert_eq!(take_run_verification("s-once"), None);
    }

    /// A run that verified nothing CLEARS its slot. Without this a later run
    /// with no verification would inherit the previous run's green — the
    /// stale-read failure this handoff exists to avoid.
    #[test]
    fn recording_none_clears_a_prior_status() {
        record_run_verification(Some("s-clear"), Some(VerificationStatus::VerifiedGreen));
        record_run_verification(Some("s-clear"), None);
        assert_eq!(take_run_verification("s-clear"), None);
    }

    /// No session id (headless paths that never set one) records nothing
    /// rather than colliding on a shared empty key.
    #[test]
    fn missing_session_id_records_nothing() {
        record_run_verification(None, Some(VerificationStatus::VerifiedGreen));
        assert_eq!(take_run_verification(""), None);
    }
}
