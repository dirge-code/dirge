//! Recovery classes for failed tool results (dirge-61sv).
//!
//! # Why one streak is not enough
//!
//! Provider errors get six-way classification in [`crate::agent::recovery`],
//! with `Retry-After` parsing and per-class backoff. Tool errors got none:
//! every failure funnelled into [`super::failure_tracker`] as one
//! undifferentiated consecutive-failure streak producing one generic
//! reflection nudge.
//!
//! That flattens distinctions the model needs. A `No such file or directory` is
//! the model's picture of the tree being wrong — it should go look. A timeout is
//! the work being too big — re-issuing it verbatim burns the budget again. A
//! schema rejection is the call being malformed — the contract is already in
//! context and wants re-reading. Telling all three "diagnose and try a
//! different approach" is advice that fits none of them.
//!
//! # What this is for, concretely
//!
//! It came out of a measured failure. Two control runs in the dirge-e31n A/Bs
//! burned 17 and 26 tool calls at 24% and 27% error rates and still classified
//! as [`super::capability::CapabilityTier::Nominal`], because every errored call
//! carries the same weight regardless of what it says. `storm` saw nothing (the
//! calls were varied, not repeated), `scavenge` and `repair` saw nothing (the
//! calls were well-formed), and the streak never reached three in a row. The
//! model was not fumbling the grammar, it was choosing badly — and nothing in
//! the harness could tell the difference between that and ordinary friction.
//!
//! # Conservative by construction
//!
//! [`ErrorClass::Unclassified`] is the default and must stay behaviourally
//! identical to today. A classifier that guesses is worse than one that
//! abstains: a mislabelled `Transient` would suppress a real signal, and a
//! mislabelled `Fatal` would stop a run that could have continued. Every
//! pattern here is anchored on wording the underlying tool or OS actually
//! emits, and anything unrecognised stays unclassified and counts exactly as it
//! did before.

/// What KIND of failure a tool result represents, and therefore what the model
/// should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorClass {
    /// The call was malformed — bad arguments, schema violation, unknown
    /// field. The tool's contract is already in context; re-read it.
    /// `tool_input_repair` already fixes the mechanical cases, so what reaches
    /// here is what repair could not.
    Misuse,
    /// The world is not shaped the way the model thinks: file missing, symbol
    /// unknown, pattern matched nothing. The fix is to LOOK, not to retry.
    ///
    /// This is the wandering signal — a run accumulating these is operating on
    /// a wrong picture of the tree, which is exactly the failure that reads as
    /// "many varied calls, none of them repeats" and that no other guard sees.
    MissingInfo,
    /// The operation did not complete for reasons unrelated to its inputs:
    /// timeout, connection reset, resource temporarily unavailable. Retrying
    /// can legitimately work; the model changing its approach cannot help.
    Transient,
    /// A wall: OS-level permission refusal, read-only filesystem. Distinct
    /// from a policy `Denied` outcome, which the tracker already routes to its
    /// own permission checkpoint — this is the filesystem saying no.
    Fatal,
    /// Not recognised. Behaves exactly as an errored result did before this
    /// module existed.
    ///
    /// Declared LAST deliberately: it is the sentinel
    /// `all_ends_at_the_sentinel_so_a_new_class_cannot_hide` anchors on, the
    /// same contract as [`super::gate_tally::GateSource::None`].
    #[default]
    Unclassified,
}

impl ErrorClass {
    /// Every variant, in [`index`](Self::index) order.
    ///
    /// Exists so the per-class error counters cannot drift from the enum. The
    /// tally's array is sized off this, the emitted line is asserted against
    /// it, and [`super::capability`] weights it with an exhaustive match — so
    /// adding a class fails to compile until it has an index, a weight and a
    /// field, rather than being silently dropped from the surface that reports
    /// it. That drift is what lost two gates from the `dirge::gates` line
    /// (dirge-l8l7.1); this is the same shape, pre-empted.
    pub const ALL: [ErrorClass; 5] = [
        ErrorClass::Misuse,
        ErrorClass::MissingInfo,
        ErrorClass::Transient,
        ErrorClass::Fatal,
        ErrorClass::Unclassified,
    ];

