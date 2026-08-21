//! The harness-intervention registry (dirge-x4se).
//!
//! An *intervention* is any message the harness injects on the model's behalf —
//! a stall checkpoint, a verify-before-done nudge, a claim-gate challenge, a
//! safe-state abort. They are `LoopMessage::User` because the model has to act
//! on them (a display-only notice it never sees changes nothing — dirge-1elu.4),
//! and each carries a leading `[tag]` so consumers can tell them apart from
//! something the user actually typed.
//!
//! # Why this module exists
//!
//! That tag was being matched against three hand-maintained lists that had
//! drifted apart:
//!
//!   - `run::HARNESS_TAGS`, driving the headless `SystemNotice` mirror, carried
//!     11 of the 16 declared tags. The other five — completeness, source, claim,
//!     prologue, code-review — injected messages that a `--print` / `--loop` /
//!     MCP consumer saw the model obey with no indication anything had steered
//!     it.
//!   - `ui::events::harness_intervention_body` (then named for the
//!     finalization family alone) carried 6. Every tag outside that
//!     six rendered in scrollback under `<you>`, attributing a harness
//!     injection to the user who never wrote it.
//!   - The tag constants themselves, scattered across ten modules, with nothing
//!     connecting a declaration to either list.
//!
//! Adding a guard meant remembering two unrelated edits in two other files, and
//! forgetting either failed silently and invisibly — which is how all three
//! lists ended up disagreeing. [`HARNESS_TAGS`] is now the single registry both
//! consumers read, and `registry_covers_every_declared_tag` fails the build if a
//! new `*_TAG` is declared without being added here.
//!
//! # Voice
//!
//! [`summary_for_user`] renders the short human-facing line. The model-facing
//! body is an imperative written for the model; a human watching wants to know
//! what the harness did and why, not to read the instruction it sent. Compare
//! little-coder's `_shared/intervention.ts`, which routes every scaffold
//! override through one helper for exactly this reason — they arrived at it
//! after one abort surfaced as several stacked warnings in different voices.

use super::message::{LoopMessage, UserMessage};

/// Every tag the harness prefixes onto a message it injects on the model's
/// behalf. The single source of truth: the headless notice mirror
/// ([`super::run::emit_harness_notices`]) and the TUI's attribution
/// ([`crate::ui::events::harness_intervention_body`]) both read it, and the
/// registry test below keeps it exhaustive.
pub const HARNESS_TAGS: &[&str] = &[
    super::run::TODO_NUDGE_TAG,
    super::run::OPEN_ISSUES_NUDGE_TAG,
    super::run::RESUME_NUDGE_TAG,
    super::run::TRACK_WORK_TAG,
    super::run::SKILL_ANCHOR_TAG,
    super::verifier::VERIFY_TAG,
    super::critic::CRITIC_TAG,
    super::goal::GOAL_TAG,
    super::progress::STALL_TAG,
    super::progress::BUDGET_TAG,
    super::progress::PROLOGUE_TAG,
    super::safe_state::SAFE_STATE_TAG,
    super::publish_guard::PUBLISH_GUARD_TAG,
    super::completeness_gate::COMPLETENESS_GATE_TAG,
    super::source_gate::SOURCE_GATE_TAG,
    super::claim_gate::CLAIM_GATE_TAG,
    super::code_review::CODE_REVIEW_TAG,
    super::thinking_budget::THINKING_TAG,
];

/// The subset rendered under the `<critic>` handle rather than `<sys>`: the
/// end-of-turn "are you actually done?" family. Kept as a named subset of
/// [`HARNESS_TAGS`] rather than a separate list — every entry must also appear
/// there, which `finalization_tags_are_registered` enforces.
pub const FINALIZATION_TAGS: &[&str] = &[
    super::critic::CRITIC_TAG,
    super::verifier::VERIFY_TAG,
    super::run::TODO_NUDGE_TAG,
    super::code_review::CODE_REVIEW_TAG,
    super::run::RESUME_NUDGE_TAG,
    super::run::OPEN_ISSUES_NUDGE_TAG,
];

