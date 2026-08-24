//! Human review of queued memory writes.
//!
//! With `memory.confirm_writes` on, `memory add` queues instead of storing
//! (see `SqliteMemoryStore::add_pending`). This renders the queue as a
//! markdown document, hands it to `$EDITOR`, and applies whatever comes
//! back.
//!
//! **The file is the desired final state of the batch**, not a diff. On
//! apply, the queue is dropped and exactly what the document contains is
//! inserted through the normal `add_entry` path. That collapses reject
//! (delete the block), edit (change the text) and add (type a new one) into
//! one operation, with no ids to track and nothing to reconcile — and it is
//! what makes "add the thing the model missed" a first-class action rather
//! than a bolted-on extra.
//!
//! A document that will not parse aborts the whole apply and leaves the
//! queue untouched. Half-applying a mangled review would lose memories with
//! no way to tell which.

#[cfg(unix)]
use crate::extras::memory_db::PendingEntry;
use crate::extras::memory_db::SqliteMemoryStore;

#[cfg(unix)]
const HEADER: &str = "\
# Memory review
#
# Delete a block to reject it. Edit the text freely. Add your own by
# writing a new block under the right heading.
#
# A block is `[kind] text`, continued on indented lines. Kinds:
# semantic, episodic, procedural, working, identity, overview.
#
# Saving with everything deleted rejects everything. Quitting the editor
# without saving (:cq) changes nothing.
";

/// Where an entry lives: which store, which target within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(unix)]
pub struct Section {
    pub global: bool,
    /// `"memory"` or `"pitfalls"`.
    pub target: &'static str,
}

#[cfg(unix)]
impl Section {
    fn heading(&self) -> String {
        format!(
            "## {} · {}",
            if self.global { "global" } else { "project" },
            self.target
        )
    }

    fn parse_heading(line: &str) -> Option<Section> {
        let rest = line.strip_prefix("## ")?;
        // Accept the ASCII separator too — a middle dot is easy to mangle
        // when retyping a heading by hand.
        let (scope, target) = rest.split_once(" · ").or_else(|| rest.split_once(" - "))?;
        let global = match scope.trim() {
            "global" => true,
            "project" => false,
            _ => return None,
        };
        let target = match target.trim() {
            "memory" => "memory",
            "pitfalls" => "pitfalls",
            _ => return None,
        };
        Some(Section { global, target })
    }
}

/// One reviewed entry, as parsed back out of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(unix)]
pub struct ReviewedEntry {
    pub section: Section,
    pub kind: Option<String>,
    pub content: String,
}

/// Render the queue for editing. `entries` is `(section, pending)` pairs.
#[cfg(unix)]
pub fn render(entries: &[(Section, PendingEntry)]) -> String {
    let mut out = String::from(HEADER);
    let mut current: Option<Section> = None;
    for (section, entry) in entries {
        if current != Some(*section) {
            out.push_str(&format!("\n{}\n", section.heading()));
            current = Some(*section);
        }
        out.push_str(&format!("\n[{}] {}\n", entry.kind, entry.content.trim()));
    }
    out
}

/// Parse an edited document back into entries.
///
/// Errors rather than guessing: a block that appears before any heading has
/// no home, and an unrecognized heading would silently swallow everything
/// under it.
#[cfg(unix)]
pub fn parse(text: &str) -> Result<Vec<ReviewedEntry>, String> {
    let mut out: Vec<ReviewedEntry> = Vec::new();
    let mut section: Option<Section> = None;
    let mut pending: Option<(Option<String>, Vec<String>)> = None;

    fn flush(
        out: &mut Vec<ReviewedEntry>,
        section: Option<Section>,
        block: Option<(Option<String>, Vec<String>)>,
    ) -> Result<(), String> {
        let Some((kind, lines)) = block else {
            return Ok(());
        };
        let content = lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if content.is_empty() {
            return Ok(());
        }
        let Some(section) = section else {
            return Err(format!(
                "entry {content:?} appears before any `## project · memory` heading — \
                 add one above it so it has a home"
            ));
        };
        out.push(ReviewedEntry {
            section,
            kind,
            content,
        });
        Ok(())
    }

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') {
            if trimmed.starts_with("## ") {
                flush(&mut out, section, pending.take())?;
                section = Some(Section::parse_heading(trimmed).ok_or_else(|| {
                    format!(
                        "unrecognized heading {trimmed:?} — expected \
                         `## project · memory`, `## project · pitfalls`, \
                         `## global · memory` or `## global · pitfalls`"
                    )
                })?);
            }
            // Any other `#` line is a comment.
            continue;
        }
        if trimmed.trim().is_empty() {
            flush(&mut out, section, pending.take())?;
            continue;
        }
        // An indented line continues the block above it.
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some((_, lines)) = pending.as_mut()
        {
            lines.push(trimmed.trim().to_string());
            continue;
        }
        flush(&mut out, section, pending.take())?;
        let (kind, rest) = split_kind(trimmed.trim());
        pending = Some((kind, vec![rest.to_string()]));
    }
    flush(&mut out, section, pending.take())?;
    Ok(out)
}

