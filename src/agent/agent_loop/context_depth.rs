//! Phase 4 part 2 — context-depth reminder tracker.
//!
//! Long agentic sessions drift: the model can spend dozens of turns
//! re-editing the same file without ever stepping back to the
//! user's original task. `FileTouchTracker` watches tool calls for
//! file-path arguments and, when the agent has touched the SAME
//! file(s) for `threshold` consecutive turns, emits a one-shot
//! reminder restating the active task + the files being touched.
//!
//! The tracker is self-contained — no rig types, no LLM state. It
//! lives behind `LoopConfig.file_touch_tracker`; when `None`, the
//! loop behaves byte-identically to today.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::message::{LoopMessage, UserMessage};
use super::steering::MID_TURN_STEER_WRAPPER;

/// Per-session tracker. Wraps a `Mutex<Inner>` so the steering hook
/// and the tool-dispatch hook can both poll it from async contexts
/// without needing `&mut LoopConfig` plumbing.
#[derive(Debug)]
pub struct FileTouchTracker {
    inner: Mutex<Inner>,
    threshold: usize,
}

#[derive(Debug)]
struct Inner {
    /// Files touched in the most recent tool-call turn. Empty
    /// before any tool has been recorded.
    last_files: HashSet<PathBuf>,
    /// Count of consecutive turns that all touched a non-empty
    /// subset of `last_files`.
    consecutive: usize,
    /// The most recent user-prompted task. Used in the reminder
    /// body. Updated by `record_user_message`.
    active_task: String,
    /// Set to true once we've emitted a reminder for the current
    /// streak; cleared on reset. Prevents per-turn spam — one
    /// reminder per streak only.
    emitted_for_streak: bool,
}

impl FileTouchTracker {
    /// Build a tracker. `threshold` is the consecutive-turn count
    /// that triggers the reminder; `active_task` seeds the
    /// reminder body.
    pub fn new(threshold: usize, active_task: String) -> Arc<Self> {
        Arc::new(Self {
            threshold,
            inner: Mutex::new(Inner {
                last_files: HashSet::new(),
                consecutive: 0,
                active_task,
                emitted_for_streak: false,
            }),
        })
    }

    /// Record a tool call. Extracts file paths from the tool args
    /// (best-effort — looks at `path`, `paths`, `file_path`,
    /// `file` fields at the top level of the args object).
    ///
    /// If the call touches a file already in `last_files`, the
    /// streak continues. If it touches a different file (or no
    /// file), the streak resets to the newly-touched set
    /// (consecutive=1) or to empty (consecutive=0 when no file
    /// was touched).
    pub fn record_tool_call(&self, _tool_name: &str, args: &serde_json::Value) {
        let touched = extract_paths(args);
        let mut inner = self.inner.lock_ignore_poison();

        if touched.is_empty() {
            // No file touched this turn — break the streak.
            // Defensive: a turn full of `bash` / `grep` shouldn't
            // count toward a file-focused streak.
            inner.consecutive = 0;
            inner.last_files.clear();
            inner.emitted_for_streak = false;
            return;
        }

        // Continue the streak when the new touch set overlaps with
        // the previous one — at least one file in common counts.
        let overlap =
            !inner.last_files.is_empty() && touched.iter().any(|p| inner.last_files.contains(p));

        if overlap {
            // Narrow `last_files` to the overlap so divergent
            // touches eventually break the streak. This matches the
            // intuition that the model is FOCUSED if it keeps
            // returning to the same file(s).
            let intersection: HashSet<PathBuf> = touched
                .iter()
                .filter(|p| inner.last_files.contains(*p))
                .cloned()
                .collect();
            inner.last_files = intersection;
            inner.consecutive += 1;
        } else {
            inner.last_files = touched;
            inner.consecutive = 1;
            inner.emitted_for_streak = false;
        }
    }

