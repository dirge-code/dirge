//! Deterministic claim/evidence gate (dirge-d0e5.2).
//!
//! At finalization, a model-visible one-shot nudge fires when the final
//! answer makes a SPECIFIC claim the run's evidence does not support:
//!
//! - **Unsupported verification claim** — the answer asserts a verification
//!   result ("5001 passed", "compiles", "clippy clean") that no observed
//!   build/test command of the matching kind supports. The evidence check is
//!   KIND-MATCHED (dirge-lavc): a test-count claim requires an observed TEST
//!   command, a build/lint claim an observed BUILD/LINT command. A green
//!   `cargo build` cannot support "N passed" — the false green this gate
//!   exists to catch — and "compiles" is only ever judged against a build
//!   that really ran, so a true "Compiles." stays silent.
//! - **Unsupported change claim** — the answer asserts having
//!   applied/fixed/changed something while zero files were mutated this run.
//!
//! Deliberately deterministic, no LLM: a pattern over "N passed" conjoined
//! with zero observed verifications cannot be talked out of or invent
//! accusations the way a judging model can. The conjunction is the control —
//! per docs/verification-discipline.md, "Over-detecting would decline good
//! verifications and nag forever, which is the same harm pointed the other
//! way."
//!
//! Carve-outs, deliberately narrow: output the model is QUOTING or
//! attributing to another actor (a pasted CI log, "CI reported", "you said")
//! is not the model's own assertion about this run, so it does not fire. Do
//! not widen them to catch more — a missed fabrication is recoverable; a
//! gate that nags on honest work gets turned off and then catches nothing.

use super::types::GateMode;

/// Tag prefixing the model-visible nudge, so it is greppable in transcripts.
pub(crate) const CLAIM_GATE_TAG: &str = "[claim-check]";

/// Per-run nudge ceiling, by mode.
///
/// `advisory` is one-shot: say it once, and a model that ignores it is not
/// nagged forever. `blocking` re-enters up to three times, so a run that keeps
/// finalizing on an unsupported claim keeps being asked — bounded, because a
/// model that cannot satisfy the check after three tries will not on the
/// fourth. `off` never fires.
///
/// Without this the two modes were byte-identical and the config surface
/// advertised a distinction that did not exist.
pub(crate) fn claim_nudge_cap(mode: GateMode) -> u8 {
    match mode {
        GateMode::Off => 0,
        GateMode::Advisory => 1,
        GateMode::Blocking => 3,
    }
}

/// Which unsupported claim fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimKind {
    Verification,
    Change,
}

impl ClaimKind {
    /// Body of the nudge (the tag is prefixed by the caller). Asks the model
    /// to correct the claim or actually do the work — never a verdict, so it
    /// cannot cause a false green.
    pub(crate) fn nudge_text(self) -> &'static str {
        match self {
            ClaimKind::Verification => {
                "Your final message asserts a verification result (a test count like \
                 \"N passed\" or a named gate like \"clippy clean\"/\"compiles\"), but no \
                 build/test command of the matching kind ran this run — a test-count claim \
                 needs a test command, a build/lint claim needs a build/lint command. Either \
                 actually run the check and report its real output, or remove the unsupported \
                 claim."
            }
            ClaimKind::Change => {
                "Your final message says you changed or fixed something, but no files were \
                 mutated this run. Either make the change you claim, or correct the claim \
                 so it matches what actually happened."
            }
        }
    }
}

/// The claims [`scan_final_answer`] found in the final answer text. Evidence
/// is applied separately by [`unsupported_claims`], so the scanner stays a
/// pure function of the text.
///
/// Verification claims are split by KIND (dirge-lavc) because the kinds
/// demand different evidence: a test-result claim ("N passed", "tests pass")
/// can only be supported by an observed test command, a build/lint-result
/// claim ("compiles", "clippy clean") only by an observed build/lint command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Claims {
    pub test_claim: bool,
    pub build_or_lint_claim: bool,
    /// A verification claim naming no kind of command ("exit 0"). Satisfied by
    /// ANY observed verification — see [`claims_generic_verification`].
    pub generic_claim: bool,
    pub change_claim: bool,
}

