//! The memory store as an editable document.
//!
//! `/memory` renders it; `/memory edit` hands the same text to `$EDITOR` and
//! applies whatever comes back. Reword a line to reword the memory, delete a
//! block to forget it, type a new one to add it.
//!
//! **Every entry is anchored on its uid**, printed in the document as
//! `[a1b2c3d4]`. That is not decoration. A dirge memory row carries far more
//! than its text — lineage, `created_at`, `use_count`, `confidence`, the
//! procedural success/failure counters that the post-session expectation pass
//! moves, and the supersession audit chain. Matching edited text back to rows
//! by content, or applying an edit as delete-then-recreate, would silently
//! reset all of it. With the uid we can UPDATE in place and keep it.
//!
//! A block with no uid is new. A uid present in the store but absent from the
//! document was deleted, and is tombstoned (not hard-deleted) so `restore`
//! still works — removing a line in an editor should not be more destructive
//! than the tool's own removal.

use crate::extras::memory_db::{BrowseEntry, SqliteMemoryStore};

/// How much of a uid to show. Stored uids are `urn:ump:<26 chars>`, which is
/// far too wide to put in front of every line — the anchor would out-shout
/// the memory. Rendered short and resolved by prefix, the same way session
/// ids are handled elsewhere.
const SHORT_UID_LEN: usize = 8;

fn short_uid(uid: &str) -> String {
    let tail = uid.rsplit(':').next().unwrap_or(uid);
    tail.chars().take(SHORT_UID_LEN).collect()
}

/// Resolve a rendered short id back to a stored entry.
///
/// Ambiguity is an error rather than a guess: picking one of two candidates
/// would edit the wrong memory, and the user would have no way to tell.
#[cfg(unix)]
fn resolve_uid<'a>(token: &str, stored: &'a [BrowseEntry]) -> Result<&'a BrowseEntry, String> {
    let matches: Vec<&BrowseEntry> = stored
        .iter()
        .filter(|e| e.uid == token || short_uid(&e.uid) == token)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "unknown memory id [{token}] — leave the ids as rendered, or drop the \
             whole `[id]` marker to add the line as a new memory"
        )),
        n => Err(format!("memory id [{token}] is ambiguous ({n} matches)")),
    }
}

#[cfg(unix)]
const HEADER: &str = "\
# dirge memory
#
# Reword a line to reword the memory. Delete a block to forget it (it is
# archived, not destroyed). Add a block to record something new.
#
# The [id] on each entry is what ties an edit back to the stored memory —
# leave it alone. A block without one is treated as new.
#
# Kinds: semantic, episodic, procedural, working, identity, overview.
";

/// What a parsed document says should happen.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg(unix)]
pub struct Plan {
    /// (uid, new content, kind) — reworded in place.
    pub updates: Vec<(String, String, Option<String>)>,
    /// uids present in the store but no longer in the document.
    pub removals: Vec<String>,
    /// (target, kind, content) — blocks with no uid.
    pub additions: Vec<(String, Option<String>, String)>,
    /// Entries whose text and kind were untouched.
    pub unchanged: usize,
}

#[cfg(unix)]
fn section_heading(target: &str) -> String {
    format!("## {target}")
}

/// Render the store. Grouped by target, uid-anchored, with the usage signal
/// as a trailing comment so it survives a round trip without being parsed
/// back — it is information for the reader, not an editable field.
#[cfg(unix)]
pub fn render(entries: &[BrowseEntry]) -> String {
    let mut out = String::from(HEADER);
    let mut current: Option<&str> = None;
    for entry in entries {
        if current != Some(entry.target.as_str()) {
            out.push_str(&format!("\n{}\n", section_heading(&entry.target)));
            current = Some(entry.target.as_str());
        }
        let usage = if entry.use_count > 0 {
            format!("  # used {}x", entry.use_count)
        } else {
            String::new()
        };
        let tier = if entry.tier == "breadcrumb" {
            "  # breadcrumb"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n[{}] [{}] {}{}{}\n",
            short_uid(&entry.uid),
            entry.kind,
            entry.content.trim(),
            usage,
            tier
        ));
    }
    out
}

