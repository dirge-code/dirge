//! Small "did you mean?" helper — nearest-name suggestions for the
//! cases a weaker model fumbles: a mistyped tool name, an invalid
//! enum value, a mistyped path component. One dependency-free edit
//! distance and a single `closest` picker shared by all callers.
//!
//! TYPOS ONLY. A name that is a different WORD for the right tool is not this
//! module's problem and cannot be solved by measuring characters — see
//! [`super::tool_aliases`], which runs first.

/// Edit distance between two strings (unicode scalar granularity), counting a
/// transposition of adjacent characters as ONE mistake — optimal string
/// alignment, the restricted Damerau-Levenshtein.
///
/// Plain Levenshtein charges 2 for `raed`/`read`, which is the single most
/// common way a name gets mistyped. Paying for the transposition row is what
/// lets the budget in [`closest`] be tight enough to stop matching unrelated
/// names (dirge-e31n.8) without losing real typos.
///
/// Three rows rather than two, since a transposition reaches back two.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let mut best = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            if i > 0 && j > 0 && ca == b[j - 1] && a[i - 1] == cb {
                best = best.min(prev2[j - 1] + 1);
            }
            cur[j + 1] = best;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The single closest candidate to `target`, but only when it is a
/// *plausible* typo: within a distance budget that scales with the
/// names being compared, and strictly closer than the runner-up (an ambiguous
/// tie suggests nothing rather than guessing). Case-insensitive.
///
/// Returns the candidate in its original casing, or `None` when nothing
/// is close enough or the field is a toss-up.
///
/// # Why the budget is a fraction, not a length ladder (dirge-e31n.8)
///
/// It used to be `len/2`, capped at 3 — half the target could differ and still
/// read as a typo. Against dirge's own tool names that produced confident
/// nonsense: `exec` → `spec`, `shell` → `skill`, `open` → `spec`, `ls` → `lsp`,
/// `ask` → `task`, `search` → `websearch`. Six of the eleven guesses it
/// resolved pointed at a tool with nothing to do with the one asked for, and
/// the message says "Did you mean `spec`?" with no hedge — so the harness
/// steers a model that wanted a shell toward a spec-management tool, then
/// scores the resulting flailing as the model being out of its depth.
///
/// A quarter of the LONGER name, with the transposition-aware distance above,
/// takes that sample from 8 wrong suggestions to 1 while keeping every real
/// typo (`raed`, `edt`, `wrtie`, `grpe`, `bahs`, `writ`). Measured, not
/// tuned by eye — see `docs/tool-name-misses.md`.
///
/// Synonyms are NOT this function's job and never were: `shell` is not a
/// mistyped `bash`, it is a different word for it. Those are resolved by name
/// before anything gets here — see [`super::tool_aliases`].
pub fn closest<'a, I, S>(target: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a S>,
    S: AsRef<str> + 'a + ?Sized,
{
    let t = target.to_lowercase();

    let mut best: Option<(usize, &str)> = None;
    let mut second: Option<usize> = None;
    for cand in candidates {
        let c = cand.as_ref();
        let d = edit_distance(&t, &c.to_lowercase());
        match best {
            None => best = Some((d, c)),
            Some((bd, _)) if d < bd => {
                second = Some(bd);
                best = Some((d, c));
            }
            Some(_) => {
                if second.is_none_or(|s| d < s) {
                    second = Some(d);
                }
            }
        }
    }

    match best {
        // `name != target` (case-sensitive) rather than `d > 0`: a
        // wrong-CASE name lowercases to distance 0 but is still worth
        // suggesting, since dispatch matches case-sensitively.
        Some((d, name)) if d <= typo_budget(&t, name) && name != target => {
            // Reject ambiguous ties: if the runner-up is equally close,
            // we can't confidently point at one.
            if second == Some(d) { None } else { Some(name) }
        }
        _ => None,
    }
}

/// Edits allowed between two names before the match stops being a typo.
///
/// A quarter of the longer name, rounded down — so a 3-character name tolerates
/// nothing (one edit on `ls` is a different word, not a slip) and `apply_patch`
/// tolerates two.
fn typo_budget(target: &str, candidate: &str) -> usize {
    target.chars().count().max(candidate.chars().count()) / 4
}

