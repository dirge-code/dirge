//! Deterministic completeness gate (dirge-2m68).
//!
//! Fires at finalization when the final answer says, **in the model's own
//! voice**, that work remains — and the run is stopping anyway.
//!
//! # The hole this fills
//!
//! On a default install the always-on gates are all narrow mechanical
//! detectors: the verifier wants code that was edited but never run, the
//! resume gate wants a failed last tool call, the claim gate wants a claim the
//! evidence contradicts, the todo gate wants todos the model actually tracked.
//! The LLM completeness judge — the one gate that asks "is this task actually
//! done?" — is inert unless `critic_provider` is configured.
//!
//! So a run that edits real files, runs a real check, claims nothing false,
//! tracks no todos, and then stops halfway hits nothing at all. That is the
//! most ordinary way for an autonomous run to end badly, and it was the one
//! shape with no backstop.
//!
//! # Why this signal and not "the answer looks short"
//!
//! The original proposal was to re-drive on a short final answer. Length is a
//! weak proxy, and per docs/verification-discipline.md a gate that fires on
//! honest work gets switched off and then catches nothing. A model that writes
//! "next I'll wire up the tests" and then stops has contradicted *itself*;
//! that needs no judgement call, and it is checkable with no LLM.
//!
//! # Deliberately narrow
//!
//! Three conditions must hold **inside one sentence** before it counts:
//!
//! 1. a first-person forward marker (`I'll`, `I still need to`, …) — not a
//!    bare "next steps", which is usually a handoff;
//! 2. a work verb (`implement`, `wire`, `fix`, …) — so "I'll explain why
//!    below", fulfilled in the same message, does not count;
//! 3. no second-person address — "I'll leave that to you" and "you'll want to
//!    run migrations" are handoffs, not abandoned intentions.
//!
//! And at the call site: the turn actually edited files, and there are no
//! unfinished tracked todos (that case belongs to the todo gate, which is more
//! actionable). Quoted spans and sentences attributed to another actor are
//! stripped first, sharing [`super::claim_gate`]'s helpers rather than a
//! second copy of them.
//!
//! Do not widen the patterns to catch more. The conjunction IS the control.

use super::types::GateMode;

/// Tag prefixing the model-visible nudge, so it is greppable in transcripts.
pub(crate) const COMPLETENESS_GATE_TAG: &str = "[unfinished-check]";

/// Per-run nudge ceiling, by mode. Mirrors
/// [`super::claim_gate::claim_nudge_cap`]: `advisory` says it once, `blocking`
/// re-enters up to three times, `off` never fires.
pub(crate) fn completeness_nudge_cap(mode: GateMode) -> u8 {
    match mode {
        GateMode::Off => 0,
        GateMode::Advisory => 1,
        GateMode::Blocking => 3,
    }
}

/// Body of the nudge. Asks for the work or an explicit stop — never a
/// verdict, so it cannot cause a false green, and never an assertion that the
/// task IS incomplete, only that the model said so.
pub(crate) fn nudge_text() -> &'static str {
    "Your final message states work you still intend to do, but the run is finishing, \
     so that work will not happen. Either do it now, or — if you are deliberately \
     stopping here — record what is left with write_todo_list and say plainly in your \
     answer that you are handing it over unfinished, and why."
}

/// The whole firing decision, as a pure function.
///
/// Extracted rather than left inline for the reason `should_advise_untracked_work`
/// was: `todo::unfinished_count()` is a PROCESS-GLOBAL mirror, so an
/// integration test that arranges it races every other `#[tokio::test]` in the
/// binary and flakes deterministically-but-unpredictably. The lesson is
/// recorded in the issues/todos split work — test the pure helper, not the
/// global.
///
/// `unfinished == 0` is not redundant with the todo gate: it is what stops the
/// two firing on the same run. With tracked todos outstanding, "finish your
/// todos" is the more actionable message and that gate owns it.
pub(crate) fn should_nudge_incomplete(
    mode: GateMode,
    nudges_so_far: u8,
    unfinished_todos: usize,
    turn_made_edits: bool,
    final_answer: &str,
) -> bool {
    // No `mode != Off` check: `completeness_nudge_cap(Off)` is 0, so the cap
    // below already encodes "never fires". Mutation-testing found the pair
    // indistinguishable — dropping the explicit check changed no behaviour,
    // which is the definition of a redundant second encoding of the same rule,
    // and the exact shape dirge-l8l7 was about. One source of truth: the cap,
    // asserted directly by `caps_match_the_documented_modes`.
    nudges_so_far < completeness_nudge_cap(mode)
        && turn_made_edits
        && unfinished_todos == 0
        && states_remaining_work(final_answer)
}