    /// Record a user message. If the prompt doesn't mention any
    /// current file, treat it as a topic change: reset the streak
    /// and update `active_task` to the new prompt. If it DOES
    /// mention current files, keep the streak but still update
    /// `active_task`.
    pub fn record_user_message(&self, content: &str) {
        let mut inner = self.inner.lock_ignore_poison();

        let mentions_current = !inner.last_files.is_empty()
            && inner.last_files.iter().any(|p| mentions_path(content, p));

        if !mentions_current {
            inner.consecutive = 0;
            inner.last_files.clear();
            inner.emitted_for_streak = false;
        }
        inner.active_task = content.to_string();
    }

    /// Steering hook: returns a one-shot reminder message when the
    /// streak just crossed the threshold AND we haven't emitted
    /// yet for this streak; otherwise returns an empty vec.
    ///
    /// The returned message is wrapped with `MID_TURN_STEER_WRAPPER`
    /// so the model doesn't treat it as a new task.
    pub fn poll_reminder(&self) -> Vec<LoopMessage> {
        let mut inner = self.inner.lock_ignore_poison();
        if inner.consecutive < self.threshold || inner.emitted_for_streak {
            return Vec::new();
        }
        inner.emitted_for_streak = true;
        let body = format_reminder(inner.consecutive, &inner.last_files, &inner.active_task);
        let wrapped = format!("{}\n{}", MID_TURN_STEER_WRAPPER, body);
        vec![LoopMessage::User(UserMessage { content: wrapped })]
    }

    /// The current working-set files (the `last_files` overlap), sorted
    /// for deterministic ordering. Consulted after compaction to re-read
    /// and re-inject the files the agent was actively editing, so a fold
    /// doesn't strand it without the concrete file state
    /// (IMPROVEMENTS_PLAN #2).
    pub fn working_files(&self) -> Vec<PathBuf> {
        let inner = self.inner.lock_ignore_poison();
        let mut files: Vec<PathBuf> = inner.last_files.iter().cloned().collect();
        files.sort();
        files
    }
}

/// Walk `args` looking for top-level `path` / `paths` / `file_path`
/// / `file` fields. `paths` may be an array; the others are scalar
/// strings.
fn extract_paths(args: &serde_json::Value) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let obj = match args.as_object() {
        Some(o) => o,
        None => return out,
    };
    for key in &["path", "file_path", "file"] {
        if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
            out.insert(PathBuf::from(s));
        }
    }
    if let Some(arr) = obj.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                out.insert(PathBuf::from(s));
            }
        }
    }
    out
}

