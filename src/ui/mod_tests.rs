//! Tests for the items declared in `src/ui/mod.rs`. Moved out of
//! the main file to keep `mod.rs` close to the line-budget target
//! set by the `arch/split-large-modules` branch.
//!
//! Included via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`
//! at the bottom of `mod.rs`, so `super::*` here resolves into the
//! `ui` module exactly as the original inline `mod tests` block did.

use super::*;
use crate::ui::search_rewind::update_search;

/// dirge-vpma.8: closing a chat shifts higher indices down; the subagent
/// id↔index maps must be reindexed to match, dropping the removed entry.
#[test]
fn reindex_subagent_maps_after_remove() {
    let mut fwd: std::collections::HashMap<String, usize> =
        [("a".to_string(), 1), ("b".to_string(), 2)]
            .into_iter()
            .collect();
    let mut rev: std::collections::HashMap<usize, String> =
        [(1, "a".to_string()), (2, "b".to_string())]
            .into_iter()
            .collect();
    reindex_subagent_maps(&mut fwd, &mut rev, 1);
    assert!(
        !fwd.contains_key("a"),
        "removed id dropped from forward map"
    );
    assert_eq!(fwd.get("b"), Some(&1), "index above removed shifts down");
    assert_eq!(rev.get(&1).map(String::as_str), Some("b"));
    assert!(!rev.contains_key(&2), "old index vacated");
}

// ============================================================
// apply_subagent_panel_event — left-panel cleanup
// ============================================================

use crate::agent::tools::task::SubagentChatEvent as E;

/// Spawn → row carries the subagent's agent name.
#[test]
fn subagent_panel_spawn_inserts_running_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "abc123".into(),
            prompt: "build the binary".into(),
            agent: Some("dev".into()),
        },
    );
    assert_eq!(rows.len(), 1);
    let agent = rows.get("abc123").unwrap();
    assert_eq!(agent, &Some("dev".to_string()));
}

/// Complete → row is REMOVED (the bug being fixed). Previously
/// the row's state changed to "completed" and the entry stayed
/// in the map forever, accumulating stale ✓ glyphs in the panel.
#[test]
fn subagent_panel_complete_removes_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "abc123".into(),
            prompt: "build the binary".into(),
            agent: None,
        },
    );
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "abc123".into(),
            result: "ok".into(),
        },
    );
    assert!(rows.is_empty(), "completed subagent must be removed");
}

/// Failed → row is REMOVED (same cleanup contract as Complete).
#[test]
fn subagent_panel_failed_removes_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "xyz789".into(),
            prompt: "run tests".into(),
            agent: None,
        },
    );
    apply_subagent_panel_event(
        &mut rows,
        &E::Failed {
            id: "xyz789".into(),
            error: "boom".into(),
        },
    );
    assert!(rows.is_empty(), "failed subagent must be removed");
}

/// Mixed: several spawns + one completion leaves the rest in
/// place and preserves insertion order (oldest at top).
#[test]
fn subagent_panel_mixed_lifecycle_preserves_order() {
    let mut rows = indexmap::IndexMap::new();
    for id in ["a", "b", "c"] {
        apply_subagent_panel_event(
            &mut rows,
            &E::Spawn {
                id: id.into(),
                prompt: format!("task {id}"),
                agent: None,
            },
        );
    }
    // Remove the middle one.
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "b".into(),
            result: "ok".into(),
        },
    );
    assert_eq!(rows.len(), 2);
    let remaining: Vec<&str> = rows.keys().map(String::as_str).collect();
    assert_eq!(
        remaining,
        vec!["a", "c"],
        "shift_remove must preserve insertion order of survivors"
    );
}

/// Complete/Failed for an unknown id is a no-op (defensive —
/// shouldn't happen since Complete always follows Spawn, but if
/// the event ordering ever drifts, don't panic).
#[test]
fn subagent_panel_complete_unknown_id_is_noop() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "never-spawned".into(),
            result: "ok".into(),
        },
    );
    assert!(rows.is_empty());
}

/// dirge-bfd: Ctrl-F search uses fuzzy matching (nucleo) — typos,
/// non-contiguous subsequences, and missing characters all match
/// where they wouldn't under the prior substring scheme.
#[test]
fn fuzzy_search_matches_non_contiguous_subsequence() {
    let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
    renderer
        .write_line("connect to database", Color::White)
        .unwrap();
    renderer
        .write_line("contributing guide", Color::White)
        .unwrap();
    renderer
        .write_line("totally unrelated", Color::White)
        .unwrap();

    let mut matches: Vec<usize> = Vec::new();
    let mut selected = 0;

    // Substring `ctd` matches nothing under the old `contains`
    // scheme. Fuzzy matches "connect to database" by its
    // c-o-n-n-e-C-T-o-D... subsequence.
    update_search(&renderer, "ctd", &mut matches, &mut selected);
    assert!(
        !matches.is_empty(),
        "fuzzy `ctd` should produce matches; matches={matches:?}",
    );

    // Empty / whitespace queries clear matches.
    update_search(&renderer, "", &mut matches, &mut selected);
    assert!(matches.is_empty());
    update_search(&renderer, "   ", &mut matches, &mut selected);
    assert!(matches.is_empty());

    // Lowercase query matches (smart case).
    update_search(&renderer, "database", &mut matches, &mut selected);
    assert!(matches.iter().any(|&i| {
        renderer
            .buffer_lines()
            .get(i)
            .map(|s| s.contains("database"))
            .unwrap_or(false)
    }));
}

