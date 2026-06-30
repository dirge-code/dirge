//! Deterministic session ground-truth digest (dirge-a62g).
//!
//! Memory formation (`agent::review`) forks an LLM over the transcript and
//! asks it to NOTICE durable facts — "what build/test commands ran", "what
//! files were touched". That is lossy (the model omits non-salient facts),
//! rate-limited (the 15-min `claim_review_slot` throttle can skip a session
//! entirely), and spends tokens rediscovering facts the session already
//! records verbatim.
//!
//! This module pulls that ground-truth straight from the session and git with
//! NO model call: the opening goal, files touched, commands run, the live todo
//! list, where we stopped, and `git diff --stat`. It is a deterministic floor
//! UNDER the LLM review, not a replacement.
//!
//! Two consumers:
//! - the background review prepends the digest (via [`assemble_review_transcript`])
//!   to the transcript so the model ranks/classifies KNOWN facts instead of
//!   hunting for them;
//! - the throttled/errored-review fallback (dirge-a62g 1b) persists the digest
//!   so a session's ground-truth is never fully lost (sibling: dirge-hcv8's
//!   open-threads carry-over builds on the same extraction).
//!
//! Files/todos come from [`crate::session::rehydrate::selected_panel_state`],
//! which prefers the persisted snapshot and so survives a destructive
//! compaction. Goal / last-state / commands are read from `messages` and are
//! best-effort: a compaction that drained the originating turns simply yields
//! less here, never wrong data.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::tools::todo::TodoItem;
use crate::session::rehydrate::selected_panel_state;
use crate::session::{MessageRole, Session, ToolCallState};

/// Cap on files listed in the digest — enough to characterize a session
/// without flooding the review prompt.
const MAX_FILES: usize = 30;
/// Cap on distinct commands listed.
const MAX_COMMANDS: usize = 20;
/// Truncation for the opening goal line.
const GOAL_MAX_CHARS: usize = 500;
/// Truncation for the where-we-stopped line.
const STATE_MAX_CHARS: usize = 600;
/// Truncation for the `git diff --stat` block.
const GIT_STAT_MAX_CHARS: usize = 2000;

/// Deterministic, reproducible facts about a session — no model involved.
#[derive(Debug, Default, Clone)]
pub struct SessionDigest {
    /// First substantive user message — what the session set out to do.
    pub goal: String,
    /// Last substantive assistant message — where we stopped.
    pub last_state: String,
    /// Files written/edited/patched, recency-ordered (freshest last).
    pub files: Vec<PathBuf>,
    /// Distinct bash commands run, in first-seen order.
    pub commands: Vec<String>,
    /// The live todo list at session end.
    pub todos: Vec<TodoItem>,
    /// `git diff --stat` output, attached by the caller (kept out of
    /// [`from_session`] so the extractor stays pure / shell-free / testable).
    pub git_diff_stat: Option<String>,
}

impl SessionDigest {
    /// Build the model-free digest from a session. Pure: reads only in-memory
    /// session state, never shells out. Attach git via [`with_git_diff_stat`].
    pub fn from_session(session: &Session) -> Self {
        let panel = selected_panel_state(session);

        let mut files = panel.modified;
        if files.len() > MAX_FILES {
            // Keep the freshest (panel order is freshest-last).
            files = files.split_off(files.len() - MAX_FILES);
        }

        let goal = session
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User && !m.content.trim().is_empty())
            .map(|m| one_line(&m.content))
            .map(|s| truncate(&s, GOAL_MAX_CHARS))
            .unwrap_or_default();