    /// Slot in a per-class counter array — the variant's own discriminant, so
    /// it cannot disagree with the declaration order [`ALL`](Self::ALL) comes
    /// from.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The field name this class carries on the `dirge::gates` line.
    ///
    /// Unlike the gate and nudge sentinels, EVERY class is emitted:
    /// `Unclassified` is a real count (the residue the classifier declined to
    /// name), and hiding it would make the other four unreadable — a run with
    /// two missing-info errors reads very differently at 2 of 3 errors than at
    /// 2 of 30.
    ///
    /// Test-only for the same reason as [`super::gate_tally::GateSource::field_name`]:
    /// `tracing` needs literals at the macro call, so the correspondence has to
    /// be asserted rather than shared.
    #[cfg(test)]
    pub fn field_name(self) -> &'static str {
        match self {
            ErrorClass::Misuse => "errored_misuse",
            ErrorClass::MissingInfo => "errored_missing_info",
            ErrorClass::Transient => "errored_transient",
            ErrorClass::Fatal => "errored_fatal",
            ErrorClass::Unclassified => "errored_unclassified",
        }
    }

    /// Short label for the checkpoint text.
    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::Misuse => "malformed calls",
            ErrorClass::MissingInfo => "things that aren't there",
            ErrorClass::Transient => "transient failures",
            ErrorClass::Fatal => "permission walls",
            ErrorClass::Unclassified => "failures",
        }
    }

    /// The one instruction that actually fits this class. Returned separately
    /// from [`Self::label`] so the checkpoint can name the class and then say
    /// something useful about it, rather than emitting generic advice that
    /// fits none of the classes it was given.
    pub fn guidance(self) -> Option<&'static str> {
        match self {
            ErrorClass::Misuse => Some(
                "These were rejected on their arguments, not on the state of the world. \
                 Re-read the tool's parameter schema in your tool definitions and fix the \
                 call shape — exploring more will not help.",
            ),
            ErrorClass::MissingInfo => Some(
                "These failed because what you asked for isn't there. Your picture of the \
                 tree is wrong, so stop calling and go look: list the directory, read the \
                 file you're assuming exists, or widen the search. Guessing another path \
                 is the same move that just failed.",
            ),
            ErrorClass::Transient => Some(
                "These did not fail on your inputs — they timed out or the resource was \
                 briefly unavailable. Changing your approach will not fix that. Narrow the \
                 work, or retry deliberately once.",
            ),
            ErrorClass::Fatal => Some(
                "These are permission walls, not mistakes. Retrying and rewording will both \
                 fail. Say what is blocked and continue with what you can reach.",
            ),
            ErrorClass::Unclassified => None,
        }
    }
}

/// Classify a failed tool result from its tool name and error text.
///
/// Matching is case-insensitive and anchored on wording the tools and the OS
/// actually emit. Order matters: the checks run most-specific first, because
/// real error strings routinely satisfy more than one pattern — a permission
/// error often also contains the path that "not found" would match, and a
/// timeout message can name a file. The first match wins, so the ordering
/// below IS the precedence rule and not an accident of arrangement.
pub fn classify(_tool_name: &str, excerpt: &str) -> ErrorClass {
    let s = excerpt.to_ascii_lowercase();

    // FATAL first. A permission error frequently also contains a path, so a
    // "not found" check running earlier would steal it and tell the model to
    // go looking for a file it is simply not allowed to open.
    if s.contains("permission denied")
        || s.contains("read-only file system")
        || s.contains("operation not permitted")
        || s.contains("eacces")
        || s.contains("eperm")
    {
        return ErrorClass::Fatal;
    }

    // TRANSIENT before MISSING_INFO for the same reason: "connection reset
    // while reading /x/y" names a path it never actually failed to find.
    if s.contains("timed out")
        || s.contains("timeout")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("temporarily unavailable")
        || s.contains("eagain")
        || s.contains("try again")
    {
        return ErrorClass::Transient;
    }

    // MISUSE before MISSING_INFO: a schema rejection can quote an argument
    // value that looks like a path, and "invalid path" is a call-shape problem
    // rather than an absent file.
    if s.contains("invalid")
        || s.contains("missing required")
        || s.contains("schema")
        || s.contains("unknown field")
        || s.contains("expected ")
        || s.contains("must be ")
        || s.contains("failed to parse")
    {
        return ErrorClass::Misuse;
    }

    if s.contains("no such file")
        || s.contains("not found")
        || s.contains("does not exist")
        || s.contains("no matches")
        || s.contains("did not match")
        || s.contains("enoent")
    {
        return ErrorClass::MissingInfo;
    }

    ErrorClass::Unclassified
}