/// dirge-bfd: Ctrl-F search uses fuzzy matching (nucleo) — typos,
/// structured entries on the stashed message. Pending entries
/// stay Interrupted (no matching result arrived); on resume,
/// `convert_history` will emit a [Tool execution was
/// interrupted] tool_result so the LLM sees paired blocks.
#[test]
fn capture_partial_on_abort_preserves_pending_tool_calls_as_interrupted() {
    let mut session = crate::session::Session::new("p", "m", 100_000);
    let mut buf = String::from("Running bash...");
    let mut calls = vec![
        crate::session::ToolCallEntry {
            id: "tc_abc".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "sleep 99"}),
            state: crate::session::ToolCallState::Interrupted,
        },
        crate::session::ToolCallEntry {
            id: "tc_xyz".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/etc/hostname"}),
            state: crate::session::ToolCallState::Completed {
                result: "myhost".to_string(),
            },
        },
    ];
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut calls);
    assert!(stashed);
    assert!(calls.is_empty(), "tool_calls_buf must be drained on stash");

    let last = session.messages.last().unwrap();
    assert_eq!(last.tool_calls.len(), 2);
    let interrupted = last
        .tool_calls
        .iter()
        .find(|e| e.id == "tc_abc")
        .expect("missing interrupted entry");
    assert!(matches!(
        interrupted.state,
        crate::session::ToolCallState::Interrupted,
    ));
    let completed = last
        .tool_calls
        .iter()
        .find(|e| e.id == "tc_xyz")
        .expect("missing completed entry");
    match &completed.state {
        crate::session::ToolCallState::Completed { result } => {
            assert_eq!(result, "myhost");
        }
        other => panic!("expected Completed; got {other:?}"),
    }
}

#[test]
fn capture_partial_on_abort_stashes_partial_with_trailer() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::from("I was about to explain that");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    assert!(stashed);
    assert_eq!(session.messages.len(), baseline + 1);
    let last = session.messages.last().unwrap();
    assert_eq!(last.role, crate::session::MessageRole::Assistant);
    assert!(
        last.content.contains("I was about to explain that"),
        "must keep the original partial: {:?}",
        last.content,
    );
    assert!(
        last.content.contains("[interrupted by user (Ctrl+C)]"),
        "must include the interruption trailer: {:?}",
        last.content,
    );
    assert!(buf.is_empty(), "buf must be cleared after stash");
}

// Aborting when nothing has streamed yet is a no-op — we don't
// want a session full of empty "[interrupted]" messages from
// mistaken Ctrl+C presses.
#[test]
fn capture_partial_on_abort_noop_on_empty_buf() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::new();
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    assert!(!stashed);
    assert_eq!(session.messages.len(), baseline);
}

// Whitespace-only partial (e.g. agent had only emitted some
// leading newlines) is also a no-op — no useful text to save.
#[test]
fn capture_partial_on_abort_noop_on_whitespace_only() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::from("   \n\n\t  ");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Esc", 0, &mut Vec::new());
    assert!(!stashed);
    assert_eq!(session.messages.len(), baseline);
}

// When tool calls ran in the same turn as the abort, the trailer
// must say so. The agent's preserved text only covers what was
// streamed via `AgentEvent::Token`; tool calls + results emitted
// separately are NOT in `response_buf`. Without this hint the
// next turn's LLM would see the partial as a definitive "this
// was the assistant's response" and could re-run side-effecting
// tool calls.
#[test]
fn capture_partial_on_abort_trailer_notes_tool_calls() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let mut buf = String::from("I deleted the file");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut Vec::new());
    assert!(stashed);
    let content = &session.messages.last().unwrap().content;
    assert!(
        content.contains("I deleted the file"),
        "partial text dropped: {content:?}",
    );
    assert!(
        content.contains("[interrupted by user (Ctrl+C);"),
        "trailer prefix changed: {content:?}",
    );
    assert!(
        content.contains("2 tool call"),
        "trailer must mention tool call count: {content:?}",
    );
    assert!(
        content.contains("not preserved"),
        "trailer must warn that tool calls were not preserved: {content:?}",
    );
}

// Single tool call uses singular phrasing — "1 tool call ran" not
// "1 tool calls ran". Tiny but the LLM is reading this verbatim.
#[test]
fn capture_partial_on_abort_trailer_handles_singular_tool_call() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let mut buf = String::from("Running tests now");
    capture_partial_on_abort(&mut buf, &mut session, "Esc", 1, &mut Vec::new());
    let content = &session.messages.last().unwrap().content;
    assert!(
        content.contains("1 tool call ran"),
        "expected singular phrasing for 1 tool call: {content:?}",
    );
    assert!(
        !content.contains("1 tool calls ran"),
        "leaked plural for singular case: {content:?}",
    );
}