/// Does `content` mention `path`? Match by either the full string
/// representation or the file-name component. Substring match — a
/// prompt like "look at foo.rs" should hit `/repo/src/foo.rs`.
fn mentions_path(content: &str, path: &std::path::Path) -> bool {
    let full = path.to_string_lossy();
    if !full.is_empty() && content.contains(full.as_ref()) {
        return true;
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && !name.is_empty()
        && content.contains(name)
    {
        return true;
    }
    false
}

/// Format the reminder body. Kept as a free fn so tests can pin
/// the exact wording.
fn format_reminder(consecutive: usize, files: &HashSet<PathBuf>, active_task: &str) -> String {
    let mut sorted: Vec<&PathBuf> = files.iter().collect();
    sorted.sort();
    let mut s = format!(
        "[Context-depth reminder] You've spent {} consecutive turns on the same files:\n",
        consecutive,
    );
    for f in sorted {
        s.push_str(&format!("  - {}\n", f.display()));
    }
    s.push_str(&format!("Active task: {}\n", active_task));
    s.push_str(
        "If you've drifted, refocus on the active task. If the user changed direction,\n\
         acknowledge that explicitly before continuing.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn touches_same_files_increments_consecutive() {
        let t = FileTouchTracker::new(8, "edit foo.rs".to_string());
        t.record_tool_call("write", &json!({"path": "foo.rs", "content": "x"}));
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("edit", &json!({"file_path": "foo.rs"}));
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 3);
    }

    // IMPROVEMENTS_PLAN #2: working_files() exposes the tracked overlap
    // set (sorted) for post-compaction re-injection.
    #[test]
    fn working_files_returns_sorted_overlap() {
        let t = FileTouchTracker::new(8, "edit".to_string());
        // Touch two files together, then re-touch them → overlap kept.
        t.record_tool_call("edit", &json!({"paths": ["b.rs", "a.rs"]}));
        t.record_tool_call("edit", &json!({"paths": ["a.rs", "b.rs"]}));
        let files = t.working_files();
        assert_eq!(
            files,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            "working_files must be the tracked set, sorted"
        );
    }

    #[test]
    fn touch_unrelated_file_resets_streak() {
        let t = FileTouchTracker::new(8, "edit foo.rs".to_string());
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("read", &json!({"path": "bar.rs"}));
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 1, "bar.rs starts a fresh streak");
        assert!(inner.last_files.contains(&PathBuf::from("bar.rs")));
    }

    #[test]
    fn threshold_crossing_emits_one_reminder() {
        let t = FileTouchTracker::new(3, "edit foo.rs".to_string());
        for _ in 0..4 {
            t.record_tool_call("read", &json!({"path": "foo.rs"}));
        }
        let first = t.poll_reminder();
        assert_eq!(first.len(), 1, "first poll past threshold emits one");
        let second = t.poll_reminder();
        assert!(second.is_empty(), "second poll on same streak is silent");
    }

    #[test]
    fn new_user_message_referencing_active_files_keeps_streak() {
        let t = FileTouchTracker::new(8, "edit foo.rs".to_string());
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_user_message("can you also update foo.rs to add logging");
        t.record_tool_call("edit", &json!({"path": "foo.rs"}));
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 3);
        assert_eq!(
            inner.active_task,
            "can you also update foo.rs to add logging"
        );
    }

    #[test]
    fn new_user_message_changing_topic_resets_streak() {
        let t = FileTouchTracker::new(3, "edit foo.rs".to_string());
        for _ in 0..4 {
            t.record_tool_call("read", &json!({"path": "foo.rs"}));
        }
        t.record_user_message("look at the database schema instead");
        let pending = t.poll_reminder();
        assert!(pending.is_empty(), "streak reset → no reminder");
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 0);
        assert_eq!(inner.active_task, "look at the database schema instead");
    }

    #[test]
    fn reminder_includes_active_task_and_files() {
        let t = FileTouchTracker::new(2, "refactor parser".to_string());
        t.record_tool_call("read", &json!({"path": "parser.rs"}));
        t.record_tool_call("edit", &json!({"path": "parser.rs"}));
        let msgs = t.poll_reminder();
        assert_eq!(msgs.len(), 1);
        let content = match &msgs[0] {
            LoopMessage::User(u) => u.content.clone(),
            _ => panic!("expected User"),
        };
        assert!(content.starts_with(MID_TURN_STEER_WRAPPER));
        assert!(content.contains("Context-depth reminder"));
        assert!(content.contains("parser.rs"));
        assert!(content.contains("refactor parser"));
        assert!(content.contains("2 consecutive turns"));
    }

    #[test]
    fn paths_array_field_recognised() {
        let t = FileTouchTracker::new(8, "x".to_string());
        t.record_tool_call("multi_read", &json!({"paths": ["a.rs", "b.rs"]}));
        t.record_tool_call("read", &json!({"path": "a.rs"}));
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 2);
    }

    #[test]
    fn turn_with_no_file_breaks_streak() {
        let t = FileTouchTracker::new(8, "x".to_string());
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("read", &json!({"path": "foo.rs"}));
        t.record_tool_call("bash", &json!({"command": "ls"}));
        let inner = t.inner.lock().unwrap();
        assert_eq!(inner.consecutive, 0);
        assert!(inner.last_files.is_empty());
    }
}