        let last_state = session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant && !m.content.trim().is_empty())
            .map(|m| one_line(&m.content))
            .map(|s| truncate(&s, STATE_MAX_CHARS))
            .unwrap_or_default();

        let commands = collect_commands(session);

        Self {
            goal,
            last_state,
            files,
            commands,
            todos: panel.todos,
            git_diff_stat: None,
        }
    }

    /// Attach a `git diff --stat` block (trimmed + capped). Chainable.
    pub fn with_git_diff_stat(mut self, stat: Option<String>) -> Self {
        self.git_diff_stat = stat.and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(truncate(t, GIT_STAT_MAX_CHARS))
            }
        });
        self
    }

    /// True when nothing was captured — caller should inject no preamble.
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty()
            && self.last_state.is_empty()
            && self.files.is_empty()
            && self.commands.is_empty()
            && self.todos.is_empty()
            && self.git_diff_stat.is_none()
    }

    /// Render the digest as a markdown preamble for the review prompt. Returns
    /// an empty string when [`is_empty`] — the caller skips injection then.
    /// Only non-empty sections are emitted.
    pub fn render_for_review(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str(
            "## Session ground-truth (deterministic — extracted without a model)\n\
             These facts were pulled directly from the session and git. Treat them as the \
             authoritative record of WHAT happened this session; your job is to decide what is \
             worth remembering, not to rediscover them.\n",
        );

        if !self.goal.is_empty() {
            out.push_str(&format!("\n**Goal:** {}\n", self.goal));
        }
        if !self.files.is_empty() {
            out.push_str(&format!("\n**Files touched ({}):**\n", self.files.len()));
            for f in &self.files {
                out.push_str(&format!("- {}\n", f.display()));
            }
        }
        if !self.commands.is_empty() {
            out.push_str(&format!("\n**Commands run ({}):**\n", self.commands.len()));
            for c in &self.commands {
                out.push_str(&format!("- `{c}`\n"));
            }
        }
        if !self.todos.is_empty() {
            out.push_str("\n**Todos at session end:**\n");
            for t in &self.todos {
                out.push_str(&format!("- [{}] {}\n", t.status, t.content));
            }
        }
        if !self.last_state.is_empty() {
            out.push_str(&format!("\n**Where we stopped:** {}\n", self.last_state));
        }
        if let Some(stat) = &self.git_diff_stat {
            out.push_str(&format!("\n**git diff --stat:**\n```\n{stat}\n```\n"));
        }
        out
    }
}

/// The transcript handed to the background review: the deterministic digest
/// preamble (when any) followed by the conversation `base`. Single owner of the
/// preamble-vs-conversation layout so both post-session entry points agree.
///
/// Takes an already-built [`SessionDigest`] and attaches git (this shells out
/// `git diff --stat`) before rendering. The caller builds the session-derived
/// digest on the UI thread (cheap, shell-free) and defers this — the git
/// subprocess — to the post-session task (dirge-6rtt), so the event loop never
/// shells out on a turn end.
pub fn assemble_review_transcript(digest: SessionDigest, repo_root: &Path, base: String) -> String {
    let preamble = digest
        .with_git_diff_stat(git_diff_stat(repo_root))
        .render_for_review();
    if preamble.is_empty() {
        base
    } else {
        format!("{preamble}\n\n{base}")
    }
}