// Rewind must sync tree.entries + message_store + leaf_id with
// the truncated `messages` slice. Without this, the tree
// references orphaned ids that no longer have content, and the
// leaf_id can point past the truncation. Subsequent fork /
// clone / save-load operations either fail or carry stale ids.
#[test]
fn rewind_truncates_tree_and_store_in_sync_with_messages() {
    let mut session = crate::session::Session::new("p", "m", 100_000);
    session.add_message(crate::session::MessageRole::User, "u1");
    session.add_message(crate::session::MessageRole::Assistant, "a1");
    session.add_message(crate::session::MessageRole::User, "u2");
    session.add_message(crate::session::MessageRole::Assistant, "a2");
    let baseline_tree = session.tree.entries.len();
    assert_eq!(baseline_tree, 4, "fixture: 4 entries");

    // Rewind back to the first user message (idx=1 in the
    // reverse-order user list means the *first* user).
    let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
    // idx=0 = "rewind through the most recent user prompt" → cut
    // at the position of u2 → messages become [u1, a1].
    let _ = rewind_session(&mut session, 0, &mut renderer);

    // After rewind, messages has [u1, a1]; tree must agree.
    assert_eq!(session.messages.len(), 2);
    assert_eq!(
        session.tree.entries.len(),
        session.messages.len(),
        "tree entries must match messages count; got tree={}, msgs={}",
        session.tree.entries.len(),
        session.messages.len(),
    );
    assert_eq!(
        session.message_store.len(),
        session.messages.len(),
        "store must match messages count",
    );
    // Leaf points to the last remaining message.
    let last_id = session.messages.last().unwrap().id.clone();
    assert_eq!(
        session.tree.leaf_id,
        Some(last_id.clone()),
        "leaf_id must anchor to the new tail",
    );
    // Every remaining message id has a tree entry + store entry.
    for m in &session.messages {
        assert!(
            session.tree.entries.contains_key(&m.id),
            "missing tree entry for {}",
            m.id,
        );
        assert!(
            session.message_store.contains_key(&m.id),
            "missing store entry for {}",
            m.id,
        );
    }
}