/// Record that a model named a tool the run does not have (dirge-e31n.8).
///
/// There are two ways to miss and they behave nothing alike: a native call is
/// rejected with "Tool X not found" and the model can retry, while a call
/// written as text is dropped silently and nobody ever hears about it. Both
/// log here, tagged by `path`, because the question this exists to answer —
/// *which names do models actually reach for* — cannot be answered from
/// either half alone.
///
/// One line per miss on `dirge::tool_miss`, carrying the nearest real name so
/// the log says whether the miss was a typo (something `closest` catches) or a
/// synonym (which nothing catches today).
pub fn log_tool_name_miss<'a, I, S>(name: &str, candidates: I, path: &'static str)
where
    I: IntoIterator<Item = &'a S>,
    S: AsRef<str> + 'a + ?Sized,
{
    tracing::info!(
        target: "dirge::tool_miss",
        tool = %name,
        nearest = closest(name, candidates).unwrap_or("-"),
        path,
        "model named a tool that does not exist",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    /// The reason this is not plain Levenshtein: swapping two adjacent keys is
    /// one mistake, and charging 2 for it is what forced the budget wide
    /// enough to match unrelated names.
    #[test]
    fn a_transposition_costs_one() {
        assert_eq!(edit_distance("raed", "read"), 1);
        assert_eq!(edit_distance("wrtie", "write"), 1);
        // Non-adjacent swaps are still two edits — the discount is for
        // adjacent pairs only, or every 4-letter word is one edit from
        // every anagram of itself.
        assert_eq!(edit_distance("eadr", "read"), 2);
    }

    #[test]
    fn suggests_obvious_typo() {
        let tools = ["read", "write", "edit", "grep", "bash"];
        assert_eq!(closest("raed", &tools), Some("read"));
        assert_eq!(closest("edt", &tools), Some("edit"));
        assert_eq!(closest("Bash", &tools), Some("bash")); // case-insensitive
    }

    /// dirge-e31n.8, and the reason the budget changed. Every one of these was
    /// a confident suggestion under `len/2`, and every one points at a tool
    /// with nothing to do with what was asked for — `spec` manages specs,
    /// `skill` loads skills, `lsp` talks to a language server. A model told
    /// "Did you mean `spec`?" when it wanted a shell is being sent the wrong
    /// way by the harness, which then scores the flailing as its own fault.
    #[test]
    fn an_unrelated_tool_is_never_suggested() {
        let tools = crate::agent::tools::BUILTIN_TOOL_NAMES;
        for guess in ["exec", "shell", "open", "ls", "search", "cat", "view"] {
            assert_eq!(
                closest(guess, tools),
                None,
                "{guess} resolved to an unrelated tool",
            );
        }
    }

    /// The other half, so the rule above cannot be satisfied by suggesting
    /// nothing ever: real typos of real dirge tools still resolve.
    #[test]
    fn real_typos_of_real_tools_still_resolve() {
        let tools = crate::agent::tools::BUILTIN_TOOL_NAMES;
        for (guess, want) in [
            ("raed", "read"),
            ("wrtie", "write"),
            ("grpe", "grep"),
            ("bahs", "bash"),
            ("aply_patch", "apply_patch"),
            ("websearh", "websearch"),
            ("list_dirs", "list_dir"),
        ] {
            assert_eq!(closest(guess, tools), Some(want), "{guess}");
        }
    }

    #[test]
    fn no_suggestion_when_nothing_close() {
        let tools = ["read", "write", "edit"];
        // A wholly different word shouldn't map onto anything.
        assert_eq!(closest("search_filesystem", &tools), None);
    }

    #[test]
    fn distant_real_synonym_is_not_suggested() {
        // "view" vs "read" is a synonym a model might pick, but it's
        // edit-distance 4 — we don't want to claim it's a typo of read.
        let tools = ["read", "grep", "bash"];
        assert_eq!(closest("view", &tools), None);
    }

    #[test]
    fn exact_match_returns_none() {
        // An exact hit isn't a "did you mean" — caller handles it.
        let tools = ["read", "write"];
        assert_eq!(closest("read", &tools), None);
    }

    #[test]
    fn ambiguous_tie_suggests_nothing() {
        // "cat" is distance 1 from both "bat" and "car" — don't guess.
        let words = ["bat", "car"];
        assert_eq!(closest("cat", &words), None);
    }

    #[test]
    fn works_over_owned_strings() {
        let tools: Vec<String> = vec!["read".into(), "write".into()];
        assert_eq!(closest("writ", &tools), Some("write"));
    }
}