/// Does the final answer state remaining first-person work?
///
/// Quoted spans go first (a pasted plan or log is not the model's own
/// intention), then each sentence must satisfy all three conditions in the
/// module doc.
pub(crate) fn states_remaining_work(text: &str) -> bool {
    let stripped = super::claim_gate::strip_quoted(text);
    super::claim_gate::split_sentences(&stripped)
        .into_iter()
        .any(sentence_states_remaining_work)
}

/// First-person forward intent: the model is talking about what IT is going to
/// do next. `next steps` / `remaining work` are deliberately absent — on their
/// own they are how a handoff is written.
const FORWARD_MARKERS: [&str; 10] = [
    "i'll ",
    "i will ",
    "i’ll ",
    "i'm going to ",
    "i am going to ",
    "i plan to ",
    "i intend to ",
    "i still need to ",
    "i still have to ",
    "i haven't ",
];

/// Work the run could have done with its tools. Present-tense counterparts of
/// [`super::claim_gate`]'s past-tense change verbs — this gate asks what is
/// left, that one asks what was claimed.
const WORK_VERBS: [&str; 24] = [
    "implement",
    "add",
    "wire",
    "hook up",
    "fix",
    "write",
    "update",
    "refactor",
    "migrate",
    "finish",
    "complete",
    "test",
    "handle",
    "remove",
    "delete",
    "rename",
    "move",
    "replace",
    "port",
    "extend",
    "cover",
    "clean up",
    "document",
    "verify",
];

/// Does `needle` occur in `lower` at the START of a word?
///
/// A plain `contains` is wrong here and measurably so: it read `test` out of
/// `latest`, `fix` out of `prefix`, and `port` out of `report`, `support` and
/// `important` — so "I'll use the latest version" fired the gate. Those are
/// precisely the honest sentences that get a gate switched off.
///
/// Only the LEADING boundary is required. Verbs appear in their infinitive
/// form but the sentence may inflect them ("I'll be implementing the retry
/// path"), and demanding a trailing boundary would drop those.
///
/// `claim_gate` solved the same problem for its past-tense verbs; this is that
/// rule applied to the same class of list.
fn contains_word_starting_with(lower: &str, needle: &str) -> bool {
    let (hay, ndl) = (lower.as_bytes(), needle.as_bytes());
    if ndl.is_empty() || hay.len() < ndl.len() {
        return false;
    }
    (0..=hay.len() - ndl.len())
        .any(|i| &hay[i..i + ndl.len()] == ndl && (i == 0 || !hay[i - 1].is_ascii_alphanumeric()))
}

/// Addressing the user makes it a handoff, not an abandoned intention —
/// "I'll leave the migration to you" is a complete answer.
///
/// Word-anchored for the same reason as the verbs: `bayou ` contains `you `,
/// and silencing the gate on it is the harmless direction but still wrong.
fn addresses_the_user(lower: &str) -> bool {
    const SECOND_PERSON: [&str; 5] = ["you ", "you'", "your ", "you,", "you."];
    SECOND_PERSON
        .iter()
        .any(|m| contains_word_starting_with(lower, m))
}