// Rewinding restores the working tree, not just the conversation:
// files mutated during the rewound prompt(s) are rolled back to
// their pre-prompt content, keyed by the user message id the
// snapshot turn was opened with.
#[test]
fn rewind_restores_files_to_pre_prompt_state() {
    use crate::agent::tools::snapshots;
    let _gate = {
        use crate::sync_util::LockExt;
        snapshots::TEST_GATE.lock_ignore_poison()
    };
    snapshots::clear();

    let dir = std::env::temp_dir().join(format!("dirge-rewind-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("work.txt");
    std::fs::write(&file, "original").unwrap();

    let mut session = crate::session::Session::new("p", "m", 100_000);

    // Turn u1: open snapshot turn, capture pre-state, mutate.
    session.add_message(crate::session::MessageRole::User, "u1");
    let uid1 = session.messages.last().unwrap().id.clone();
    snapshots::begin_turn(&uid1);
    snapshots::capture(&file);
    std::fs::write(&file, "edited by u1").unwrap();
    session.add_message(crate::session::MessageRole::Assistant, "a1");

    // Turn u2: another mutation.
    session.add_message(crate::session::MessageRole::User, "u2");
    let uid2 = session.messages.last().unwrap().id.clone();
    snapshots::begin_turn(&uid2);
    snapshots::capture(&file);
    std::fs::write(&file, "edited by u2").unwrap();
    session.add_message(crate::session::MessageRole::Assistant, "a2");

    // Rewind back through BOTH user prompts (idx=1 → cut at u1).
    let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
    let _ = rewind_session(&mut session, 1, &mut renderer);

    let after = std::fs::read_to_string(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    snapshots::clear();
    assert_eq!(
        after, "original",
        "rewinding to u1 must restore the file to its pre-u1 content"
    );
}

// The token accumulator on the abort path keeps `total_tokens`
// in sync with `total_estimated_tokens`. Both fields are
// TODO(cost-tracking) placeholders today but the inconsistency
// between Done/Interjected (which both update total_tokens) and
// abort (which didn't) made the abort case look like the agent
// produced zero tokens that turn.
#[test]
fn capture_partial_on_abort_keeps_total_tokens_in_sync() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline_total = session.total_tokens;
    let baseline_est = session.total_estimated_tokens;
    let mut buf = String::from(
        "A reasonably long partial response that should produce a non-zero token estimate.",
    );
    capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    // Both fields advanced by the same amount (the stashed
    // message's estimated_tokens). Without the parity fix, only
    // total_estimated_tokens moved.
    assert!(
        session.total_estimated_tokens > baseline_est,
        "total_estimated_tokens should advance on stash",
    );
    assert_eq!(
        session.total_tokens.saturating_sub(baseline_total),
        session.total_estimated_tokens.saturating_sub(baseline_est),
        "total_tokens must advance in lockstep with total_estimated_tokens",
    );
}

// Regression H1: lifecycle line for a failed task previously embedded the
// raw error string. Renderer.write_line splits on '\n', so a multi-line
// error broke the line layout (color reset, closing ']' on its own row).
// sanitize_single_line must collapse newlines into spaces.
#[test]
fn sanitize_replaces_newlines_with_space() {
    let s = sanitize_single_line("line one\nline two\nline three", 100);
    assert_eq!(s, "line one line two line three");
    assert!(!s.contains('\n'));
}

#[test]
fn sanitize_replaces_carriage_return_and_tab() {
    let s = sanitize_single_line("a\rb\tc", 100);
    assert_eq!(s, "a b c");
}

// Regression: ANSI escape sequences (ESC = 0x1B) would otherwise be
// emitted verbatim and corrupt terminal state.
#[test]
fn sanitize_strips_ansi_escape() {
    let s = sanitize_single_line("hello \x1b[31mred\x1b[0m world", 100);
    assert!(!s.contains('\x1b'));
    assert!(s.contains("hello"));
    assert!(s.contains("world"));
}

// Other ASCII control chars (bell, backspace, etc.) are also stripped.
#[test]
fn sanitize_strips_other_controls() {
    let s = sanitize_single_line("a\x07b\x08c\x00d", 100);
    // Each control disappears; visible chars remain in order.
    assert_eq!(s, "abcd");
}

#[test]
fn sanitize_truncates_at_char_limit() {
    let s = sanitize_single_line(&"x".repeat(200), 50);
    // 50 x's + ellipsis.
    assert_eq!(s.chars().count(), 51);
    assert!(s.ends_with('…'));
}

#[test]
fn sanitize_does_not_truncate_when_within_limit() {
    let s = sanitize_single_line("hello", 100);
    assert_eq!(s, "hello");
    assert!(!s.ends_with('…'));
}

// Multibyte content counts by chars, not bytes, and remains intact.
#[test]
fn sanitize_handles_utf8_correctly() {
    let s = sanitize_single_line("🦀🦀🦀\n🦀🦀", 100);
    assert_eq!(s, "🦀🦀🦀 🦀🦀");
}

// Truncation at a multibyte boundary must produce valid UTF-8.
#[test]
fn sanitize_truncation_does_not_split_multibyte() {
    let s = sanitize_single_line("🦀🦀🦀🦀🦀", 3);
    // 3 emojis + ellipsis. No broken bytes.
    assert_eq!(s.chars().count(), 4);
    assert!(s.ends_with('…'));
    // Round-trip as &str succeeds.
    let _ = s.as_str();
}

#[test]
fn with_queue_hides_zero_count() {
    // No interjections waiting → status line unchanged so the user
    // doesn't see ambient "q:0" noise during normal operation.
    let s = with_queue("ready".to_string(), 0);
    assert_eq!(s, "ready");
}

#[test]
fn with_queue_appends_count() {
    let s = with_queue("running".to_string(), 3);
    assert!(s.ends_with("q:3"));
    assert!(s.starts_with("running"));
}

/// User bug: `read` output containing a tab caused the chamber's
/// right border to drift right. `\t` has Unicode width 0 but the
/// terminal renders it as 4+ cells, so width-based padding
/// undercounted. The fix expands tabs to spaces (stop=4) before
/// measurement so the right `│` lands at the expected column.
#[test]
fn chamber_row_right_border_aligns_with_tabs() {
    use unicode_width::UnicodeWidthStr;
    let inner = 60;
    // Three rows: no tab, one tab at start, tab embedded mid-line.
    // After tab-expansion all should produce equal display width.
    let rows = [
        chamber_row("plain text", inner),
        chamber_row("\tindented", inner),
        chamber_row("2:\t(cd ..; make library)", inner),
    ];
    let widths: Vec<usize> = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.as_str()))
        .collect();
    // All rows occupy exactly `inner + 4` cells (`│ ` + inner + ` │`).
    let expected = inner + 4;
    for (r, w) in rows.iter().zip(widths.iter()) {
        assert_eq!(
            *w, expected,
            "chamber row width mismatch — content {r:?} measured {w} cells, want {expected}"
        );
    }
    // Sanity: every row ends with `│` (right border didn't get
    // pushed off into oblivion by under-padded tab).
    for r in &rows {
        assert!(r.ends_with('│'), "row {r:?} missing right border");
    }
}

/// `chamber_row_with_bg` gets the same tab-expansion treatment so
/// diff `+`/`-` lines whose source uses tab indentation also
/// align correctly.
#[test]
fn chamber_row_with_bg_right_border_aligns_with_tabs() {
    use unicode_width::UnicodeWidthStr;
    let inner = 60;
    let row = chamber_row_with_bg("+\tadded line", inner, 22);
    // chamber_row_with_bg wraps content in SGR escapes; the
    // visible width should still be inner + 4.
    let visible = crate::ui::wrap::visible_width(&row);
    assert_eq!(visible, inner + 4);
    // Plain UnicodeWidthStr counts SGR payload too, but the
    // visible-width helper from `wrap.rs` is the right tool.
    // Sanity-only width assertion via the visible helper.
    let _ = UnicodeWidthStr::width(row.as_str());
    assert!(row.ends_with('│'));
}