/// Split a leading `[kind]` marker off a block's first line. A block with no
/// marker is accepted with `kind = None` so a human can type a bare line and
/// let the store pick the default.
#[cfg(unix)]
fn split_kind(line: &str) -> (Option<String>, &str) {
    if let Some(rest) = line.strip_prefix('[')
        && let Some((kind, tail)) = rest.split_once(']')
    {
        return (Some(kind.trim().to_lowercase()), tail.trim_start());
    }
    (None, line)
}

/// What an applied review did.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg(unix)]
pub struct ApplyReport {
    pub stored: usize,
    pub rejected: usize,
    /// Entries the store refused (duplicate, oversized). Reported rather
    /// than swallowed — the human wrote these and deserves to know.
    pub failures: Vec<String>,
}

#[cfg(unix)]
impl ApplyReport {
    pub fn summary(&self) -> String {
        let mut s = format!("{} stored, {} rejected", self.stored, self.rejected);
        if !self.failures.is_empty() {
            s.push_str(&format!(", {} failed", self.failures.len()));
        }
        s
    }
}

/// Store the reviewed set and drop the queue.
///
/// Insertion goes through `add_entry`, so budget compaction, salience
/// defaults, threat scanning and FTS indexing behave exactly as they do for
/// an unreviewed write — an entry the human typed is indistinguishable from
/// one the model proposed, which is the point.
#[cfg(unix)]
pub fn apply(
    project: &SqliteMemoryStore,
    global: Option<&SqliteMemoryStore>,
    reviewed: &[ReviewedEntry],
    queued: usize,
) -> ApplyReport {
    let mut report = ApplyReport::default();
    for entry in reviewed {
        let store = if entry.section.global {
            match global {
                Some(g) => g,
                None => {
                    report.failures.push(format!(
                        "{:?}: no global memory store is configured",
                        truncate(&entry.content)
                    ));
                    continue;
                }
            }
        } else {
            project
        };
        let kind = entry
            .kind
            .as_deref()
            .and_then(crate::extras::memory_db::parse_kind);
        match store.add_entry(entry.section.target, &entry.content, kind) {
            Ok(_) => report.stored += 1,
            Err(e) => report
                .failures
                .push(format!("{:?}: {e}", truncate(&entry.content))),
        }
    }
    report.rejected = queued.saturating_sub(report.stored + report.failures.len());

    // Cleared last: if an insert panicked we would rather re-review the
    // batch than lose it.
    let _ = project.clear_pending();
    if let Some(g) = global {
        let _ = g.clear_pending();
    }
    report
}

#[cfg(unix)]
fn truncate(s: &str) -> String {
    if s.chars().count() <= 48 {
        return s.to_string();
    }
    format!("{}…", s.chars().take(48).collect::<String>())
}

/// Tell the user the queue is waiting, if it is.
///
/// Called after the post-session passes (which are the writes nobody sees
/// happen) and once at startup. Without it, `confirm_writes` silently turns
/// the agent into one that never learns: it keeps proposing, nothing is
/// ever stored, and there is no sign anything is pending.
pub fn notify_if_queued(paths: &crate::extras::dirge_paths::ProjectPaths) {
    let project = SqliteMemoryStore::load(paths).ok();
    let global = SqliteMemoryStore::load_global().ok();
    let count = project.as_ref().map(|s| s.pending_count()).unwrap_or(0)
        + global.as_ref().map(|s| s.pending_count()).unwrap_or(0);
    if count == 0 {
        return;
    }
    let plural = if count == 1 { "memory" } else { "memories" };
    crate::ui::notifications::notify_send(crate::ui::notifications::Notification::Info(format!(
        "{count} {plural} awaiting review — /memory review"
    )));
}