/// A short, non-editable listing for `/memory` with no arguments.
pub fn summarize(entries: &[BrowseEntry]) -> Vec<String> {
    if entries.is_empty() {
        return vec!["no memories stored".to_string()];
    }
    let mut lines = Vec::new();
    let mut current: Option<&str> = None;
    for entry in entries {
        if current != Some(entry.target.as_str()) {
            lines.push(format!("{}:", entry.target));
            current = Some(entry.target.as_str());
        }
        let flags = match (entry.tier.as_str(), entry.use_count) {
            ("breadcrumb", _) => " (breadcrumb)".to_string(),
            (_, n) if n > 0 => format!(" ({n}x)"),
            _ => String::new(),
        };
        lines.push(format!(
            "  [{}] {} {}{}",
            short_uid(&entry.uid),
            entry.kind,
            one_line(&entry.content),
            flags
        ));
    }
    lines
}

fn one_line(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 96 {
        return flat;
    }
    format!("{}…", flat.chars().take(96).collect::<String>())
}

/// Diff an edited document against what is stored.
///
/// Errors rather than guessing: a block before any heading has no target, and
/// an unknown uid means the anchor was mangled — applying that as a fresh
/// insert would duplicate the memory it was meant to edit.
#[cfg(unix)]
pub fn parse(text: &str, stored: &[BrowseEntry]) -> Result<Plan, String> {
    let mut plan = Plan::default();
    let mut seen: Vec<String> = Vec::new();
    let mut target: Option<String> = None;

    for raw in text.lines() {
        let line = strip_comment(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if raw.trim_start().starts_with("## ") {
                target = Some(parse_heading(raw.trim())?);
            }
            continue;
        }
        if raw.trim_start().starts_with('#') {
            if raw.trim_start().starts_with("## ") {
                target = Some(parse_heading(raw.trim())?);
            }
            continue;
        }

        // The first bracket is the uid — unless it names a kind. The header
        // invites `[identity] something new` for a fresh entry, and a user
        // following it should not be told their kind is an unknown id. Kinds
        // are a closed set, so this is decidable rather than a guess.
        let (mut uid, mut rest) = split_bracket(trimmed);
        if uid
            .as_deref()
            .and_then(crate::extras::memory_db::parse_kind)
            .is_some()
        {
            rest = trimmed;
            uid = None;
        }
        let (kind, content) = split_bracket(rest.trim());
        let content = content.trim().to_string();
        if content.is_empty() {
            continue;
        }

        match uid {
            Some(token) => {
                let existing = resolve_uid(&token, stored)?;
                seen.push(existing.uid.clone());
                let kind_changed = kind.as_deref().is_some_and(|k| k != existing.kind);
                if existing.content.trim() != content || kind_changed {
                    // Only forward the kind when it actually changed. The
                    // document always echoes it back, and the store treats a
                    // supplied kind as a re-classification — which resets the
                    // procedural success/failure counters. Passing it
                    // unconditionally meant a plain rewording silently wiped
                    // the track record this whole module exists to preserve.
                    let kind = if kind_changed { kind } else { None };
                    plan.updates.push((existing.uid.clone(), content, kind));
                } else {
                    plan.unchanged += 1;
                }
            }
            None => {
                let target = target.clone().ok_or_else(|| {
                    format!(
                        "entry {content:?} appears before any `## memory` heading — \
                         add one above it so it has a home"
                    )
                })?;
                plan.additions.push((target, kind, content));
            }
        }
    }

    for entry in stored {
        if !seen.contains(&entry.uid) {
            plan.removals.push(entry.uid.clone());
        }
    }
    Ok(plan)
}

#[cfg(unix)]
fn parse_heading(line: &str) -> Result<String, String> {
    match line.strip_prefix("## ").map(str::trim) {
        Some("memory") => Ok("memory".to_string()),
        Some("pitfalls") => Ok("pitfalls".to_string()),
        _ => Err(format!(
            "unrecognized heading {line:?} — expected `## memory` or `## pitfalls`"
        )),
    }
}