/// dirge chamber-width fix: a chamber box must span the full painted chat
/// band (`Layout::chat.width - 1`), not the gutter-blind, 120-capped
/// `content_width`. With panels hidden the band reclaims both gutters, so
/// the box must widen with it — otherwise a dead strip is left on the
/// right where stale border glyphs showed.
#[test]
fn chamber_spans_full_chat_band_when_panels_hidden() {
    use crate::ui::tool_display::chamber_widths;
    let mut r = Renderer::new().expect("renderer");
    // Wide gutter but below the panel auto-show threshold (152), so both
    // side panels are hidden and the chat band reclaims their gutters.
    r.set_test_cols(148);
    assert!(
        !r.left_panel_visible() && !r.right_panel_visible(),
        "panels should be hidden at 148 cols"
    );

    let band = r.chat_band_width();
    let (frame_w, inner) = chamber_widths(&r);
    // Right │ lands on the last painted cell (ChatPane paints chat.width-1).
    assert_eq!(frame_w, band - 1, "chamber must fill the painted band");
    assert_eq!(inner + 4, frame_w);
    // The band genuinely exceeds the old 120-capped content_width — i.e.
    // the dead strip the artifacts lived in is gone.
    assert!(
        band > r.content_width(),
        "band should reclaim gutters past the 120 cap (band={band}, content_width={})",
        r.content_width(),
    );
}

/// No regression when panels ARE shown: the band is capped at the panel
/// layout width, so the chamber width is unchanged from the old behavior.
#[test]
fn chamber_width_unchanged_when_panels_shown() {
    use crate::ui::tool_display::chamber_widths;
    let mut r = Renderer::new().expect("renderer");
    r.set_test_cols(200); // >= 152 and wide gutter → panels auto-show
    assert!(
        r.left_panel_visible() && r.right_panel_visible(),
        "panels should be visible at 200 cols"
    );
    let (frame_w, _) = chamber_widths(&r);
    // Panels take the gutter, so the band is the 120-cap and the chamber
    // matches the pre-fix content_width-1.
    assert_eq!(frame_w, r.chat_band_width() - 1);
    assert_eq!(frame_w, r.content_width() - 1);
}

/// Chat window switching: next / prev index math wraps correctly.
#[test]
fn chat_index_next_prev_wraps() {
    // Simulate 3 chats (0=main, 1, 2).
    let count: usize = 3;
    // Ctrl+N: next = (active + 1) % count
    for (active, expected) in [(0usize, 1usize), (1, 2), (2, 0)] {
        assert_eq!((active + 1) % count, expected, "next from {active}");
    }
    // Ctrl+P: prev = (active + count - 1) % count
    for (active, expected) in [(0usize, 2usize), (2, 1), (1, 0)] {
        assert_eq!((active + count - 1) % count, expected, "prev from {active}");
    }
}

/// Chat window switching: single chat is a no-op.
#[test]
fn chat_index_next_prev_one_chat_is_noop() {
    let count: usize = 1;
    let active: usize = 0;
    assert_eq!((active + 1) % count, 0);
    assert_eq!((active + count - 1) % count, 0);
}

// ============================================================
// safe_during_agent — slash commands allowed while agent runs
// ============================================================

#[test]
fn mode_is_safe_during_agent() {
    assert!(is_safe_during_agent("/mode"));
    assert!(is_safe_during_agent("/mode yolo"));
    assert!(is_safe_during_agent("/mode standard"));
    assert!(is_safe_during_agent("/mode accept"));
    assert!(is_safe_during_agent("/mode restrictive"));
}

#[test]
fn quit_help_reasoning_tasks_always_safe_during_agent() {
    assert!(is_safe_during_agent("/quit"));
    assert!(is_safe_during_agent("/help"));
    assert!(is_safe_during_agent("/reasoning"));
    assert!(is_safe_during_agent("/tasks"));
    assert!(is_safe_during_agent("/tasks list"));
}

#[test]
fn sessions_tree_model_prompt_safe_only_without_args() {
    assert!(is_safe_during_agent("/sessions"));
    assert!(is_safe_during_agent("/tree"));
    assert!(is_safe_during_agent("/model"));
    assert!(is_safe_during_agent("/prompt"));
    assert!(!is_safe_during_agent("/sessions 42"));
    assert!(!is_safe_during_agent("/model gpt-4"));
    assert!(!is_safe_during_agent("/prompt my-prompt"));
}

#[test]
fn mutating_commands_are_not_safe_during_agent() {
    assert!(!is_safe_during_agent("/cd /tmp"));
    assert!(!is_safe_during_agent("/clear"));
    assert!(!is_safe_during_agent("/compress"));
    assert!(!is_safe_during_agent("/clone"));
    assert!(!is_safe_during_agent("/fork"));
    assert!(!is_safe_during_agent("/compact"));
    assert!(!is_safe_during_agent("/undo"));
    assert!(!is_safe_during_agent("/retry"));
    assert!(!is_safe_during_agent("/allow bash rm *"));
}

#[test]
fn memory_skill_list_safe_during_agent() {
    assert!(is_safe_during_agent("/memory list"));
    assert!(is_safe_during_agent("/skill list"));
    assert!(!is_safe_during_agent("/memory add key value"));
    assert!(!is_safe_during_agent("/skill load foo"));
}

