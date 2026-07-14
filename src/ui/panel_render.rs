//! Build the info-panel snapshot (cwd, MCP, LSP, todos, modified
//! files) and a small cache of the modified-files list keyed by the
//! tracker's monotonic version.
//!
//! Extracted from `ui/mod.rs`. Reading global statics (TODO_LIST,
//! MODIFIED_FILES) under their own mutexes is fine from the UI loop
//! tick — they're all short-lived locks.

#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::session::Session;
use crate::sync_util::LockExt;
use crate::ui::panel_data::{ContextGauge, GitSnapshot, LeftPanelInfo};
use crate::ui::renderer::PanelData;
use crate::ui::sysload::SharedSysLoad;

/// Usage fraction at/above which the context gauge flags an imminent
/// fold. Tracks the post-usage NORMAL-fold trigger in
/// `agent_loop::context_manager` (75%); kept as a local constant so the
/// UI doesn't depend on the agent-loop internals.
const FOLD_WARN_PCT: u16 = 75;

/// Most TODOS rows to show. The board is already ordered in_progress-first,
/// so the cap keeps the active work visible while stopping a long backlog
/// from crowding out the MODIFIED panel below (use `/issues` for the full
/// board).
const MAX_TODO_ROWS: usize = 8;

/// Map the live board mirror to TODOS panel rows: a status glyph, the title,
/// and an `active` flag (the issue is in_progress) the painter renders in the
/// focus color. The mirror is already in_progress-first, so `take` keeps the
/// active item(s) pinned at the top. Pure so the glyph/active mapping is
/// unit-testable without a Session or the global mirror.
pub(crate) fn todo_rows(
    list: &[crate::agent::tools::todo::TodoItem],
) -> Vec<(String, String, bool)> {
    list.iter()
        .take(MAX_TODO_ROWS)
        .map(|t| {
            let active = t.status == "in_progress";
            let glyph = match t.status.as_str() {
                "in_progress" => "▶",
                "blocked" => "[!]",
                _ => "[ ]",
            };
            (glyph.to_string(), t.content.clone(), active)
        })
        .collect()
}

#[cfg(test)]
mod todo_row_tests {
    use super::todo_rows;
    use crate::agent::tools::todo::TodoItem;

    fn item(content: &str, status: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: status.into(),
            priority: "normal".into(),
        }
    }

    #[test]
    fn marks_in_progress_active_with_distinct_glyph() {
        let rows = todo_rows(&[
            item("wire retry backoff", "in_progress"),
            item("add jitter", "open"),
            item("audit timeouts", "blocked"),
        ]);
        assert_eq!(
            rows[0],
            ("▶".to_string(), "wire retry backoff".to_string(), true)
        );
        assert!(!rows[1].2);
        assert_eq!(rows[1].0, "[ ]");
        assert_eq!(rows[2].0, "[!]");
        assert!(!rows[2].2, "blocked is not the active/focus row");
    }

    #[test]
    fn caps_row_count() {
        let many: Vec<TodoItem> = (0..20).map(|i| item(&format!("t{i}"), "open")).collect();
        assert_eq!(todo_rows(&many).len(), super::MAX_TODO_ROWS);
    }
}

/// Build the left-panel idle card: a live context gauge, the recent-tool
/// activity ticker, and the git snapshot. Rebuilt each event-loop tick
/// (cheap — a few field reads + the pre-polled git snapshot). `activity`
/// is the UI loop's recent-tool ring (oldest-first) and `git` is the
/// latest poll from `gitstatus`.
pub(crate) fn build_left_panel_info(
    session: &Session,
    activity: &[String],
    git: Option<GitSnapshot>,
) -> LeftPanelInfo {
    let used = session.total_estimated_tokens;
    let window = session.context_window;
    let pct = ((used.saturating_mul(100)).checked_div(window).unwrap_or(0)).min(100) as u16;
    LeftPanelInfo {
        context: ContextGauge {
            used,
            window,
            pct,
            compactions: session.compactions.len(),
            fold_soon: pct >= FOLD_WARN_PCT,
        },
        activity: activity.to_vec(),
        git,
    }
}

/// Cache of the panel's rendered MODIFIED list, keyed by
/// `(modified::version, cwd)`. Skips the lock + 256-PathBuf clone +
/// path-strip on every redraw when nothing has changed. Single-
/// threaded read (the UI loop) so a Mutex around the tuple is the
/// simplest correct shape; contention is nil.
static PANEL_MODIFIED_CACHE: std::sync::Mutex<Option<(u64, std::path::PathBuf, Vec<String>)>> =
    std::sync::Mutex::new(None);