fn sentence_states_remaining_work(sentence: &str) -> bool {
    if super::claim_gate::sentence_attributes_to_another_actor(sentence) {
        return false;
    }
    // Pad so a marker at the very start still matches its trailing space.
    let lower = format!(" {} ", sentence.to_ascii_lowercase());
    if addresses_the_user(&lower) {
        return false;
    }
    let has_marker = FORWARD_MARKERS
        .iter()
        .any(|m| contains_word_starting_with(&lower, m));
    if !has_marker {
        return false;
    }
    WORK_VERBS
        .iter()
        .any(|v| contains_word_starting_with(&lower, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_match_the_documented_modes() {
        assert_eq!(completeness_nudge_cap(GateMode::Off), 0);
        assert_eq!(completeness_nudge_cap(GateMode::Advisory), 1);
        assert_eq!(completeness_nudge_cap(GateMode::Blocking), 3);
    }

    #[test]
    fn fires_on_stated_remaining_first_person_work() {
        for answer in [
            "I've added the parser. Next I'll wire up the tests.",
            "The core change is in. I still need to implement the retry path.",
            "Done with the refactor; I will update the docs next.",
            "I haven't migrated the old callers yet.",
            "I plan to extend this to the other providers.",
        ] {
            assert!(
                states_remaining_work(answer),
                "should have fired on: {answer}"
            );
        }
    }

    /// The other side, and the one that decides whether this gate survives
    /// contact with real runs. Every case here is an honest, complete answer.
    #[test]
    fn stays_silent_on_honest_answers() {
        for answer in [
            // Plain completion.
            "Added the parser and wired up the tests. All checks pass.",
            // Handoff — remaining work that is explicitly the user's.
            "I've landed the code change. You'll want to run the migration next.",
            "The refactor is done. I'll leave the deploy to you.",
            "Next steps for you: run the migration, then restart the workers.",
            // Forward intent that is fulfilled inside the same message.
            "I'll explain the approach: the parser now walks the tree once.",
            // A quoted plan is not the model's own intention.
            "The issue says \"I will implement the retry path\" — that is already done.",
            // Attributed to another actor.
            "CI reported that I will need to update the lockfile.",
            // Forward intent with no work verb.
            "I'll be brief: the change is a one-liner.",
        ] {
            assert!(
                !states_remaining_work(answer),
                "should have stayed silent on: {answer}"
            );
        }
    }

    /// Each of the three in-sentence conditions must be load-bearing on its
    /// own — otherwise the conjunction that keeps this gate narrow is not
    /// actually doing the work the module doc claims it does.
    #[test]
    fn every_condition_is_load_bearing() {
        // marker + verb + no "you" -> fires.
        assert!(states_remaining_work("I'll implement the retry path"));
        // ...drop the marker.
        assert!(!states_remaining_work("The retry path needs implementing"));
        // ...drop the work verb.
        assert!(!states_remaining_work("I'll be around tomorrow"));
        // ...address the user.
        assert!(!states_remaining_work("I'll implement it if you want"));
    }

    const STATES_WORK: &str = "Parser is in. Next I'll implement the retry path.";

    #[test]
    fn fires_on_the_full_conjunction() {
        assert!(should_nudge_incomplete(
            GateMode::Advisory,
            0,
            0,
            true,
            STATES_WORK
        ));
    }

    /// Each condition alone must be able to silence the gate — otherwise the
    /// conjunction that keeps it narrow is decorative. One flipped input per
    /// case, against the firing baseline above.
    #[test]
    fn every_call_site_condition_is_load_bearing() {
        // Off is byte-identical to not having the gate.
        assert!(!should_nudge_incomplete(
            GateMode::Off,
            0,
            0,
            true,
            STATES_WORK
        ));
        // Advisory says it once.
        assert!(!should_nudge_incomplete(
            GateMode::Advisory,
            1,
            0,
            true,
            STATES_WORK
        ));
        // A read-only turn has no work of its own to be unfinished.
        assert!(!should_nudge_incomplete(
            GateMode::Advisory,
            0,
            0,
            false,
            STATES_WORK
        ));
        // Tracked todos outstanding: the todo gate owns that run.
        assert!(!should_nudge_incomplete(
            GateMode::Advisory,
            0,
            2,
            true,
            STATES_WORK
        ));
        // An answer that states nothing left.
        assert!(!should_nudge_incomplete(
            GateMode::Advisory,
            0,
            0,
            true,
            "Added the parser and wired up the tests. All checks pass."
        ));
    }

    #[test]
    fn blocking_re_enters_more_than_once() {
        for spent in 0..3 {
            assert!(
                should_nudge_incomplete(GateMode::Blocking, spent, 0, true, STATES_WORK),
                "blocking should still fire with {spent} spent"
            );
        }
        assert!(
            !should_nudge_incomplete(GateMode::Blocking, 3, 0, true, STATES_WORK),
            "and stop at the ceiling"
        );
    }

    /// Found by reviewing this module rather than by a failing test, which is
    /// the point of keeping them: a plain `contains` read `test` out of
    /// `latest`, `fix` out of `prefix`, and `port` out of `report`, `support`
    /// and `important`. Every one is an ordinary, complete sentence, and a gate
    /// that fires on those gets switched off.
    #[test]
    fn a_verb_inside_a_longer_word_does_not_count() {
        for answer in [
            "I'll use the latest version of the crate.",
            "I'll keep the prefix as-is.",
            "I'll report the numbers in the summary.",
            "I'll note that support for this is important.",
            "I'll take the transport layer as given.",
        ] {
            assert!(
                !states_remaining_work(answer),
                "substring match fired on: {answer}"
            );
        }
    }

    /// The other side: the boundary rule must not cost real matches. Verbs are
    /// listed as infinitives, so inflected forms have to keep counting.
    #[test]
    fn an_inflected_verb_still_counts() {
        for answer in [
            "I'll be implementing the retry path.",
            "I'll start wiring the handlers.",
            "I'll finish porting the last module.",
        ] {
            assert!(
                states_remaining_work(answer),
                "boundary rule dropped a real match: {answer}"
            );
        }
    }

    #[test]
    fn second_person_detection_is_word_anchored() {
        // `bayou` contains `you ` — silencing on it is the harmless direction,
        // but it is still a wrong reason.
        assert!(states_remaining_work("I'll implement the bayou parser."));
        // ...while a real handoff still suppresses.
        assert!(!states_remaining_work(
            "I'll implement it once you confirm."
        ));
    }

    #[test]
    fn an_empty_or_trivial_answer_is_silent() {
        assert!(!states_remaining_work(""));
        assert!(!states_remaining_work("Done."));
        assert!(!states_remaining_work("   \n  "));
    }
}
