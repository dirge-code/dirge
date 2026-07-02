//! UTF-8-safe string truncation helpers (dirge-fb8t).
//!
//! Many display / error / telemetry sites cut a string at a fixed BYTE
//! offset (`&s[..n]`). That panics — "byte index N is not a char
//! boundary" — the instant a multibyte codepoint (CJK, emoji, an accented
//! filename) straddles the cut, and the input is routinely
//! model/user/path-controlled. These helpers floor/ceil the cut to a char
//! boundary and never panic.

/// Largest PREFIX of `s` that fits in `max_bytes` and ends on a char
/// boundary. Returns all of `s` when it's already within budget. Never
/// panics.
pub(crate) fn head(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Largest byte index `<= n` that's on a UTF-8 char boundary. Use to floor a
/// head-cut so slicing never panics on a multibyte split.
pub(crate) fn char_boundary_at_or_before(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    let mut i = n;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest byte index `>= n` that's on a UTF-8 char boundary. Use to ceil a
/// tail-cut so slicing never panics on a multibyte split.
pub(crate) fn char_boundary_at_or_after(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    let mut i = n;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Cap `s` to at most `max_bytes` bytes (UTF-8-safe), appending `…` when it had
/// to truncate. The `…` (3 bytes) fits within the budget. Returns `s` unchanged
/// when already within budget. For DISPLAY / error text only — not model-facing
/// (use the head/tail truncators for prompt context, which carry instructive
/// markers).
pub(crate) fn ellipsize(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    format!("{}…", head(s, max_bytes.saturating_sub('…'.len_utf8())))
}

/// First line of `content`, capped at 80 chars (77 + `...`). The
/// preview shape shared by memory breadcrumb indexes and curator
/// audit reports (dirge-rwrg — was duplicated byte-for-byte in
/// memory_db and memory_curator). Char-based, UTF-8 safe.
pub(crate) fn first_line_preview(content: &str) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 80 {
        first.to_string()
    } else {
        let cut: String = first.chars().take(77).collect();
        format!("{cut}...")
    }
}

/// Short display prefix of an id — its first 8 characters. Used across the UI
/// to compact session / subagent / notification / plugin ids for display.
/// Char-based (UTF-8 safe) and consistent everywhere, replacing the ~9
/// scattered `id.chars().take(8).collect()` copies.
pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A compact but *distinct* session id for glance UI (the status footer).
///
/// `short_id`'s fixed 8-char head collapses every `compacted-<uuid>` /
/// `forked-<uuid>` session to its shared word prefix ("compacte"), so two such
/// sessions look identical in the footer. When the id has an alphabetic
/// `word-` prefix, keep the prefix AND show the first 6 chars of the following
/// segment (the unique uuid head); a bare uuid keeps the first-8 behavior. For
/// the FULL id (e.g. to resume with `dirge --session <id>`), use `/sessions
/// current`.
pub(crate) fn session_glance_id(id: &str) -> String {
    if let Some((prefix, rest)) = id.split_once('-')
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphabetic())
        && !rest.is_empty()
    {
        let head: String = rest.chars().take(6).collect();
        return format!("{prefix}-{head}");
    }
    short_id(id)
}

/// The canonical Session-DB primary key for a runtime session id. MUST be
/// used by every DB writer (turn persistence, session-end, the fold
/// handler's insert + parent link) or a session's turns and its lineage
/// land in different rows.
///
/// The old `format!("dirge-{}", short_id(id))` collapsed every
/// `compacted-<uuid>` fold to the constant `dirge-compacte`, so all
/// folded sessions in every conversation piled into one row and
/// `resolve_parent` could never walk a fold (dirge-g1ze). Reuses
/// `session_glance_id`, which keeps a `word-` prefix plus the unique tail
/// head for rotated ids while leaving bare-uuid keys byte-identical to
/// the old derivation (so existing rows still resolve).
pub(crate) fn db_session_id(id: &str) -> String {
    format!("dirge-{}", session_glance_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_ascii_within_and_over_budget() {
        assert_eq!(head("hello", 10), "hello");
        assert_eq!(head("hello", 3), "hel");
        assert_eq!(head("hello", 0), "");
    }

    #[test]
    fn session_glance_id_distinguishes_prefixed_sessions() {
        // Compacted/forked sessions share a word prefix — keep it plus the
        // unique uuid head instead of collapsing to "compacte".
        assert_eq!(
            session_glance_id("compacted-3f9a2b1c4d5e"),
            "compacted-3f9a2b"
        );
        assert_eq!(session_glance_id("forked-abc1234567"), "forked-abc123");
        // Bare uuid (simple form, no internal '-') keeps the first-8 head.
        assert_eq!(session_glance_id("3f9a2b1c4d5e6f70"), "3f9a2b1c");
        // Hyphenated uuid: first group is hex (not all-alpha) → first-8 head,
        // not mistaken for a word prefix.
        assert_eq!(session_glance_id("3f9a2b1c-4d5e-6f70"), "3f9a2b1c");
    }

    #[test]
    fn db_session_id_is_distinct_across_folds() {
        // dirge-g1ze: the DB key derived via `short_id` collapsed every
        // `compacted-<uuid>` fold to "compacte", so all folded sessions in
        // every conversation piled into one row. The canonical DB id must
        // stay distinct across folds and forks.
        assert_ne!(
            db_session_id("compacted-3f9a2b1c"),
            db_session_id("compacted-9e8d7c6b"),
            "different folds must map to different DB ids"
        );
        assert_ne!(
            db_session_id("forked-aaaa1111"),
            db_session_id("forked-bbbb2222")
        );
        // Stable: same input → same key (writers + parent links must agree).
        assert_eq!(
            db_session_id("compacted-3f9a2b1c"),
            db_session_id("compacted-3f9a2b1c")
        );
        // Every key carries the `dirge-` DB namespace prefix.
        assert!(db_session_id("3f9a2b1c4d5e").starts_with("dirge-"));
    }

    #[test]
    fn head_never_splits_a_multibyte_char() {
        // "é" is 2 bytes; cutting at byte 1 would split it.
        let s = "café"; // c a f é  -> bytes: 1 1 1 2 = 5
        assert_eq!(head(s, 4), "caf"); // byte 4 is mid-'é' -> floor to 3
        assert_eq!(head(s, 5), "café");
        // CJK (3 bytes each) and emoji (4 bytes) crossing the cut.
        let cjk = "日本語"; // 9 bytes
        assert_eq!(head(cjk, 4), "日"); // floor 4 -> 3
        assert_eq!(head(cjk, 3), "日");
        let emoji = "a😀b"; // 1 + 4 + 1
        assert_eq!(head(emoji, 3), "a"); // floor 3 -> 1 (😀 is bytes 1..5)
        assert_eq!(head(emoji, 5), "a😀");
    }

    /// The exact reported panic shape: a 3-byte char straddling a fixed
    /// byte cut must not panic.
    #[test]
    fn head_handles_multibyte_straddling_a_large_cut() {
        let mut s = "a".repeat(199);
        s.push('世'); // 3 bytes spanning offsets 199..202
        s.push_str(&"b".repeat(50));
        let cut = head(&s, 200); // byte 200 is mid-'世'
        assert!(s.starts_with(cut));
        assert_eq!(cut.len(), 199, "floored below the multibyte char");
    }

    #[test]
    fn ellipsize_caps_and_appends_marker_utf8_safe() {
        assert_eq!(ellipsize("short", 100), "short"); // within budget → unchanged
        // 3-byte '…' reserved within the budget.
        assert_eq!(ellipsize("abcdefgh", 6), "abc…");
        // Never splits a multibyte char: budget 7 reserves 3 for '…', leaves 4
        // bytes → one full CJK char (3 bytes), floored off the 2nd.
        assert_eq!(ellipsize("日本語", 7), "日…");
    }

    #[test]
    fn char_boundary_helpers_floor_and_ceil() {
        let cjk = "日本語"; // each char is 3 bytes
        assert_eq!(char_boundary_at_or_before(cjk, 4), 3); // mid-'本' → floor to 3
        assert_eq!(char_boundary_at_or_after(cjk, 4), 6); // mid-'本' → ceil to 6
        assert_eq!(char_boundary_at_or_before(cjk, 99), cjk.len());
    }
}