/// Scan the model's final answer for concrete claims about what it ran and
/// what it changed. Quoted/attributed output is stripped first (the
/// carve-outs), so a pasted CI log or a "CI reported …" sentence never
/// counts as the model's own claim.
pub(crate) fn scan_final_answer(text: &str) -> Claims {
    let unquoted = strip_quoted(text);
    let sentences = split_sentences(&unquoted);
    let mut claims = Claims::default();
    for sentence in sentences {
        if sentence_attributes_to_another_actor(sentence) {
            continue;
        }
        claims.test_claim |= claims_test_result(sentence);
        claims.build_or_lint_claim |= claims_build_or_lint_result(sentence);
        claims.generic_claim |= claims_generic_verification(sentence);
        claims.change_claim |= claims_change(sentence);
    }
    claims
}

/// Which (if any) claim the observed evidence fails to support. Evidence is
/// the verifier's [`crate::agent::agent_loop::verifier::VerifierGate::observed_commands`] —
/// every build/test command observed this run, with whether it failed — and
/// it is matched to the claim by KIND (dirge-lavc): a test-result claim needs
/// an observed TEST command, a build/lint-result claim an observed
/// BUILD/LINT command. The pass/fail flag is deliberately not consulted: the
/// red/green verdict belongs to the verifier's own status machinery, and a
/// command that ran at all is evidence its kind was attempted. This gate
/// biases toward under-detecting.
pub(crate) fn unsupported_claims(
    claims: &Claims,
    observed: &[(String, bool)],
    files_mutated: usize,
) -> Option<ClaimKind> {
    let (ran_test, ran_build_or_lint) = observed_kinds(observed);
    if (claims.test_claim && !ran_test)
        || (claims.build_or_lint_claim && !ran_build_or_lint)
        // dirge-hwk9.6: a kind-agnostic claim needs SOME verification, not a
        // particular one.
        || (claims.generic_claim && !ran_test && !ran_build_or_lint)
    {
        return Some(ClaimKind::Verification);
    }
    if claims.change_claim && files_mutated == 0 {
        return Some(ClaimKind::Change);
    }
    None
}

/// The kind of evidence a verification command provides. A verification TIER
/// (verifier.rs) is a COST axis; this is a COVERAGE axis — which claims the
/// command can support. The two are not interchangeable: `cargo build` is
/// Slow (costly) AND build/lint kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    /// Runs tests: `cargo test`, `pytest`, `go test`, `npm test`.
    Test,
    /// Builds, checks, lints, or formats: `cargo build`, `tsc`, `eslint`.
    BuildOrLint,
}

/// Which evidence kinds the observed commands provide, as (test, build/lint).
/// A command provides a kind when ANY of its segments is that kind, so
/// `cargo fmt && cargo test` counts as both. A command that is verification
/// but whose kind is unrecognized (rare — a bare script path) provides
/// neither, and cannot support a claim.
fn observed_kinds(commands: &[(String, bool)]) -> (bool, bool) {
    let mut ran_test = false;
    let mut ran_build_or_lint = false;
    for (command, _failed) in commands {
        for segment in command.split(['&', '|', ';', '\n']) {
            match segment_kind(segment) {
                Some(CommandKind::Test) => ran_test = true,
                Some(CommandKind::BuildOrLint) => ran_build_or_lint = true,
                None => {}
            }
        }
    }
    (ran_test, ran_build_or_lint)
}

