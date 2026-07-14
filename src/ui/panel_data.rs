//! Info panel data types.
//!
//! `PanelData`, `SubagentStatusRow`, and `LeftPanelInfo` — the three
//! structs that carry the right-hand side panel's content. Extracted
//! from `renderer.rs` so the panel painter and the UI loop can share
//! them without pulling in the full Renderer.

/// Snapshot of the data the info panel displays. Built fresh by the UI loop
/// at each redraw because the underlying state (todos, modified files, etc.)
/// is mutated by the agent and we don't want stale reads.
#[derive(Default, Clone)]
pub struct PanelData {
    /// (server name, connected) — connected currently always true because the
    /// MCP manager drops failed connections at connect time; future health
    /// tracking can flip this to false.
    pub mcp: Vec<(String, bool)>,
    /// (server_id, short root path, ok) — ok=false for broken servers.
    pub lsp: Vec<(String, String, bool)>,
    /// (status glyph, todo text, active). `active` marks the in_progress
    /// issue the agent is currently on — the painter pins it at the top (the
    /// mirror is already in_progress-first) and renders it in a distinct
    /// "focus" color so the current work stands out from the queue.
    pub todos: Vec<(String, String, bool)>,
    /// Recent modified file paths, shortened relative to cwd when possible.
    pub modified: Vec<String>,
    /// ui-redesign: latest system load snapshot for the
    /// [SYSTEM LOAD] sub-panel. `None` when the polling task hasn't
    /// produced a reading yet (very early startup) — painter skips
    /// the section in that case.
    pub sysload: Option<crate::ui::sysload::SysLoadSnapshot>,
}

/// One row in the left-gutter subagent panel. The `[AGENTS]` box shows
/// `agent` (e.g. "architect"), falling back to `id_short` when unset.
#[derive(Debug, Clone, Default)]
pub struct SubagentStatusRow {
    pub id_short: String,
    /// Agent-profile name (e.g. "architect"); falls back to `id_short` when unset.
    pub agent: Option<String>,
}

/// Context-window fill gauge for the left panel's `[CONTEXT]` section.
#[derive(Debug, Clone, Default)]
pub struct ContextGauge {
    /// Estimated tokens used by the live conversation.
    pub used: u64,
    /// Total context window for the active model.
    pub window: u64,
    /// `used/window` as an integer percent (0–100, saturating).
    pub pct: u16,
    /// Number of compaction (fold) events so far this session.
    pub compactions: usize,
    /// True once usage crosses the auto-compaction warning threshold,
    /// so the panel can flag that a fold is imminent.
    pub fold_soon: bool,
}

/// Git working-tree snapshot for the left panel's `[GIT]` section.
/// `None` (on `LeftPanelInfo.git`) when the cwd isn't a git repo or the
/// poller hasn't produced a reading yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitSnapshot {
    pub branch: String,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    /// Subject line of the most recent commit (may be empty on a repo
    /// with no commits yet).
    pub last_commit: String,
}

/// ui-redesign: idle-state info for the left panel. When no
/// subagents are active, the left gutter paints this card: the DIRGE
/// banner + live session vitals (context gauge, recent tool activity,
/// git status). Identity (model/prompt/etc.) lives in the status line,
/// so it's intentionally not duplicated here. Rebuilt each event-loop
/// tick by the UI loop.
#[derive(Debug, Clone, Default)]
pub struct LeftPanelInfo {
    /// Context-window fill gauge.
    pub context: ContextGauge,
    /// Recent tool actions, oldest-first (newest last). Each is a short
    /// label like `read run.rs` / `bash cargo test`.
    pub activity: Vec<String>,
    /// Git working-tree snapshot, when the cwd is a repo.
    pub git: Option<GitSnapshot>,
}

/// Build a compact, glanceable label for a tool call shown in the
/// left-panel `[ACTIVITY]` ticker — `<verb> <concise target>`. The
/// target is the basename for path tools, the command head for `bash`,
/// the pattern for `grep`, etc. Kept pure (no UI deps) so it's unit-
/// testable. `args` is the tool's JSON argument object.
pub fn tool_call_label(name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    let basename = |p: &str| -> String { p.rsplit(['/', '\\']).next().unwrap_or(p).to_string() };
    // Collapse whitespace/newlines and clip to keep the row tight.
    let clip = |v: &str, n: usize| -> String {
        let one = v.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > n {
            format!(
                "{}…",
                one.chars().take(n.saturating_sub(1)).collect::<String>()
            )
        } else {
            one
        }
    };
    let target = match name {
        "read" | "write" | "edit" | "apply_patch" => s("path")
            .or_else(|| s("file_path"))
            .or_else(|| s("file"))
            .map(basename),
        "bash" => s("command").map(|c| clip(c, 28)),
        "grep" => s("pattern").map(|p| clip(p, 24)),
        "find_files" | "glob" => s("pattern").or_else(|| s("query")).map(|p| clip(p, 24)),
        "list_dir" => s("path").map(basename),
        "memory" | "skill" | "task" | "task_status" => s("name")
            .or_else(|| s("action"))
            .or_else(|| s("prompt"))
            .map(|v| clip(v, 24)),
        "bash_output" | "kill_shell" => s("id").map(|i| clip(i, 12)),
        _ => None,
    };
    match target {
        Some(t) if !t.is_empty() => format!("{name} {t}"),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::tool_call_label;
    use serde_json::json;

    #[test]
    fn label_uses_basename_for_path_tools() {
        assert_eq!(
            tool_call_label("read", &json!({"path": "/abs/src/agent/run.rs"})),
            "read run.rs"
        );
        assert_eq!(
            tool_call_label("edit", &json!({"file_path": "src/ui/mod.rs"})),
            "edit mod.rs"
        );
    }

    #[test]
    fn label_clips_bash_command_head() {
        let out = tool_call_label(
            "bash",
            &json!({"command": "cargo test --all-features --workspace"}),
        );
        assert!(out.starts_with("bash cargo test"), "got: {out}");
        assert!(out.chars().count() <= "bash ".len() + 28, "got: {out}");
    }

    #[test]
    fn label_collapses_whitespace() {
        let out = tool_call_label("bash", &json!({"command": "echo   a\n  b"}));
        assert_eq!(out, "bash echo a b");
    }

    #[test]
    fn label_falls_back_to_name_without_usable_args() {
        assert_eq!(
            tool_call_label("repo_overview", &json!({})),
            "repo_overview"
        );
        assert_eq!(tool_call_label("read", &json!({})), "read");
    }
}