// ============================================================
// scroll_snap_for — typing / Down jump the scrolled-up chat to bottom
// ============================================================

#[test]
fn scroll_snap_only_down_jumps_typing_leaves_scroll_alone() {
    use crossterm::event::KeyEvent;
    let none = KeyModifiers::NONE;

    // Typing does NOT snap — you can read history while composing.
    assert_eq!(
        scroll_snap_for(&KeyEvent::new(KeyCode::Char('a'), none)),
        None
    );
    // Shift+char (a capital) is still typing — still no snap.
    assert_eq!(
        scroll_snap_for(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        None
    );
    // Plain Down → jump to bottom and consume the key.
    assert_eq!(
        scroll_snap_for(&KeyEvent::new(KeyCode::Down, none)),
        Some(ScrollSnap::Jump)
    );

    // Command combos (Ctrl/Alt/Super) and other keys leave the scroll alone.
    assert_eq!(
        scroll_snap_for(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(
        scroll_snap_for(&KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
        None
    );
    assert_eq!(scroll_snap_for(&KeyEvent::new(KeyCode::Up, none)), None);
    assert_eq!(scroll_snap_for(&KeyEvent::new(KeyCode::Enter, none)), None);
}

// ============================================================
// dirge #444 — live thinking streams into the Ctrl+O panel
// ============================================================

/// After Ctrl+O expands a live thinking burst, new reasoning deltas re-render
/// the expanded block IN PLACE (anchored at the same start) rather than leaving
/// the frozen snapshot the user first saw.
#[test]
fn live_thinking_expansion_streams_in_place() {
    let mut r = Renderer::new().expect("renderer");
    r.write_line("<you> hi", Color::White).unwrap();
    r.write_line(
        "  ◇ thinking… (Ctrl+O to view)",
        crate::ui::theme::thinking(),
    )
    .unwrap();

    // Ctrl+O expands the current snapshot of the thinking-so-far.
    let start = r.buffer_len();
    render_thinking_block(&mut r, "first thought").unwrap();
    let anchor = (start, r.buffer_len(), r.eviction_generation());
    let snap: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    assert!(snap.iter().any(|l| l.contains("first thought")));
    assert!(
        !snap.iter().any(|l| l.contains("second thought")),
        "snapshot must not contain text that hasn't streamed yet",
    );

    // More reasoning arrives — re-render in place with the fuller buffer.
    let updated = restream_expanded_thinking(&mut r, anchor, "first thought\nsecond thought")
        .unwrap()
        .expect("re-rendered in place");

    let now: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    assert!(now.iter().any(|l| l.contains("first thought")));
    assert!(
        now.iter().any(|l| l.contains("second thought")),
        "new thinking streamed into the expanded block: {now:?}",
    );
    // Exactly one of each — the old block was replaced, not duplicated.
    assert_eq!(
        now.iter().filter(|l| l.contains("first thought")).count(),
        1
    );
    assert_eq!(updated.0, start, "block stays anchored at the same start");
    // The placeholder above the block is untouched.
    assert!(now.iter().any(|l| l.contains("thinking… (Ctrl+O to view)")));
}

/// When front-eviction has shifted indices since the block was anchored
/// (gen mismatch), the in-place re-render bails (returns None) so the caller
/// stops tracking instead of truncating live content at a stale index.
#[test]
fn restream_bails_on_eviction_generation_mismatch() {
    let mut r = Renderer::new().expect("renderer");
    render_thinking_block(&mut r, "thought").unwrap();
    // Anchor with a deliberately wrong eviction generation.
    let stale = (
        0usize,
        r.buffer_len(),
        r.eviction_generation().wrapping_add(1),
    );
    let out = restream_expanded_thinking(&mut r, stale, "thought more").unwrap();
    assert!(
        out.is_none(),
        "stale-generation anchor must not re-render in place"
    );
}

/// dirge #448 finding 1: when content has been appended below the anchored
/// block (the block is no longer at the buffer tail), restreaming must bail
/// (return None) instead of truncating from `start` to the END of the buffer
/// and silently destroying that appended content.
#[test]
fn restream_bails_when_block_buried_below_tail() {
    let mut r = Renderer::new().expect("renderer");
    r.write_line("<you> hi", Color::White).unwrap();

    let start = r.buffer_len();
    render_thinking_block(&mut r, "first thought").unwrap();
    let anchor = (start, r.buffer_len(), r.eviction_generation());

    // Something appends below the block — it's no longer at the tail.
    r.write_line("response token", Color::White).unwrap();

    let out = restream_expanded_thinking(&mut r, anchor, "first thought\nsecond thought").unwrap();
    assert!(
        out.is_none(),
        "buried anchor (content below the block) must not re-render in place"
    );

    let now: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    assert!(
        now.iter().any(|l| l.contains("response token")),
        "appended content below the block must survive a bailed restream: {now:?}"
    );
    assert!(
        !now.iter().any(|l| l.contains("second thought")),
        "bailed restream must not stream new thinking: {now:?}"
    );
}

/// dirge-8p79: the per-delta restream is coalesced, so the last deltas of a
/// burst can be unpainted when the burst ends. `freeze_live_thinking` flushes
/// the full buffer one final time at the boundary so the frozen block is
/// complete, and stops live tracking.
#[test]
fn freeze_live_thinking_flushes_coalesced_tail() {
    let mut r = Renderer::new().expect("renderer");
    let start = r.buffer_len();
    // The block was last painted with only the first delta (coalescing skipped
    // the rest as more events were still queued).
    render_thinking_block(&mut r, "first thought").unwrap();
    let mut anchor = Some((start, r.buffer_len(), r.eviction_generation()));
    let mut expanded = true;

    freeze_live_thinking(
        &mut r,
        &mut anchor,
        &mut expanded,
        "first thought\nsecond thought",
    )
    .unwrap();

    let now: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    assert!(
        now.iter().any(|l| l.contains("second thought")),
        "coalesced tail flushed into the frozen block: {now:?}"
    );
    assert_eq!(
        now.iter().filter(|l| l.contains("first thought")).count(),
        1,
        "block replaced, not duplicated: {now:?}"
    );
    assert!(!expanded, "live tracking stops after the block freezes");
}

/// `freeze_live_thinking` is a no-op when nothing is being live-tracked — it
/// must not touch the buffer or re-render anything.
#[test]
fn freeze_live_thinking_noop_when_not_tracking() {
    let mut r = Renderer::new().expect("renderer");
    render_thinking_block(&mut r, "done thinking").unwrap();
    let len_before = r.buffer_len();
    let mut anchor = None;
    let mut expanded = false;

    freeze_live_thinking(&mut r, &mut anchor, &mut expanded, "done thinking\nmore").unwrap();

    assert_eq!(
        r.buffer_len(),
        len_before,
        "buffer untouched when not tracking"
    );
    assert!(anchor.is_none());
    assert!(!expanded);
}

/// A long line in the expanded thinking block must wrap with the `│` bar on
/// EVERY row — previously write_line wrapped `  │ {line}` with no continuation
/// prefix, so wrapped rows dropped the bar and started at column 0, escaping
/// the box. Width-agnostic: content_width caps at 120, so a 600-char line
/// always wraps to multiple rows.
#[test]
fn expanded_thinking_wrapped_rows_keep_the_bar() {
    let mut r = Renderer::new().expect("renderer");
    let long = "word ".repeat(120);
    render_thinking_block(&mut r, long.trim()).unwrap();

    let lines: Vec<String> = r.buffer_lines().iter().map(|s| s.to_string()).collect();
    let header = lines
        .iter()
        .position(|l| l.contains("╭─ thinking"))
        .expect("thinking header present");
    let footer = lines
        .iter()
        .position(|l| l.contains("╰─"))
        .expect("thinking footer present");
    let content = &lines[header + 1..footer];

    assert!(
        content.len() >= 2,
        "a 600-char line must wrap to multiple rows: {content:?}"
    );
    for row in content {
        assert!(
            row.starts_with("  │"),
            "every wrapped thinking row must keep the bar (stay in the box): {row:?}"
        );
    }
}

// ============================================================
// real_user_prompt — history seeding filter
// ============================================================
use crate::session::{MessageRole, SessionMessage};

fn user_msg(content: &str) -> SessionMessage {
    SessionMessage {
        role: MessageRole::User,
        content: compact_str::CompactString::new(content),
        estimated_tokens: 0,
        id: compact_str::CompactString::new("m"),
        timestamp: 0,
        tool_calls: Vec::new(),
        images: Vec::new(),
    }
}

#[test]
fn real_user_prompt_passes_real_input() {
    assert_eq!(
        super::real_user_prompt(&user_msg("run tests")),
        Some("run tests")
    );
}

#[test]
fn real_user_prompt_strips_system_reminder_wrapper() {
    let m = user_msg("<system-reminder>\nctx\n</system-reminder>\n\nwhat's next?");
    assert_eq!(super::real_user_prompt(&m), Some("what's next?"));
}

#[test]
fn real_user_prompt_rejects_synthetic_turns() {
    // Non-user role.
    let mut m = user_msg("x");
    m.role = MessageRole::Assistant;
    assert_eq!(super::real_user_prompt(&m), None);
    // Empty after strip.
    assert_eq!(super::real_user_prompt(&user_msg("")), None);
    assert_eq!(
        super::real_user_prompt(&user_msg("<system-reminder>x</system-reminder>")),
        None,
    );
    // Mid-turn steer + auto-continue markers.
    assert_eq!(
        super::real_user_prompt(&user_msg("[Mid-turn steer from user] hey")),
        None,
    );
    assert_eq!(
        super::real_user_prompt(&user_msg(
            "Continue based on the background task results above."
        )),
        None,
    );
}

// ============================================================
// shell_overlay_rows — vt100 screen collapses cursor redraws
// ============================================================

/// The fix for the `gh auth login` rendering bug: an interactive prompt that
/// redraws a line in place (cursor-up + erase-line + reprint) must update the
/// same screen row, not append a second copy. A naive append of ansi-stripped
/// text would stack every redraw (the screen filled with repeated menus); the
/// vt100 screen parser shows only the final state.
#[test]
fn shell_overlay_rows_collapses_cursor_redraws() {
    let mut p = vt100::Parser::new(4, 24, 0);
    p.process(b"? pick one\r\n");
    p.process(b"> GitHub.com\r\n");
    // Emulate a survey redraw after pressing Down: move the cursor back up to
    // the selection line, erase it, and reprint with the marker moved.
    p.process(b"\x1b[1A"); // up 1
    p.process(b"\x1b[2K"); // erase entire line
    p.process(b"> Other");
    let rows = shell_overlay_rows(&p);
    let lines: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(lines, vec!["? pick one", "> Other"]);
    // The stale selection must not linger anywhere on the screen.
    assert!(!lines.iter().any(|l| l.contains("GitHub.com")));
}

// ============================================================
// option_select_action — question-modal keystroke mapping (dirge-72h2)
// ============================================================

use crossterm::event::KeyCode as KC;

// Multi-select, cursor on an option row.

#[test]
fn opt_multi_space_toggles_option() {
    // Space on an option row toggles its checkbox.
    let a = option_select_action(KC::Char(' '), true, true, 0, 3, false);
    assert_eq!(a, OptionAction::Toggle);
}

#[test]
fn opt_multi_enter_on_option_confirms() {
    let a = option_select_action(KC::Enter, true, true, 1, 3, false);
    assert_eq!(a, OptionAction::Confirm);
}

// Multi-select, cursor on the (custom) row.

#[test]
fn opt_multi_custom_row_enter_opens_editor_when_empty() {
    // No custom text yet: Enter starts typing.
    let a = option_select_action(KC::Enter, true, true, 3, 3, false);
    assert_eq!(a, OptionAction::OpenEntry);
}

#[test]
fn opt_multi_custom_row_enter_confirms_once_text_entered() {
    // The bug: after typing a custom answer, Enter must confirm — not
    // re-open the editor forever.
    let a = option_select_action(KC::Enter, true, true, 3, 3, true);
    assert_eq!(a, OptionAction::Confirm);
}

#[test]
fn opt_multi_custom_row_space_reopens_editor() {
    // Space on the custom row (re)opens the editor to edit the answer,
    // whether or not text already exists.
    assert_eq!(
        option_select_action(KC::Char(' '), true, true, 3, 3, false),
        OptionAction::OpenEntry
    );
    assert_eq!(
        option_select_action(KC::Char(' '), true, true, 3, 3, true),
        OptionAction::OpenEntry
    );
}

// Single-select.

#[test]
fn opt_single_enter_on_option_confirms() {
    let a = option_select_action(KC::Enter, false, false, 2, 4, false);
    assert_eq!(a, OptionAction::Confirm);
}

#[test]
fn opt_single_space_on_option_confirms() {
    // Single-select has no checkboxes: Space picks the option like Enter.
    let a = option_select_action(KC::Char(' '), false, false, 2, 4, false);
    assert_eq!(a, OptionAction::Confirm);
}

#[test]
fn opt_single_custom_row_enter_opens_editor() {
    // Single-select custom row always opens the editor (typing auto-submits).
    let a = option_select_action(KC::Enter, false, true, 4, 4, false);
    assert_eq!(a, OptionAction::OpenEntry);
}

// Navigation / rejection, shared across modes.

#[test]
fn opt_arrow_and_vim_navigation() {
    assert_eq!(
        option_select_action(KC::Up, true, true, 1, 3, false),
        OptionAction::CursorUp
    );
    assert_eq!(
        option_select_action(KC::Char('k'), true, true, 1, 3, false),
        OptionAction::CursorUp
    );
    assert_eq!(
        option_select_action(KC::Down, true, true, 1, 3, false),
        OptionAction::CursorDown
    );
    assert_eq!(
        option_select_action(KC::Char('j'), true, true, 1, 3, false),
        OptionAction::CursorDown
    );
}

#[test]
fn opt_esc_rejects() {
    assert_eq!(
        option_select_action(KC::Esc, true, true, 0, 3, false),
        OptionAction::Reject
    );
}

#[test]
fn opt_unhandled_key_is_ignored() {
    assert_eq!(
        option_select_action(KC::Tab, true, true, 0, 3, false),
        OptionAction::Ignore
    );
}

#[test]
fn opt_space_off_options_without_custom_is_ignored() {
    // No custom row and cursor somehow at num_options: Space does nothing.
    let a = option_select_action(KC::Char(' '), true, false, 3, 3, false);
    assert_eq!(a, OptionAction::Ignore);
}

// ============================================================
// panel_refresh_due — ambient-panel throttle (dirge-b14h)
// ============================================================

#[test]
fn panel_refresh_due_none_is_true() {
    assert!(panel_refresh_due(
        None,
        std::time::Instant::now(),
        PANEL_REFRESH_INTERVAL
    ));
}

#[test]
fn panel_refresh_due_within_interval_is_false() {
    let now = std::time::Instant::now();
    assert!(!panel_refresh_due(Some(now), now, PANEL_REFRESH_INTERVAL));
}

#[test]
fn panel_refresh_due_past_interval_is_true() {
    let now = std::time::Instant::now();
    let past = now - PANEL_REFRESH_INTERVAL;
    assert!(panel_refresh_due(Some(past), now, PANEL_REFRESH_INTERVAL));
}