/// Collect the queue from both stores, in a stable order.
#[cfg(unix)]
pub fn collect(
    project: &SqliteMemoryStore,
    global: Option<&SqliteMemoryStore>,
) -> Vec<(Section, PendingEntry)> {
    let mut out: Vec<(Section, PendingEntry)> = Vec::new();
    let mut push = |is_global: bool, store: &SqliteMemoryStore| {
        for entry in store.list_pending().unwrap_or_default() {
            let target = if entry.target == "pitfalls" {
                "pitfalls"
            } else {
                "memory"
            };
            out.push((
                Section {
                    global: is_global,
                    target,
                },
                entry,
            ));
        }
    };
    push(false, project);
    if let Some(g) = global {
        push(true, g);
    }
    out.sort_by_key(|(s, _)| (s.global, s.target));
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn pending(kind: &str, content: &str) -> PendingEntry {
        PendingEntry {
            id: 1,
            target: "memory".into(),
            kind: kind.into(),
            content: content.into(),
        }
    }

    const PROJECT_MEMORY: Section = Section {
        global: false,
        target: "memory",
    };
    const GLOBAL_PITFALLS: Section = Section {
        global: true,
        target: "pitfalls",
    };

    #[test]
    fn render_parse_round_trip() {
        let entries = vec![
            (PROJECT_MEMORY, pending("semantic", "the wiki repo is x/y")),
            (GLOBAL_PITFALLS, pending("procedural", "never push to main")),
        ];
        let parsed = parse(&render(&entries)).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].section, PROJECT_MEMORY);
        assert_eq!(parsed[0].kind.as_deref(), Some("semantic"));
        assert_eq!(parsed[0].content, "the wiki repo is x/y");
        assert_eq!(parsed[1].section, GLOBAL_PITFALLS);
        assert_eq!(parsed[1].content, "never push to main");
    }

    #[test]
    fn deleting_a_block_rejects_it() {
        let entries = vec![
            (PROJECT_MEMORY, pending("semantic", "keep me")),
            (PROJECT_MEMORY, pending("semantic", "drop me")),
        ];
        let doc = render(&entries).replace("[semantic] drop me\n", "");
        let parsed = parse(&doc).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, "keep me");
    }

    #[test]
    fn an_empty_document_rejects_everything() {
        assert!(parse(HEADER).unwrap().is_empty());
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn a_human_can_add_a_bare_line() {
        let doc = "## project · memory\n\nsomething the model missed\n";
        let parsed = parse(doc).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, None);
        assert_eq!(parsed[0].content, "something the model missed");
    }

    #[test]
    fn indented_lines_continue_a_block() {
        let doc = "## project · memory\n\n[semantic] first line\n  second line\n";
        let parsed = parse(doc).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, "first line second line");
    }

    #[test]
    fn blank_line_separates_blocks() {
        let doc = "## project · memory\n\n[semantic] one\n\n[semantic] two\n";
        assert_eq!(parse(doc).unwrap().len(), 2);
    }

    #[test]
    fn ascii_separator_is_accepted_in_a_heading() {
        let doc = "## global - memory\n\n[semantic] typed by hand\n";
        assert!(parse(doc).unwrap()[0].section.global);
    }

    /// Silently dropping these would lose exactly the entry the human cared
    /// enough to write by hand.
    #[test]
    fn an_entry_before_any_heading_is_an_error() {
        let err = parse("[semantic] homeless\n").unwrap_err();
        assert!(err.contains("before any"), "{err}");
    }

    #[test]
    fn an_unknown_heading_is_an_error() {
        let err = parse("## project · nonsense\n\n[semantic] x\n").unwrap_err();
        assert!(err.contains("unrecognized heading"), "{err}");
    }

    #[test]
    fn comment_lines_are_ignored() {
        let doc = "# a comment\n## project · memory\n# another\n\n[semantic] kept\n";
        let parsed = parse(doc).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, "kept");
    }

    #[test]
    fn report_summary_mentions_failures_only_when_present() {
        let clean = ApplyReport {
            stored: 2,
            rejected: 1,
            failures: vec![],
        };
        assert_eq!(clean.summary(), "2 stored, 1 rejected");
        let dirty = ApplyReport {
            stored: 1,
            rejected: 0,
            failures: vec!["x: duplicate".into()],
        };
        assert!(dirty.summary().contains("1 failed"));
    }
}