/// The class that accounts for strictly more than half of `classes`.
///
/// A strict majority, not a plurality, and this is the load-bearing choice.
/// The checkpoint prints one class-specific instruction, so naming a class that
/// describes three of seven failures would give the model confident direction
/// about the minority of what went wrong. Below a majority the honest answer is
/// that the streak has no single character, and the generic advice — which is
/// still good advice — stands alone.
///
/// [`ErrorClass::Unclassified`] can never win: it carries no guidance, so
/// "mostly unrecognised" is indistinguishable from having no dominant class.
pub fn dominant_class(classes: &[ErrorClass]) -> Option<ErrorClass> {
    if classes.is_empty() {
        return None;
    }
    let mut best: Option<(ErrorClass, usize)> = None;
    for candidate in [
        ErrorClass::Misuse,
        ErrorClass::MissingInfo,
        ErrorClass::Transient,
        ErrorClass::Fatal,
    ] {
        let n = classes.iter().filter(|c| **c == candidate).count();
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((candidate, n));
        }
    }
    best.filter(|(_, n)| *n * 2 > classes.len()).map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_each_class_from_real_error_wording() {
        assert_eq!(
            classify("read", "Error: No such file or directory (os error 2)"),
            ErrorClass::MissingInfo
        );
        assert_eq!(
            classify("bash", "command timed out after 120s"),
            ErrorClass::Transient
        );
        assert_eq!(
            classify("write", "Permission denied (os error 13)"),
            ErrorClass::Fatal
        );
        assert_eq!(
            classify(
                "edit",
                "invalid arguments: missing required field `old_text`"
            ),
            ErrorClass::Misuse
        );
    }

    /// The default must stay behaviourally identical to pre-classification, so
    /// an unrecognised error has to land here rather than being forced into
    /// the nearest-looking bucket.
    #[test]
    fn unrecognised_text_stays_unclassified() {
        assert_eq!(
            classify("bash", "make: *** [target] Error 1"),
            ErrorClass::Unclassified
        );
        assert_eq!(classify("read", ""), ErrorClass::Unclassified);
    }

    /// Precedence is the whole design, not an accident of ordering. Each of
    /// these satisfies TWO patterns, and the wrong winner sends the model
    /// somewhere actively unhelpful.
    #[test]
    fn precedence_resolves_errors_that_match_two_patterns() {
        // Contains a path, but the problem is the wall — telling the model to
        // go looking for the file would be wrong.
        assert_eq!(
            classify(
                "read",
                "Permission denied: /etc/shadow not found in allowlist"
            ),
            ErrorClass::Fatal
        );
        // Names a file, but nothing was missing — the read timed out.
        assert_eq!(
            classify(
                "read",
                "timed out reading /src/main.rs, file not found in cache"
            ),
            ErrorClass::Transient
        );
        // Quotes an argument that looks like a path; the call shape is wrong.
        assert_eq!(
            classify("edit", "invalid parameter `path`: /a/b does not exist"),
            ErrorClass::Misuse
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(
            classify("read", "NO SUCH FILE OR DIRECTORY"),
            ErrorClass::MissingInfo
        );
        assert_eq!(
            classify("bash", "Connection Reset By Peer"),
            ErrorClass::Transient
        );
    }

    #[test]
    fn a_strict_majority_wins() {
        use ErrorClass::*;
        assert_eq!(
            dominant_class(&[MissingInfo, MissingInfo, Misuse]),
            Some(MissingInfo)
        );
    }

    /// An exact tie is not a majority. Naming one of two equally-represented
    /// classes would hand the model confident direction about half its
    /// failures and silence about the other half.
    #[test]
    fn an_exact_tie_has_no_dominant_class() {
        use ErrorClass::*;
        assert_eq!(dominant_class(&[MissingInfo, Misuse]), None);
        assert_eq!(
            dominant_class(&[MissingInfo, MissingInfo, Misuse, Misuse]),
            None
        );
    }

    /// A plurality below half is not enough either — 3 of 7 leaves the
    /// majority of the streak undescribed.
    #[test]
    fn a_plurality_short_of_half_is_not_dominant() {
        use ErrorClass::*;
        let streak = [
            MissingInfo,
            MissingInfo,
            MissingInfo,
            Misuse,
            Misuse,
            Transient,
            Fatal,
        ];
        assert_eq!(dominant_class(&streak), None);
    }

    /// Unclassified carries no guidance, so a mostly-unrecognised streak has
    /// no dominant class rather than a useless one.
    #[test]
    fn unclassified_never_dominates() {
        use ErrorClass::*;
        assert_eq!(
            dominant_class(&[Unclassified, Unclassified, Unclassified]),
            None
        );
        // ...but a real majority still wins alongside unclassified noise.
        assert_eq!(
            dominant_class(&[MissingInfo, MissingInfo, MissingInfo, Unclassified]),
            Some(MissingInfo)
        );
    }

    #[test]
    fn empty_streak_has_no_dominant_class() {
        assert_eq!(dominant_class(&[]), None);
    }

    /// The one way left to break the per-class counters: add a variant ahead
    /// of `Unclassified` and not extend `ALL`. `index` is the discriminant, so
    /// the new class would write past the end of a `[u32; ALL.len()]` and
    /// panic at runtime — while every test that iterates `ALL` stayed green,
    /// because `ALL` is exactly what is missing it. Anchoring on the last
    /// variant's discriminant is what makes that visible (dirge-l8l7.1).
    #[test]
    fn all_ends_at_the_sentinel_so_a_new_class_cannot_hide() {
        assert_eq!(
            ErrorClass::ALL.len(),
            ErrorClass::Unclassified as usize + 1,
            "an ErrorClass variant was added without extending ALL"
        );
    }

    #[test]
    fn all_classes_are_indexed_contiguously() {
        let mut seen = vec![false; ErrorClass::ALL.len()];
        for class in ErrorClass::ALL {
            let i = class.index();
            assert!(i < seen.len(), "{class:?} indexes {i}, past the end of ALL");
            assert!(!seen[i], "index {i} is claimed twice; {class:?} collides");
            seen[i] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "ErrorClass::ALL is missing a variant"
        );
    }

    /// Field names must be distinct, or two classes share a counter on the
    /// emitted line and the mix is unreadable.
    #[test]
    fn every_class_has_a_distinct_field_name() {
        let names: std::collections::HashSet<&str> = ErrorClass::ALL
            .into_iter()
            .map(ErrorClass::field_name)
            .collect();
        assert_eq!(
            names.len(),
            ErrorClass::ALL.len(),
            "two ErrorClass variants share a field name: {names:?}"
        );
    }

    /// Every class that can be dominant must have guidance to print, or the
    /// checkpoint names a class and then says nothing about it.
    #[test]
    fn every_dominatable_class_carries_guidance() {
        for c in [
            ErrorClass::Misuse,
            ErrorClass::MissingInfo,
            ErrorClass::Transient,
            ErrorClass::Fatal,
        ] {
            assert!(
                c.guidance().is_some(),
                "{c:?} can dominate but has no guidance"
            );
            assert!(!c.label().is_empty());
        }
        assert!(ErrorClass::Unclassified.guidance().is_none());
    }
}