/// Prefix on the `SystemNotice` that mirrors an intervention to consumers
/// which never see the tagged message (dirge-hwk9.5).
///
/// Shared so the producer (`run::emit_harness_notices`) and the TUI, which
/// must recognise its own mirror to avoid rendering the body twice, cannot
/// drift — the same reason this module exists at all.
pub const NOTICE_PREFIX: &str = "harness intervention: ";

/// The summary line of an intervention notice, without the body.
///
/// The notice carries `"harness intervention: {summary}\n{body}"` because
/// headless consumers see only it — `--print` renders `SystemNotice` and
/// ignores `UserMessage` entirely. The TUI sees BOTH, and renders the body
/// from the message (which is also the copy `dirge-m10x` guarantees survives
/// the next turn's stream anchor), so showing the notice in full puts the
/// instruction on screen twice with a summary above the first copy.
///
/// `None` for any notice that is not an intervention mirror — the max-turns
/// cap and friends are shown in full.
pub fn notice_summary(content: &str) -> Option<&str> {
    content
        .starts_with(NOTICE_PREFIX)
        .then(|| content.split('\n').next().unwrap_or(content))
}

/// The harness tag `text` carries, if any.
pub fn tag_of(text: &str) -> Option<&'static str> {
    let t = text.trim_start();
    HARNESS_TAGS.iter().copied().find(|tag| t.starts_with(tag))
}

/// True when `text` is a finalization nudge (the `<critic>`-handle subset).
pub fn is_finalization(text: &str) -> bool {
    let t = text.trim_start();
    FINALIZATION_TAGS.iter().any(|tag| t.starts_with(tag))
}

/// `text` with its harness tag stripped, or `None` when it carries none.
pub fn strip_tag(text: &str) -> Option<&str> {
    let t = text.trim_start();
    HARNESS_TAGS
        .iter()
        .find_map(|tag| t.strip_prefix(*tag).map(str::trim_start))
}

/// Build a tagged intervention message.
///
/// The single constructor for an injected steer, so a guard cannot forget the
/// tag and have its message silently attributed to the user.
pub fn user_message(tag: &str, body: &str) -> LoopMessage {
    LoopMessage::User(UserMessage::text(format!("{tag} {body}")))
}

