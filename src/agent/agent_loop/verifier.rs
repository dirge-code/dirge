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
    /// (`verification_tiers` ≠ off): off-mode `status()` never returns
    /// it — fast-only coverage collapses to [`VerificationStatus::VerifiedGreen`]
    /// (dirge-uw2l.2).
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
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
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
                inner.edits_since_verify = inner.edits_since_verify.saturating_add(1);
            }
            "bash" => {
                let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if is_verification_command(command) {
                    inner.ran_verification = true;
                    // Latest outcome wins.
                    let failed = is_error || result_indicates_failure(result);
                    inner.verification_failed = failed;
                    // Any verification attempt clears the mid-run counter —
                    // the model did go and check, whatever the outcome.
                    inner.edits_since_verify = 0;
                    // Tier coverage only counts when the command PASSED.
                    if !failed {
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
                return vec![LoopMessage::User(UserMessage::text(text))];
            }
        }
        if inner.is_fast_green_only() && inner.escalations < escalation_cap(mode) {
            inner.escalations += 1;
            return vec![LoopMessage::User(UserMessage::text(FULL_SUITE_NUDGE))];
        }
        Vec::new()
    }
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

fn segment_is_verification(segment: &str) -> bool {
    let tokens: Vec<String> = segment
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    if tokens.iter().any(|t| NON_VERIFY.contains(&t.as_str())) {
        return false;
    }
    // Whole-word markers. A `--check`-style flag is its dash-stripped
    // word, so `prettier --check .` and `cmake --build` register.
    if tokens
        .iter()
        .any(|t| WORD_MARKERS.contains(&t.trim_start_matches('-')))
    {
        return true;
    }
    if tokens
        .windows(2)
        .any(|w| PAIR_MARKERS.contains(&(w[0].as_str(), w[1].as_str())))
    {
        return true;
    }
    // The command word may be a path to a repo script: match markers
    // inside its basename, split on `-`/`_`/`.`, accepting a plural form,
    // so `./run-tests.sh` and `scripts/lint.sh` register. Only the
    // executed word gets this treatment — an argument like `ls tests/`
    // must not count (dirge-eg37).
    command_word(&tokens).is_some_and(script_name_is_verification)
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
    let Some(obj) = args.as_object() else {
        return false;
    };
    let mut paths: Vec<&str> = Vec::new();
    for key in ["path", "file_path", "file"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            paths.push(s);
        }
    }
    if let Some(ops) = obj.get("operations").and_then(|v| v.as_array()) {
        for op in ops {
            if let Some(s) = op.get("path").and_then(|v| v.as_str()) {
                paths.push(s);
            }
        }
    }
    paths.iter().any(|p| is_code_path(p))
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
}