/// Strip a trailing ` # ...` annotation (the usage/tier hints `render` adds).
/// Only after at least one non-space character, so a `#` opening the line is
/// still a comment.
#[cfg(unix)]
fn strip_comment(line: &str) -> &str {
    match line.find("  # ") {
        Some(idx) if !line[..idx].trim().is_empty() => &line[..idx],
        _ => line,
    }
}

/// Pull a leading `[token]` off a line.
#[cfg(unix)]
fn split_bracket(line: &str) -> (Option<String>, &str) {
    if let Some(rest) = line.strip_prefix('[')
        && let Some((token, tail)) = rest.split_once(']')
    {
        return (Some(token.trim().to_string()), tail);
    }
    (None, line)
}

/// What applying a plan did.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg(unix)]
pub struct ApplyReport {
    pub updated: usize,
    pub removed: usize,
    pub added: usize,
    pub unchanged: usize,
    pub failures: Vec<String>,
}

#[cfg(unix)]
impl ApplyReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.updated > 0 {
            parts.push(format!("{} reworded", self.updated));
        }
        if self.added > 0 {
            parts.push(format!("{} added", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("{} forgotten", self.removed));
        }
        if parts.is_empty() {
            return "no changes".to_string();
        }
        let mut summary = parts.join(", ");
        if !self.failures.is_empty() {
            summary.push_str(&format!(", {} failed", self.failures.len()));
        }
        summary
    }
}