/// The short human-facing line for an intervention, phrased as a continuation
/// of "harness intervention: ". Leads with the consequence — what the harness
/// did — rather than repeating the instruction sent to the model.
pub fn summary_for_user(tag: &str) -> &'static str {
    match tag {
        t if t == super::thinking_budget::THINKING_TAG => {
            "the model has thought long enough — thinking disabled, pushing it to implement"
        }
        t if t == super::progress::STALL_TAG => {
            "no progress for several turns — asked the model to name what is blocking it"
        }
        t if t == super::progress::BUDGET_TAG => "the run is nearing its turn budget",
        t if t == super::progress::PROLOGUE_TAG => {
            "the run has produced nothing yet — pushed for the smallest first write"
        }
        t if t == super::safe_state::SAFE_STATE_TAG => {
            "repeated failures on an unverified tree — asked for a fresh plan from the last green state"
        }
        t if t == super::publish_guard::PUBLISH_GUARD_TAG => {
            "blocked a write to a published artifact"
        }
        t if t == super::claim_gate::CLAIM_GATE_TAG => {
            "the answer asserted something the run never checked"
        }
        t if t == super::source_gate::SOURCE_GATE_TAG => {
            "a written comment cited a source the run never consulted"
        }
        t if t == super::completeness_gate::COMPLETENESS_GATE_TAG => {
            "the run looks unfinished — asked the model to confirm before stopping"
        }
        t if t == super::critic::CRITIC_TAG => "the critic pushed back on the answer",
        t if t == super::verifier::VERIFY_TAG => "asked the model to verify before finishing",
        t if t == super::goal::GOAL_TAG => "restated the goal the run started from",
        t if t == super::run::TODO_NUDGE_TAG => "asked the model to close out its todo list",
        t if t == super::run::TRACK_WORK_TAG => "asked the model to track this work",
        t if t == super::run::SKILL_ANCHOR_TAG => {
            "restated a loaded skill's anchor so it stays in force"
        }
        t if t == super::run::OPEN_ISSUES_NUDGE_TAG => "surfaced open issues before finishing",
        t if t == super::run::RESUME_NUDGE_TAG => "asked the model to resume the interrupted task",
        t if t == super::code_review::CODE_REVIEW_TAG => "code review found something to fix",
        _ => "the harness redirected the model",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant this module exists to hold: every `*_TAG` declared
    /// anywhere in the agent loop is registered here.
    ///
    /// Rust can't enumerate constants reflectively, so this scans the source.
    /// Crude, but it fails loudly at the moment a guard is added without being
    /// registered — which is precisely the failure that let three lists drift
    /// apart, each one silent about what it was missing.
    #[test]
    fn registry_covers_every_declared_tag() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut declared: Vec<(String, String)> = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in src.lines() {
                    let line = line.trim();
                    // Declarations only — a comment mentioning the shape of one
                    // (including the one above this loop) is not a declaration.
                    if line.starts_with("//") {
                        continue;
                    }
                    // Shape: `[pub[(crate)]] const NAME_TAG: &str = "[x]";`
                    let Some(rest) = line.split_once("_TAG: &str = ") else {
                        continue;
                    };
                    if !rest.0.contains("const ") {
                        continue;
                    }
                    let name = rest.0.rsplit(' ').next().unwrap_or("").to_string() + "_TAG";
                    let Some(value) = rest.1.split('"').nth(1) else {
                        continue;
                    };
                    declared.push((name, value.to_string()));
                }
            }
        }

        assert!(
            declared.len() >= HARNESS_TAGS.len(),
            "the source scan found only {} tag declarations — it has stopped matching \
             how they are written, so it can no longer enforce anything",
            declared.len()
        );

        let missing: Vec<_> = declared
            .iter()
            .filter(|(_, value)| !HARNESS_TAGS.contains(&value.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "these harness tags are declared but not in HARNESS_TAGS, so headless \
             consumers get no notice and the TUI attributes the injection to the \
             user: {missing:?}"
        );
    }

    #[test]
    fn finalization_tags_are_registered() {
        for tag in FINALIZATION_TAGS {
            assert!(
                HARNESS_TAGS.contains(tag),
                "{tag} is a finalization tag but is not in the registry"
            );
        }
    }

    #[test]
    fn tag_is_recognized_with_and_without_leading_space() {
        let tagged = format!("  {} do the thing", super::super::progress::STALL_TAG);
        assert_eq!(tag_of(&tagged), Some(super::super::progress::STALL_TAG));
        assert_eq!(strip_tag(&tagged), Some("do the thing"));
    }

    #[test]
    fn ordinary_user_text_carries_no_tag() {
        assert_eq!(tag_of("fix the parser bug"), None);
        assert_eq!(tag_of("[note] not a harness tag"), None);
        assert_eq!(strip_tag("fix the parser bug"), None);
    }

    #[test]
    fn constructor_produces_a_recognizable_intervention() {
        let msg = user_message(super::super::progress::STALL_TAG, "name the blocker");
        let LoopMessage::User(u) = &msg else {
            panic!("an intervention must be a user message so the model acts on it");
        };
        let text = u.text_joined();
        assert_eq!(tag_of(&text), Some(super::super::progress::STALL_TAG));
        assert_eq!(strip_tag(&text), Some("name the blocker"));
    }

    /// Every registered tag has a human summary written for it; the fallback is
    /// for tags a future guard adds, not a place to leave existing ones.
    #[test]
    fn every_tag_has_a_written_summary() {
        for tag in HARNESS_TAGS {
            assert_ne!(
                summary_for_user(tag),
                "the harness redirected the model",
                "{tag} fell through to the generic summary"
            );
        }
    }

    #[test]
    fn finalization_subset_is_distinguishable() {
        assert!(is_finalization(&format!(
            "{} not done",
            super::super::critic::CRITIC_TAG
        )));
        assert!(!is_finalization(&format!(
            "{} stalled",
            super::super::progress::STALL_TAG
        )));
    }
}