/// The evidence kind of a single shell segment, or `None` when it is not a
/// recognized build/test/lint command. Mirrors the verifier's recognition
/// vocabulary (verifier.rs) on the kind axis.
/// Peel an interpreter or package-runner prefix off a command, returning the
/// tool that actually runs and its remaining arguments (dirge-hwk9.3).
///
/// Three shapes, and the distinction between the first two is the whole point:
///
/// - `python -m pytest …` — `-m` names a MODULE to run, so `pytest` is the
///   tool. This is the documented way to run pytest against the current
///   interpreter and is what the model reached for unprompted.
/// - `python script.py`, `python -c '…'` — the interpreter is running a
///   script, not a known tool. Left alone, so it classifies as `None` rather
///   than becoming whatever the script is named.
/// - `npx jest`, `poetry run pytest`, `uv run pytest` — a runner whose first
///   non-flag argument is the tool.
///
/// Peeling is single-step by design: `poetry run python -m pytest` peels to
/// `python -m pytest` and then this is not applied again. Recursing would be
/// more thorough and would also let a long prefix chain walk to an argument
/// that is not a command at all, which is the direction that produces a false
/// green.
fn strip_runner_prefix<'a>(tokens: &'a [&'a str]) -> Option<(&'a &'a str, &'a [&'a str])> {
    let (command, args) = tokens.split_first()?;
    let base = command.rsplit('/').next().unwrap_or(command);
    // `python -m <module>`: the module is the tool.
    let is_interpreter = base == "python" || base == "python3" || base.starts_with("python3.");
    if is_interpreter
        && let Some(pos) = args.iter().position(|t| *t == "-m")
        && let Some(module) = args.get(pos + 1)
    {
        return Some((module, &args[pos + 1..]));
    }
    // `npx <tool>`, `poetry run <tool>`, `uv run <tool>`, `pipenv run <tool>`.
    let runner_takes_next = match base {
        "npx" | "bunx" | "pnpx" => true,
        "poetry" | "uv" | "pipenv" | "rye" | "hatch" => args.first().is_some_and(|a| *a == "run"),
        _ => false,
    };
    if runner_takes_next
        && let Some(pos) = args.iter().position(|t| !t.starts_with('-') && *t != "run")
    {
        return Some((&args[pos], &args[pos + 1..]));
    }
    Some((command, args))
}

fn segment_kind(segment: &str) -> Option<CommandKind> {
    // Drop `VAR=value` prefixes so `RUST_LOG=debug cargo test` classifies.
    let tokens: Vec<&str> = segment
        .split_whitespace()
        .filter(|t| !t.contains('='))
        .collect();
    // dirge-hwk9.3: step past an interpreter or runner prefix, so the tool that
    // actually runs is the one classified. `python3 -m pytest` used to classify
    // as `python3` and fall through to `None` — the verifier recognised the
    // same command (its markers match ANY token) and the two gates then told
    // the model opposite things about whether it had run the tests.
    let (command, args) = strip_runner_prefix(&tokens)?;
    let base = command.rsplit('/').next().unwrap_or(command);
    let sub = args.iter().find(|t| !t.starts_with('-')).copied();
    match base {
        "cargo" => match sub {
            Some("test") | Some("bench") | Some("nextest") => Some(CommandKind::Test),
            Some("build") | Some("check") | Some("clippy") | Some("fmt") | Some("doc") => {
                Some(CommandKind::BuildOrLint)
            }
            _ => Some(CommandKind::BuildOrLint),
        },
        "go" => match sub {
            Some("test") => Some(CommandKind::Test),
            _ => Some(CommandKind::BuildOrLint),
        },
        "pytest" | "unittest" | "tox" | "jest" | "vitest" | "mocha" | "rspec" => {
            Some(CommandKind::Test)
        }
        "make" => match sub {
            // By automake convention `make check` IS the full suite.
            Some("test") | Some("check") => Some(CommandKind::Test),
            _ => Some(CommandKind::BuildOrLint),
        },
        "npm" | "pnpm" | "yarn" => match sub {
            Some("test") => Some(CommandKind::Test),
            Some("run") => {
                let script = args
                    .iter()
                    .find(|t| !t.starts_with('-') && **t != "run")
                    .copied();
                match script {
                    Some(s) if s.contains("test") => Some(CommandKind::Test),
                    _ => Some(CommandKind::BuildOrLint),
                }
            }
            _ => Some(CommandKind::BuildOrLint),
        },
        "mvn" | "gradle" => match sub {
            Some("test") => Some(CommandKind::Test),
            _ => Some(CommandKind::BuildOrLint),
        },
        "dotnet" => match sub {
            Some("test") => Some(CommandKind::Test),
            _ => Some(CommandKind::BuildOrLint),
        },
        // Typecheckers and linters/formatters are build/lint by construction.
        // The list mirrors verifier.rs's FAST_LINTERS plus common formatters.
        "tsc" | "rustc" | "eslint" | "ruff" | "mypy" | "flake8" | "shellcheck" | "rubocop"
        | "golangci-lint" | "prettier" | "gofmt" | "rustfmt" | "clang-format" | "black"
        | "isort" => Some(CommandKind::BuildOrLint),
        _ => None,
    }
}

pub(crate) fn strip_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '`' {
            let open = c;
            for inner in chars.by_ref() {
                if inner == open {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn split_sentences(text: &str) -> Vec<&str> {
    text.split(['.', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

pub(crate) fn sentence_attributes_to_another_actor(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    const MARKERS: [&str; 9] = [
        "ci reported",
        "ci says",
        "ci shows",
        "the ci log",
        "the log shows",
        "the output shows",
        "you said",
        "you told",
        "reported by",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Does the sentence assert a TEST-RESULT claim — a test count ("5001
/// passed") or a tests-passed phrase ("tests pass", "all green")? Such a
/// claim requires observed TEST-command evidence (dirge-lavc); a build
/// cannot support it.
fn claims_test_result(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let rest = &lower[i..];
            let after_digits = rest.trim_start();
            if (after_digits.starts_with("passed")
                || after_digits.starts_with("passing")
                || after_digits.starts_with("tests passed")
                || after_digits.starts_with("test passed")
                || after_digits.starts_with("tests passing")
                || after_digits.starts_with("tests pass"))
                && i - start >= 2
            {
                return true;
            }
            continue;
        }
        i += 1;
    }
    const TEST_GATES: &[&str] = &["all green", "all tests pass", "tests pass", "tests passing"];
    TEST_GATES.iter().any(|g| lower.contains(g))
}

/// Does the sentence assert a BUILD/LINT-RESULT claim — a named build, lint,
/// or format gate ("clippy clean", "compiles", "builds clean", "exit 0")?
/// Such a claim requires observed BUILD/LINT-command evidence (dirge-lavc).
///
/// "compiles" and "builds clean" live here — and only here — because a green
/// build is exactly the evidence that supports them: kind-matched, they can
/// never fire against a build that really ran, which is why adding them to
/// the unkinded GATES list was retracted (the observed "Compiles." was TRUE).
fn claims_build_or_lint_result(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    const BUILD_OR_LINT_GATES: &[&str] = &[
        "clippy clean",
        "clippy is clean",
        "fmt clean",
        "formatted clean",
        "compiles",
        "builds clean",
    ];
    BUILD_OR_LINT_GATES.iter().any(|g| lower.contains(g))
}

/// A verification claim that names no KIND of command (dirge-hwk9.6).
///
/// "exit 0" is a claim about whatever ran, and which gate it refers to is not
/// recoverable from the phrase. It used to sit in the build/lint list, so a
/// model that ran its tests cleanly and reported the exit status — which is
/// precisely what the verify nudge asks for when it declines a masked run —
/// was told no build command had run. The harness asked for a number and then
/// penalised the answer.
///
/// Kind-matching stays where it earns its keep: `cargo build` still cannot
/// support "N passed", and a test run still cannot support "clippy clean".
fn claims_generic_verification(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    const GENERIC_GATES: &[&str] = &["exit 0", "exit code 0", "exit status 0"];
    GENERIC_GATES.iter().any(|g| lower.contains(g))
}

fn claims_change(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    const VERBS: [&str; 22] = [
        "fixed",
        "applied",
        "changed",
        "updated",
        "added",
        "removed",
        "implemented",
        "created",
        "deleted",
        "wrote",
        "refactored",
        "renamed",
        "moved",
        "replaced",
        "patched",
        "corrected",
        "edited",
        "modified",
        "rewrote",
        "restructured",
        "adjusted",
        "revised",
    ];
    let needs_boundary = |b: &[u8]| b.first().is_none_or(|&c| !c.is_ascii_alphanumeric());
    VERBS.iter().any(|verb| {
        for prefix in ["i ", "i've ", "i have "] {
            let needle = format!("{prefix}{verb}");
            let bytes = lower.as_bytes();
            let mut idx = 0;
            while let Some(rel) = find_subslice(&bytes[idx..], needle.as_bytes()) {
                let pos = idx + rel;
                let before_ok = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
                let after = pos + needle.len();
                let after_ok = needs_boundary(&bytes[after..]);
                if before_ok && after_ok {
                    return true;
                }
                idx = pos + needle.len();
            }
        }
        false
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Claims {
        scan_final_answer(text)
    }

    fn fires(text: &str, commands: &[&str], files_mutated: usize) -> Option<ClaimKind> {
        let observed: Vec<(String, bool)> =
            commands.iter().map(|c| (c.to_string(), false)).collect();
        unsupported_claims(&scan(text), &observed, files_mutated)
    }

    /// dirge-hwk9.3: an interpreter-prefixed test command is a test command.
    ///
    /// Measured live. The model ran `cd <dir> && python3 -m pytest -v`. The
    /// verifier recognised it — its markers match on ANY token and `pytest` is
    /// there — and the run ended `VerifiedGreen`. This gate did not:
    /// `segment_kind` took the segment's FIRST token, got `python3`, and
    /// returned `None`. So the model was told "no build/test command of the
    /// matching kind ran this run" one turn after it had run one, correctly,
    /// having just been corrected by the verifier for running it wrong.
    ///
    /// `python -m pytest` is the documented way to run pytest against the
    /// current interpreter, and the same shape covers `npx`, `poetry run`,
    /// `uv run` and `pipenv run`.
    /// A claim string these tests can rely on being DETECTED.
    ///
    /// `claims_test_result` requires a run of at least two digits, so the
    /// obvious `"4 passed"` makes no claim at all — and a test written with it
    /// passes whatever the evidence says, which is how two of these first went
    /// green against the very bug they were written for.
    const A_TEST_CLAIM: &str = "All done. 4954 passed, 0 failed.";

    #[test]
    fn the_claim_used_by_these_tests_is_actually_detected() {
        assert!(
            scan(A_TEST_CLAIM).test_claim,
            "the fixture must make a claim, or every test using it is vacuous"
        );
        assert!(
            !scan("All done. 4 passed.").test_claim,
            "single-digit counts are deliberately not claims — if this changes, \
             the fixture above can be simplified"
        );
    }

    #[test]
    fn an_interpreter_prefixed_test_command_is_evidence() {
        for cmd in [
            "python3 -m pytest -v",
            "python -m pytest",
            "cd /tmp/proj && python3 -m pytest -v",
            "python3 -m unittest discover",
            "npx jest",
            "poetry run pytest",
            "uv run pytest -q",
            "pipenv run pytest",
        ] {
            assert_eq!(
                fires(A_TEST_CLAIM, &[cmd], 3),
                None,
                "`{cmd}` runs tests and must count as evidence for a test claim",
            );
        }
    }

    /// The build/lint half of the same shape, so the fix cannot be a special
    /// case for pytest.
    #[test]
    fn an_interpreter_prefixed_lint_command_is_build_evidence() {
        for cmd in [
            "python3 -m mypy .",
            "python -m ruff check",
            "npx tsc --noEmit",
        ] {
            assert_eq!(
                fires("Compiles.", &[cmd], 3),
                None,
                "`{cmd}` is a build/lint command",
            );
        }
    }

    /// The other half, or the fix is "call everything a test": a command that
    /// merely mentions a module must not become evidence, and an interpreter
    /// running an ordinary script is not a test run.
    #[test]
    fn an_interpreter_running_something_else_is_not_evidence() {
        for cmd in [
            "python3 -c \"import inventory; print(inventory.total_value())\"",
            "python3 scripts/seed_db.py",
            "npx prettier --write .", // a formatter is build/lint, never test
        ] {
            assert_eq!(
                fires(A_TEST_CLAIM, &[cmd], 3),
                Some(ClaimKind::Verification),
                "`{cmd}` does not run tests and must not support a test claim",
            );
        }
    }

    /// The two recognisers must agree about what ran. They are separate by
    /// design — the verifier scores COST, this scores COVERAGE — but a command
    /// one of them counts as a test and the other does not counts as nothing
    /// coherent to the model: it gets told to verify, verifies, and is told it
    /// did not. This pins the overlap that produced the live failure.
    #[test]
    fn the_two_recognisers_agree_on_what_counts_as_having_run() {
        use crate::agent::agent_loop::verifier;
        for cmd in [
            "python3 -m pytest -v",
            "cd /tmp/p && python3 -m pytest -v",
            "npx jest",
            "poetry run pytest",
            "python3 -m unittest discover",
            "cargo test",
            "make check",
        ] {
            assert!(
                verifier::is_verification_command_for_test(cmd),
                "the verifier must recognise `{cmd}`"
            );
            assert_eq!(
                fires(A_TEST_CLAIM, &[cmd], 3),
                None,
                "...and so must the claim gate: `{cmd}`"
            );
        }
    }

    /// dirge-hwk9.6: "exit 0" names no KIND of command, so a test run supports
    /// it.
    ///
    /// Measured on deepseek: the model ran a clean `python3 -m pytest -q`,
    /// then wrote "Confirmed with a real exit status: `22 passed in 0.01s`,
    /// exit 0." The claim gate fired, because "exit 0" sat in the
    /// build/lint list and no build command had run.
    ///
    /// It is the verify nudge that asks for the exit status in the first
    /// place — so the harness told the model to report a number and then
    /// penalised it for doing so. Kind-matching exists to stop `cargo build`
    /// supporting "N passed"; a phrase that names no kind should be satisfied
    /// by any verification that ran.
    #[test]
    fn a_bare_exit_status_is_supported_by_any_verification() {
        // The sentence deepseek actually produced. It makes TWO claims — a
        // test count and a bare exit status — and a test run supports both.
        let said = "Confirmed with a real exit status: 22 passed, exit 0.";
        assert_eq!(fires(said, &["python3 -m pytest -q"], 3), None);

        // The exit status ALONE names no kind, so either sort of verification
        // supports it.
        let bare = "The command finished with exit 0.";
        assert_eq!(fires(bare, &["python3 -m pytest -q"], 3), None);
        assert_eq!(fires(bare, &["cargo build"], 3), None);

        // ...and with NOTHING run it still fires — the claim is unsupported,
        // not unconditionally allowed.
        assert_eq!(fires(bare, &[], 3), Some(ClaimKind::Verification));
        assert_eq!(fires(said, &[], 3), Some(ClaimKind::Verification));
    }

    /// The kind-matching that DOES matter is unchanged: a build cannot support
    /// a test-count claim. Without this the fix above could be "accept
    /// anything", which is the false green the gate exists to catch.
    #[test]
    fn a_build_still_cannot_support_a_test_count() {
        assert_eq!(
            fires(A_TEST_CLAIM, &["cargo build"], 3),
            Some(ClaimKind::Verification),
            "a green build says nothing about how many tests passed"
        );
        assert_eq!(
            fires("clippy clean.", &["cargo test"], 3),
            Some(ClaimKind::Verification),
            "...and a test run says nothing about clippy"
        );
    }

    #[test]
    fn verification_claim_without_evidence_fires() {
        assert_eq!(
            fires("All done. 4954 passed, 0 failed.", &[], 3),
            Some(ClaimKind::Verification)
        );
        assert_eq!(
            fires("clippy clean and fmt clean.", &[], 3),
            Some(ClaimKind::Verification)
        );
        assert_eq!(fires("Compiles.", &[], 3), Some(ClaimKind::Verification));
    }

    #[test]
    fn verification_claim_with_evidence_is_silent() {
        assert_eq!(
            fires("All done. 4954 passed, 0 failed.", &["cargo test"], 3),
            None
        );
        assert_eq!(fires("clippy clean.", &["cargo clippy"], 3), None);
    }

    #[test]
    fn test_count_claim_with_only_build_observed_fires() {
        // The false green this gate exists to catch (dirge-lavc): a bare
        // `cargo build` runs zero tests, so "5001 passed" is unsupported
        // even though the build was green.
        assert_eq!(
            fires("All done. 5001 passed, 0 failed.", &["cargo build"], 3),
            Some(ClaimKind::Verification)
        );
    }

    #[test]
    fn compile_claim_with_green_build_observed_is_silent() {
        // False-positive guard, the most important case (dirge-lavc): the
        // observed incident's "Compiles." was TRUE — cargo build really had
        // succeeded. Kind-matched, a green build supports the build claim.
        assert_eq!(fires("Compiles.", &["cargo build"], 3), None);
        assert_eq!(fires("It builds clean.", &["cargo build"], 3), None);
    }

    #[test]
    fn test_count_claim_with_test_observed_is_silent() {
        assert_eq!(
            fires("All done. 5001 passed, 0 failed.", &["cargo test"], 3),
            None
        );
        assert_eq!(fires("All tests pass.", &["cargo test"], 3), None);
    }

    #[test]
    fn mismatched_kind_still_fires() {
        // The kinds are not interchangeable: a test run proves nothing about
        // a named lint gate, and a lint proves nothing about tests.
        assert_eq!(
            fires("clippy clean.", &["cargo test"], 3),
            Some(ClaimKind::Verification)
        );
        assert_eq!(
            fires("All tests pass.", &["cargo clippy"], 3),
            Some(ClaimKind::Verification)
        );
    }

    #[test]
    fn change_claim_without_evidence_fires() {
        assert_eq!(
            fires("I fixed the parser.", &[], 0),
            Some(ClaimKind::Change)
        );
        assert_eq!(
            fires("I've updated the config.", &[], 0),
            Some(ClaimKind::Change)
        );
    }

    #[test]
    fn change_claim_with_evidence_is_silent() {
        assert_eq!(fires("I fixed the parser.", &[], 2), None);
    }

    #[test]
    fn attributed_claim_is_silent() {
        assert_eq!(fires("CI reported 4954 passed.", &[], 0), None);
        assert_eq!(fires("You said the tests pass.", &[], 0), None);
        assert_eq!(
            fires(
                "The log shows \"clippy clean\". I fixed the parser.",
                &[],
                2
            ),
            None
        );
    }

    #[test]
    fn attributed_sentence_does_not_silence_real_claim() {
        assert_eq!(
            fires("CI reported 4954 passed. I fixed the parser.", &[], 0),
            Some(ClaimKind::Change)
        );
    }

    #[test]
    fn future_tense_does_not_fire() {
        assert_eq!(fires("I will fix the parser next.", &[], 0), None);
    }

    #[test]
    fn no_claims_is_silent() {
        assert_eq!(
            fires("Here is a summary of what we discussed.", &[], 0),
            None
        );
    }

    #[test]
    fn quoted_output_is_silent() {
        assert_eq!(fires("The transcript says \"4954 passed\".", &[], 0), None);
    }

    #[test]
    fn advisory_and_blocking_have_different_budgets() {
        assert_eq!(claim_nudge_cap(GateMode::Off), 0, "off must never fire");
        assert_eq!(
            claim_nudge_cap(GateMode::Advisory),
            1,
            "advisory is one-shot"
        );
        assert!(
            claim_nudge_cap(GateMode::Blocking) > claim_nudge_cap(GateMode::Advisory),
            "blocking must re-enter more than advisory, else the mode is decorative"
        );
    }
}
