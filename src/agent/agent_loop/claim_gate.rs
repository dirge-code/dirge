//! Deterministic claim/evidence gate (dirge-d0e5.2).
//!
//! At finalization, a model-visible one-shot nudge fires when the final
//! answer makes a SPECIFIC claim the run's evidence does not support:
//!
//! - **Unsupported verification claim** — the answer asserts a test count or
//!   named-gate result ("4954 passed", "clippy clean") while the verifier
//!   recorded NO build/test command this run.
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
/// fourth. Mirrors [`super::code_review::MAX_REVIEW_REACT`].
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
                 \"N passed\" or a named gate like \"clippy clean\"), but no build/test \
                 command ran this run, so the claim is unsupported. Either actually run \
                 the check and report its real output, or remove the unsupported claim."
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Claims {
    pub verification_claim: bool,
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
        claims.verification_claim |= claims_verification(sentence);
        claims.change_claim |= claims_change(sentence);
    }
    claims
}

/// The deterministic conjunction: a claim with no supporting evidence from
/// this run. `ran_verification` — did the verifier observe a build/test
/// command this run. `files_mutated` — how many files the tracker recorded
/// since the run's epoch.
pub(crate) fn unsupported_claims(
    claims: &Claims,
    ran_verification: bool,
    files_mutated: usize,
) -> Option<ClaimKind> {
    if claims.verification_claim && !ran_verification {
        return Some(ClaimKind::Verification);
    }
    if claims.change_claim && files_mutated == 0 {
        return Some(ClaimKind::Change);
    }
    None
}

/// Drop double-quoted and backtick-quoted spans. A pasted CI log or a
/// user-supplied transcript is someone else's output; the model quoting it is
/// not asserting it as its own run's outcome.
fn strip_quoted(text: &str) -> String {
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

/// Split on sentence boundaries so an attributed sentence can be dropped
/// without silencing a real claim in the same answer ("CI reported 4954
/// passed. I then fixed the parser.").
fn split_sentences(text: &str) -> Vec<&str> {
    text.split(['.', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// A claim scoped to another actor or to the past ("CI reported", "you
/// said") is not the model asserting this run's outcome — the carve-out
/// from the spec.
fn sentence_attributes_to_another_actor(sentence: &str) -> bool {
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

/// A concrete verification-outcome claim: a test count ("4954 passed",
/// "12 tests passing") or a named gate ("clippy clean", "fmt clean", "all
/// green", "exit 0").
fn claims_verification(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    // Numeric test counts: <digits> [test|tests] (passed|passing).
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
    const GATES: [&str; 10] = [
        "clippy clean",
        "clippy is clean",
        "fmt clean",
        "formatted clean",
        "all green",
        "all tests pass",
        "tests pass",
        "tests passing",
        "exit 0",
        "exit code 0",
    ];
    GATES.iter().any(|g| lower.contains(g))
}

/// A first-person past-tense change claim ("I fixed the parser", "I've
/// updated the config"). Present/future tense ("I will fix …") does not
/// assert that a change was already applied, so it does not count.
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

    fn fires(text: &str, ran_verification: bool, files_mutated: usize) -> Option<ClaimKind> {
        unsupported_claims(&scan(text), ran_verification, files_mutated)
    }

    // Spec case 1: "4954 passed" with no verification command → fires.
    #[test]
    fn verification_claim_without_evidence_fires() {
        assert_eq!(
            fires("All done. 4954 passed, 0 failed.", false, 3),
            Some(ClaimKind::Verification)
        );
        assert_eq!(
            fires("clippy clean and fmt clean.", false, 3),
            Some(ClaimKind::Verification)
        );
    }

    // Spec case 2: same claim WITH a verification command → silent. The
    // discriminating pair with case 1; neither means anything alone.
    #[test]
    fn verification_claim_with_evidence_is_silent() {
        assert_eq!(fires("All done. 4954 passed, 0 failed.", true, 3), None);
        assert_eq!(fires("clippy clean.", true, 3), None);
    }

    // Spec case 3: "I fixed the parser" with zero files mutated → fires.
    #[test]
    fn change_claim_without_evidence_fires() {
        assert_eq!(
            fires("I fixed the parser.", false, 0),
            Some(ClaimKind::Change)
        );
        assert_eq!(
            fires("I've updated the config.", false, 0),
            Some(ClaimKind::Change)
        );
    }

    // Spec case 4: same claim with files mutated → silent.
    #[test]
    fn change_claim_with_evidence_is_silent() {
        assert_eq!(fires("I fixed the parser.", false, 2), None);
    }

    // Spec case 5: a quoted / attributed claim is someone else's output, not
    // the model's own assertion → silent.
    #[test]
    fn attributed_claim_is_silent() {
        assert_eq!(fires("CI reported 4954 passed.", false, 0), None);
        assert_eq!(fires("You said the tests pass.", false, 0), None);
        assert_eq!(
            fires(
                "The log shows \"clippy clean\". I fixed the parser.",
                false,
                2
            ),
            None
        );
    }

    // An attributed sentence does not silence a REAL claim in the same
    // answer — the conjunction stays honest.
    #[test]
    fn attributed_sentence_does_not_silence_real_claim() {
        assert_eq!(
            fires("CI reported 4954 passed. I fixed the parser.", false, 0),
            Some(ClaimKind::Change)
        );
    }

    // Past tense only: "I will fix" is not an assertion that a change
    // happened.
    #[test]
    fn future_tense_does_not_fire() {
        assert_eq!(fires("I will fix the parser next.", false, 0), None);
    }

    // A plain summary with no concrete claims never fires.
    #[test]
    fn no_claims_is_silent() {
        assert_eq!(
            fires("Here is a summary of what we discussed.", false, 0),
            None
        );
    }

    // A quoted count ("the transcript says \"4954 passed\"") is quoting, not
    // asserting — the carve-out strips the quotes.
    #[test]
    fn quoted_output_is_silent() {
        assert_eq!(
            fires("The transcript says \"4954 passed\".", false, 0),
            None
        );
    }
    /// dirge-d0e5.2 follow-up: `advisory` and `blocking` must actually differ.
    /// They were byte-identical at first — both gated on a single `MAX_CLAIM_NUDGES`
    /// — so the config surface advertised a distinction that did not exist.
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
