//! Small "did you mean?" helper — nearest-name suggestions for the
//! cases a weaker model fumbles: a hallucinated tool name, an invalid
//! enum value, a mistyped path component. One dependency-free
//! Levenshtein and a single `closest` picker shared by all callers.

/// Levenshtein edit distance between two strings (unicode scalar
/// granularity). Iterative two-row DP — O(n·m) time, O(min) space is
/// not worth the complexity for the short identifiers we compare.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The single closest candidate to `target`, but only when it is a
/// *plausible* typo: within a distance budget that scales with the
/// target length, and strictly closer than the runner-up (an ambiguous
/// tie suggests nothing rather than guessing). Case-insensitive.
///
/// Returns the candidate in its original casing, or `None` when nothing
/// is close enough or the field is a toss-up.
pub fn closest<'a, I, S>(target: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a S>,
    S: AsRef<str> + 'a + ?Sized,
{
    let t = target.to_lowercase();
    // Budget: 1 edit for very short names, up to len/2 for longer ones,
    // capped so "read" → "grep" (distance 3) never matches.
    let budget = (t.chars().count() / 2).clamp(1, 3);

    let mut best: Option<(usize, &str)> = None;
    let mut second: Option<usize> = None;
    for cand in candidates {
        let c = cand.as_ref();
        let d = levenshtein(&t, &c.to_lowercase());
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
        Some((d, name)) if d <= budget && name != target => {
            // Reject ambiguous ties: if the runner-up is equally close,
            // we can't confidently point at one.
            if second == Some(d) { None } else { Some(name) }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn suggests_obvious_typo() {
        let tools = ["read", "write", "edit", "grep", "bash"];
        assert_eq!(closest("raed", &tools), Some("read"));
        assert_eq!(closest("edt", &tools), Some("edit"));
        assert_eq!(closest("Bash", &tools), Some("bash")); // case-insensitive
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