/// Apply a plan. Each operation is independent: one rejected entry (a
/// duplicate, something over budget) is reported and the rest still apply,
/// because failing the whole edit would throw away every other correction the
/// user just made.
#[cfg(unix)]
pub fn apply(store: &SqliteMemoryStore, plan: &Plan) -> ApplyReport {
    let mut report = ApplyReport {
        unchanged: plan.unchanged,
        ..Default::default()
    };

    for (uid, content, kind) in &plan.updates {
        let parsed = kind
            .as_deref()
            .and_then(crate::extras::memory_db::parse_kind);
        match store.replace_entry_by_uid(uid, content, parsed) {
            Ok(()) => report.updated += 1,
            Err(e) => report.failures.push(format!("[{uid}]: {e}")),
        }
    }
    for (target, kind, content) in &plan.additions {
        let parsed = kind
            .as_deref()
            .and_then(crate::extras::memory_db::parse_kind);
        match store.add_entry(target, content, parsed) {
            Ok(_) => report.added += 1,
            Err(e) => report
                .failures
                .push(format!("{:?}: {e}", one_line(content))),
        }
    }
    // Removals last: an edit that both reworded and deleted should not lose
    // the rewording because a delete failed first.
    for uid in &plan.removals {
        match store.remove_entry_by_uid(uid) {
            Ok(()) => report.removed += 1,
            Err(e) => report.failures.push(format!("[{uid}]: {e}")),
        }
    }
    report
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn entry(uid: &str, content: &str) -> BrowseEntry {
        BrowseEntry {
            uid: uid.into(),
            target: "memory".into(),
            kind: "semantic".into(),
            content: content.into(),
            tier: "hot".into(),
            use_count: 0,
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn an_untouched_document_changes_nothing() {
        let stored = vec![entry("aaa1", "first"), entry("bbb2", "second")];
        let plan = parse(&render(&stored), &stored).unwrap();
        assert_eq!(plan.unchanged, 2);
        assert!(plan.updates.is_empty());
        assert!(plan.removals.is_empty());
        assert!(plan.additions.is_empty());
    }

    #[test]
    fn rewording_updates_in_place() {
        let stored = vec![entry("aaa1", "first")];
        let doc = render(&stored).replace("first", "first, corrected");
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(
            plan.updates,
            vec![("aaa1".into(), "first, corrected".into(), None)]
        );
        assert!(plan.removals.is_empty());
    }

    /// The store resets procedural outcome counters on a re-classification.
    /// A rewording must therefore NOT forward an unchanged kind.
    #[test]
    fn rewording_does_not_forward_an_unchanged_kind() {
        let stored = vec![entry("aaa1", "first")];
        let doc = render(&stored).replace("first", "first, corrected");
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(
            plan.updates[0].2, None,
            "unchanged kind would reset counters"
        );
    }

    #[test]
    fn deleting_a_block_forgets_it() {
        let stored = vec![entry("aaa1", "keep me"), entry("bbb2", "drop me")];
        let doc = render(&stored)
            .lines()
            .filter(|l| !l.contains("drop me"))
            .collect::<Vec<_>>()
            .join("\n");
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(plan.removals, vec!["bbb2".to_string()]);
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn a_block_without_an_id_is_new() {
        let stored = vec![entry("aaa1", "existing")];
        let doc = format!("{}\n[procedural] something new\n", render(&stored));
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(
            plan.additions,
            vec![(
                "memory".to_string(),
                Some("procedural".to_string()),
                "something new".to_string()
            )]
        );
        assert!(plan.removals.is_empty());
    }

    /// The header invites `[identity] ...` for a new entry. Reading that
    /// leading bracket as a uid told the user their kind was an unknown id.
    #[test]
    fn a_new_entry_may_lead_with_its_kind() {
        let stored = vec![entry("aaa1", "existing")];
        let doc = format!("{}\n[identity] added by hand\n", render(&stored));
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(
            plan.additions,
            vec![(
                "memory".to_string(),
                Some("identity".to_string()),
                "added by hand".to_string()
            )]
        );
        assert!(plan.removals.is_empty(), "the existing entry must survive");
    }

    #[test]
    fn changing_the_kind_counts_as_an_update() {
        let stored = vec![entry("aaa1", "first")];
        let doc = render(&stored).replace("[semantic]", "[procedural]");
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(plan.updates.len(), 1);
        assert_eq!(plan.updates[0].2, Some("procedural".to_string()));
    }

    /// The usage/tier hints are output, not input — they must not come back
    /// as part of the memory text.
    #[test]
    fn trailing_annotations_are_not_part_of_the_content() {
        let mut e = entry("aaa1", "a fact");
        e.use_count = 7;
        let stored = vec![e];
        let doc = render(&stored);
        assert!(doc.contains("# used 7x"));
        let plan = parse(&doc, &stored).unwrap();
        assert_eq!(plan.unchanged, 1, "annotation leaked into the content");
    }

    /// Silently treating a mangled id as a new memory would duplicate the
    /// entry it was meant to edit.
    #[test]
    fn an_unknown_id_is_an_error() {
        let stored = vec![entry("aaa1", "first")];
        let err = parse("## memory\n\n[zzz9] [semantic] first\n", &stored).unwrap_err();
        assert!(err.contains("unknown memory id"), "{err}");
    }

    #[test]
    fn an_entry_before_any_heading_is_an_error() {
        let err = parse("[semantic] homeless\n", &[]).unwrap_err();
        assert!(err.contains("before any"), "{err}");
    }

    #[test]
    fn an_empty_document_forgets_everything() {
        let stored = vec![entry("aaa1", "first"), entry("bbb2", "second")];
        let plan = parse("", &stored).unwrap();
        assert_eq!(plan.removals.len(), 2);
    }

    #[test]
    fn summary_reads_naturally() {
        let report = ApplyReport {
            updated: 1,
            added: 2,
            removed: 1,
            unchanged: 4,
            failures: vec![],
        };
        assert_eq!(report.summary(), "1 reworded, 2 added, 1 forgotten");
        assert_eq!(ApplyReport::default().summary(), "no changes");
    }

    #[test]
    fn pitfalls_round_trip_under_their_own_heading() {
        let mut e = entry("ccc3", "never force push");
        e.target = "pitfalls".into();
        let stored = vec![entry("aaa1", "a fact"), e];
        let doc = render(&stored);
        assert!(doc.contains("## pitfalls"));
        assert_eq!(parse(&doc, &stored).unwrap().unchanged, 2);
    }
}