pub(crate) fn panel_modified_cached(cwd: &std::path::Path) -> Vec<String> {
    let v = crate::agent::tools::modified::version();
    {
        let guard = PANEL_MODIFIED_CACHE.lock_ignore_poison();
        if let Some((cached_v, cached_cwd, cached_data)) = guard.as_ref()
            && *cached_v == v
            && cached_cwd.as_path() == cwd
        {
            return cached_data.clone();
        }
    }
    // Cache miss — rebuild. Lock the modified tracker, project to
    // display strings, store back.
    let cwd_buf = cwd.to_path_buf();
    let rendered: Vec<String> = crate::agent::tools::modified::recent(256)
        .into_iter()
        .map(|p| {
            p.strip_prefix(&cwd_buf)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| p.display().to_string())
                })
        })
        .collect();
    let mut guard = PANEL_MODIFIED_CACHE.lock_ignore_poison();
    *guard = Some((v, cwd_buf, rendered.clone()));
    rendered
}

/// Snapshot the various pieces of state the info panel surfaces (cwd, MCP,
/// LSP, todos, modified files) into a `PanelData` ready to hand to the
/// renderer. Reads global statics (TODO_LIST, MODIFIED_FILES) under their
/// own mutexes; safe to call from the UI loop tick.
pub(crate) fn build_panel_data(
    session: &Session,
    sysload: Option<&SharedSysLoad>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> PanelData {
    use std::path::Path;

    #[cfg(feature = "mcp")]
    let mcp: Vec<(String, bool)> = mcp_manager
        .map(|m| {
            // Live connections render as healthy (`●`). GH #541: a server
            // whose initial connect failed is appended afterwards as
            // broken (`○`) so it's visible instead of silently omitted.
            let mut rows: Vec<(String, bool)> = m
                .connections_snapshot()
                .into_iter()
                .map(|(name, _conn)| (name, true))
                .collect();
            for name in m.failed_servers() {
                rows.push((name, false));
            }
            rows
        })
        .unwrap_or_default();
    #[cfg(not(feature = "mcp"))]
    let mcp: Vec<(String, bool)> = Vec::new();

    #[cfg(feature = "lsp")]
    let lsp: Vec<(String, String, bool)> = lsp_manager
        .map(|m| {
            let cwd_path = Path::new(session.working_dir.as_str());
            let shorten = |p: &Path| -> String {
                p.strip_prefix(cwd_path)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(String::from)
                            .unwrap_or_else(|| p.display().to_string())
                    })
            };
            let mut all = Vec::new();
            for (id, root) in m.active_servers() {
                all.push((id, shorten(&root), true));
            }
            for (id, root) in m.broken_servers() {
                all.push((id, shorten(&root), false));
            }
            all
        })
        .unwrap_or_default();
    #[cfg(not(feature = "lsp"))]
    let lsp: Vec<(String, String, bool)> = Vec::new();

    // The TODOS panel mirrors this session's live issue board (open /
    // in_progress / blocked). Terminal items (done / cancelled) have already
    // dropped off the board, so no completed glyph is needed here.
    let todos = todo_rows(&crate::agent::tools::todo::TODO_LIST.lock_ignore_poison());

    let cwd_path = Path::new(session.working_dir.as_str()).to_path_buf();
    // Pull the full tracked set (capped at MAX_MODIFIED=256 inside the
    // tracker). The renderer's `build_panel_lines` decides how many
    // actually fit in the panel based on remaining terminal rows and
    // appends a `+N older` footer when truncated — matches opencode's
    // grow-to-fit pattern.
    //
    // Review #6: cache the rendered Vec<String> against the
    // tracker's monotonic version counter. The panel redraws on
    // every keystroke / streamed token; without the cache we'd
    // lock + clone 256 PathBufs + path-strip per redraw. The cache
    // also includes the cwd so a `/cd` invalidates it correctly.
    let modified = panel_modified_cached(&cwd_path);

    PanelData {
        mcp,
        lsp,
        todos,
        modified,
        sysload: sysload.map(|s| s.snapshot()),
    }
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;
    use crate::extras::mcp::McpClientManager;
    use crate::extras::mcp::config::McpServerConfig;
    use std::collections::HashMap;

    fn bogus_server() -> McpServerConfig {
        // A binary that can't exist → spawn fails immediately, so `connect`
        // returns Err well inside the init timeout (no 10s wait).
        McpServerConfig::Command {
            command: "dirge-nonexistent-mcp-binary".to_string(),
            args: vec![],
            env: HashMap::new(),
            allow_external_paths: false,
        }
    }

    /// GH #541: a server that fails its initial connect must still show
    /// up in the info panel as broken (`○`), not vanish entirely. Before
    /// the fix `build_panel_data` enumerated only live connections, so a
    /// misconfigured server rendered as `· (none)`.
    #[tokio::test]
    async fn build_panel_data_surfaces_failed_mcp_servers_as_broken() {
        let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
        configs.insert("ghost".to_string(), bogus_server());
        let mgr = McpClientManager::connect_all(&configs).await;

        let session = crate::session::Session::new("p", "m", 100_000);
        let data = build_panel_data(
            &session,
            None,
            Some(&mgr),
            #[cfg(feature = "lsp")]
            None,
        );

        assert_eq!(data.mcp.len(), 1, "failed server must still be listed");
        assert_eq!(data.mcp[0].0, "ghost");
        assert!(
            !data.mcp[0].1,
            "failed server must render as broken (ok=false)"
        );
    }
}