/// Distinct bash commands from completed `bash` tool calls, first-seen order,
/// capped at [`MAX_COMMANDS`].
fn collect_commands(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for msg in &session.messages {
        for tc in &msg.tool_calls {
            if tc.name != "bash" {
                continue;
            }
            if !matches!(tc.state, ToolCallState::Completed { .. }) {
                continue;
            }
            if let Some(cmd) = tc.args.get("command").and_then(|v| v.as_str()) {
                let cmd = one_line(cmd);
                if !cmd.is_empty() && !out.iter().any(|c| c == &cmd) {
                    out.push(cmd);
                    if out.len() >= MAX_COMMANDS {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Run `git -C <root> diff --stat HEAD` and return its trimmed stdout, or
/// `None` if git is absent, the repo has no HEAD, or there is no diff. Bare
/// `Command` (no shell) matching the existing `git_worktree` helpers.
pub fn git_diff_stat(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["--no-optional-locks", "diff", "--stat", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stat.is_empty() { None } else { Some(stat) }
}

/// Collapse all whitespace runs (incl. newlines) into single spaces and trim.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` chars (char-boundary safe), appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionMessage, ToolCallEntry, ToolCallState};
    use compact_str::CompactString;

    fn msg(role: MessageRole, content: &str, calls: Vec<ToolCallEntry>) -> SessionMessage {
        SessionMessage {
            role,
            content: CompactString::from(content),
            estimated_tokens: 0,
            id: crate::session::new_message_id(),
            timestamp: 0,
            tool_calls: calls,
        }
    }

    fn completed(name: &str, args: serde_json::Value) -> ToolCallEntry {
        ToolCallEntry {
            id: "tc".to_string(),
            name: name.to_string(),
            args,
            state: ToolCallState::Completed {
                result: String::new(),
            },
        }
    }

    fn session_with(messages: Vec<SessionMessage>) -> Session {
        let mut s = Session::new("test", "test-model", 1000);
        s.messages = messages;
        s
    }

    #[test]
    fn extracts_goal_last_state_files_commands() {
        let s = session_with(vec![
            msg(MessageRole::User, "Fix the failing build", vec![]),
            msg(
                MessageRole::Assistant,
                "Looking into it",
                vec![
                    completed("bash", serde_json::json!({"command": "cargo build"})),
                    completed("write", serde_json::json!({"path": "/proj/a.rs"})),
                ],
            ),
            msg(
                MessageRole::Assistant,
                "Build is green now",
                vec![completed(
                    "bash",
                    serde_json::json!({"command": "cargo test"}),
                )],
            ),
        ]);
        let d = SessionDigest::from_session(&s);
        assert_eq!(d.goal, "Fix the failing build");
        assert_eq!(d.last_state, "Build is green now");
        assert_eq!(d.commands, vec!["cargo build", "cargo test"]);
        assert_eq!(d.files.len(), 1);
        assert!(d.files[0].ends_with("a.rs"));
        assert!(!d.is_empty());
    }

    #[test]
    fn commands_are_deduped_in_first_seen_order() {
        let s = session_with(vec![msg(
            MessageRole::Assistant,
            "",
            vec![
                completed("bash", serde_json::json!({"command": "cargo build"})),
                completed("bash", serde_json::json!({"command": "cargo test"})),
                completed("bash", serde_json::json!({"command": "cargo build"})),
            ],
        )]);
        let d = SessionDigest::from_session(&s);
        assert_eq!(d.commands, vec!["cargo build", "cargo test"]);
    }

    #[test]
    fn ignores_non_completed_bash_calls() {
        let interrupted = ToolCallEntry {
            id: "x".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "rm -rf /"}),
            state: ToolCallState::Interrupted,
        };
        let s = session_with(vec![msg(MessageRole::Assistant, "", vec![interrupted])]);
        let d = SessionDigest::from_session(&s);
        assert!(d.commands.is_empty());
    }

    #[test]
    fn goal_skips_empty_user_messages_and_collapses_whitespace() {
        let s = session_with(vec![
            msg(MessageRole::User, "   ", vec![]),
            msg(MessageRole::User, "do\n\n  the   thing", vec![]),
        ]);
        let d = SessionDigest::from_session(&s);
        assert_eq!(d.goal, "do the thing");
    }

    #[test]
    fn empty_session_is_empty_and_renders_nothing() {
        let d = SessionDigest::from_session(&session_with(vec![]));
        assert!(d.is_empty());
        assert_eq!(d.render_for_review(), "");
    }

    #[test]
    fn with_git_diff_stat_drops_blank_and_caps() {
        let d = SessionDigest::default().with_git_diff_stat(Some("   \n  ".into()));
        assert!(d.git_diff_stat.is_none());

        let long = "x".repeat(GIT_STAT_MAX_CHARS + 50);
        let d = SessionDigest::default().with_git_diff_stat(Some(long));
        let stat = d.git_diff_stat.unwrap();
        assert!(stat.ends_with('…'));
        assert_eq!(stat.chars().count(), GIT_STAT_MAX_CHARS + 1);
    }

    #[test]
    fn render_includes_sections_and_is_skippable_when_empty() {
        let mut d = SessionDigest {
            goal: "Add a feature".into(),
            last_state: "Done".into(),
            files: vec![PathBuf::from("/proj/a.rs")],
            commands: vec!["cargo build".into()],
            todos: vec![],
            git_diff_stat: Some("1 file changed".into()),
        };
        let r = d.render_for_review();
        assert!(r.contains("Session ground-truth"));
        assert!(r.contains("**Goal:** Add a feature"));
        assert!(r.contains("**Files touched (1):**"));
        assert!(r.contains("- `cargo build`"));
        assert!(r.contains("**Where we stopped:** Done"));
        assert!(r.contains("git diff --stat"));
        // No todos section when empty.
        assert!(!r.contains("Todos at session end"));

        d = SessionDigest::default();
        assert_eq!(d.render_for_review(), "");
    }

    #[test]
    fn files_capped_to_freshest() {
        let mut calls = Vec::new();
        for i in 0..(MAX_FILES + 5) {
            calls.push(completed(
                "write",
                serde_json::json!({ "path": format!("/proj/f{i}.rs") }),
            ));
        }
        let s = session_with(vec![msg(MessageRole::Assistant, "", calls)]);
        let d = SessionDigest::from_session(&s);
        assert_eq!(d.files.len(), MAX_FILES);
        // Freshest kept: the very last write must be present.
        let last = format!("f{}.rs", MAX_FILES + 4);
        assert!(d.files.last().unwrap().ends_with(&last));
    }
}
