//! dirge-5h5 reproduction harness.
//!
//! Isolated repro for the "first 6 chambers show TOP+BOTTOM only,
//! LAST chamber has full content" bug on N parallel reads. Drives the
//! chamber state machine directly — same `Renderer` + same
//! `handle_tool_result` the production path uses — but bypasses the
//! tokio::select! / bridge / agent_loop layers so we can isolate the
//! chamber logic from any concurrency-layer interactions.
//!
//! Each test:
//!   1. Constructs a fresh Renderer + RunCtx scaffolding.
//!   2. Simulates N ToolCall events by inlining the same chamber-TOP
//!      paint logic from `ui/mod.rs::AgentEvent::ToolCall`.
//!   3. Simulates N ToolResult events by calling the production
//!      `handle_tool_result` in some order.
//!   4. Walks `renderer.buffer_lines()` and asserts every TOP has at
//!      least one body row before its matching BOTTOM.
//!
//! Run individually with:
//!   cargo test --features "<features>" --bin dirge -- dirge_5h5_repro
//!
//! Re-run with tracing on the production trace target to see the
//! per-event chamber state:
//!   RUST_LOG=dirge::ui::chamber=trace cargo test … -- --nocapture

use ansi_to_tui::IntoText;
use crossterm::style::Color;

use crate::cli::Cli;
use crate::config::Config;
use crate::session::Session;
use crate::ui::colors::c_tool;
use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;
use crate::ui::tool_display::{
    chamber_widths, close_tool_chamber_passive, fit_banner_header, format_tool_banner_value,
};

use super::RunCtx;
use super::tool_result::handle_tool_result;

/// Mirrors the inline ToolCall handling in `ui/mod.rs:1713+`. Kept in
/// sync with that path — if production logic drifts, update here.
fn simulate_tool_call(ctx: &mut RunCtx<'_>, id: &str, name: &str, args: serde_json::Value) {
    // Push to tool_calls_buf — production code does this too.
    ctx.tool_calls_buf.push(crate::session::ToolCallEntry {
        id: id.to_string(),
        name: name.to_string(),
        args: args.clone(),
        state: crate::session::ToolCallState::Interrupted,
    });
    *ctx.tool_calls_this_run = ctx.tool_calls_this_run.saturating_add(1);

    // Close any prior chamber via the passive path.
    close_tool_chamber_passive(
        ctx.renderer,
        ctx.last_tool_name,
        ctx.tool_chamber_open,
        ctx.chamber_top_start,
        ctx.chamber_top_end,
    )
    .expect("close passive");

    *ctx.last_tool_name = Some(name.to_string());
    *ctx.last_tool_call_id = Some(id.to_string());

    // Paint the chamber TOP: spacer + header.
    *ctx.chamber_top_start = Some(ctx.renderer.buffer_len());
    ctx.renderer.write_line("", Color::White).expect("spacer");
    let upper = name.to_ascii_uppercase();
    let raw_value = format_tool_banner_value(name, &args);
    let raw_value = sanitize_output(&raw_value).into_string();
    let (frame_w, _) = chamber_widths(ctx.renderer);
    let header = fit_banner_header(&upper, &raw_value, frame_w);
    ctx.renderer
        .write_line_raw(&header, c_tool())
        .expect("header");
    *ctx.chamber_top_end = Some(ctx.renderer.buffer_len());
    *ctx.tool_chamber_open = true;
}

/// One chamber's slice of the rendered buffer: the TOP banner, all
/// rows between it and the next BOTTOM, and the BOTTOM itself.
#[derive(Debug)]
struct Chamber {
    /// Buffer line index of the `╭─ NAME …` row.
    top_idx: usize,
    /// Buffer line index of the closing `╰─…` row.
    bottom_idx: usize,
    /// Number of body rows between TOP and BOTTOM (exclusive of both).
    body_rows: usize,
    /// The tool name extracted from the TOP banner ("READ", "WRITE", …).
    name: String,
    /// First body row's content, if any (for debugging the failure
    /// case — keep even though Debug derives it implicitly).
    #[allow(dead_code)]
    first_body: Option<String>,
}

/// Walk the buffer, pair every TOP with its next BOTTOM, count body
/// rows. Returns one entry per chamber in buffer order.
fn collect_chambers(renderer: &Renderer) -> Vec<Chamber> {
    let lines: Vec<String> = renderer
        .buffer_lines()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with('\u{256d}') {
            // `╭` — TOP row. Find next BOTTOM.
            let name = extract_banner_name(&lines[i]);
            let top_idx = i;
            let mut j = i + 1;
            while j < lines.len() && !lines[j].starts_with('\u{2570}') {
                j += 1;
            }
            if j < lines.len() {
                // Found BOTTOM. Body rows are i+1..j (exclusive of both).
                let body_rows = j.saturating_sub(top_idx).saturating_sub(1);
                let first_body = lines.get(top_idx + 1).cloned();
                out.push(Chamber {
                    top_idx,
                    bottom_idx: j,
                    body_rows,
                    name,
                    first_body,
                });
                i = j + 1;
            } else {
                // Unclosed chamber — still record so the assertion
                // surfaces it clearly.
                out.push(Chamber {
                    top_idx,
                    bottom_idx: usize::MAX,
                    body_rows: 0,
                    name,
                    first_body: None,
                });
                break;
            }
        } else {
            i += 1;
        }
    }
    out
}

fn extract_banner_name(top_line: &str) -> String {
    // Banner shape: `╭─ NAME ─ value ─...─╮` — split on `─` and take
    // the first non-empty trimmed token after the leading `╭─`.
    top_line
        .split('\u{2500}') // `─`
        .map(|s| s.trim())
        .find(|s| !s.is_empty() && !s.starts_with('\u{256d}'))
        .unwrap_or("?")
        .to_string()
}

/// Build a fresh (Cli, Config, Session, Renderer) tuple suitable for
/// driving RunCtx. Returns owned values so the test can build the
/// RunCtx with &mut refs.
fn fresh_scaffold() -> (Cli, Config, Session, Renderer) {
    use clap::Parser;
    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let session = Session::new("test-provider", "test-model", 200_000);
    let renderer = Renderer::new().expect("renderer");
    (cli, cfg, session, renderer)
}

/// Build a RunCtx<'_> from already-mutable scaffolding pieces.
/// Returns the RunCtx by-value (lifetime tied to the borrows).
#[allow(clippy::too_many_arguments)]
fn make_ctx<'a>(
    renderer: &'a mut Renderer,
    session: &'a mut Session,
    state: &'a mut State,
    cli: &'a Cli,
    cfg: &'a Config,
) -> RunCtx<'a> {
    RunCtx {
        renderer,
        session,
        response_buf: &mut state.response_buf,
        response_start_line: &mut state.response_start_line,
        reasoning_buf: &mut state.reasoning_buf,
        reasoning_start_line: &mut state.reasoning_start_line,
        agent_line_started: &mut state.agent_line_started,
        last_tool_name: &mut state.last_tool_name,
        last_tool_call_id: &mut state.last_tool_call_id,
        tool_chamber_open: &mut state.tool_chamber_open,
        chamber_top_start: &mut state.chamber_top_start,
        chamber_top_end: &mut state.chamber_top_end,
        tool_calls_buf: &mut state.tool_calls_buf,
        tool_calls_this_run: &mut state.tool_calls_this_run,
        last_collapsed: &mut state.last_collapsed,
        last_thinking: &mut state.last_thinking,
        expand_target: &mut state.expand_target,
        expansion_anchor: &mut state.expansion_anchor,
        last_user_prompt: &mut state.last_user_prompt,
        cli,
        cfg,
        active_plan: &mut state.active_plan,
    }
}

/// Free-standing State struct mirrored from the macro — kept as a
/// real type so `make_ctx` can take `&mut State`.
struct State {
    response_buf: String,
    response_start_line: Option<usize>,
    reasoning_buf: String,
    reasoning_start_line: Option<usize>,
    agent_line_started: bool,
    last_tool_name: Option<String>,
    last_tool_call_id: Option<String>,
    tool_chamber_open: bool,
    chamber_top_start: Option<usize>,
    chamber_top_end: Option<usize>,
    tool_calls_buf: Vec<crate::session::ToolCallEntry>,
    tool_calls_this_run: u32,
    last_collapsed: Option<crate::ui::tool_display::CollapsedToolResult>,
    last_thinking: Option<String>,
    expand_target: crate::ui::state::ExpandTarget,
    expansion_anchor: Option<(usize, usize, u64)>,
    last_user_prompt: String,
    active_plan: Option<crate::agent::plan::runtime::ActivePlan>,
}

impl State {
    fn new() -> Self {
        Self {
            response_buf: String::new(),
            response_start_line: None,
            reasoning_buf: String::new(),
            reasoning_start_line: None,
            agent_line_started: false,
            last_tool_name: None,
            last_tool_call_id: None,
            tool_chamber_open: false,
            chamber_top_start: None,
            chamber_top_end: None,
            tool_calls_buf: Vec::new(),
            tool_calls_this_run: 0,
            last_collapsed: None,
            last_thinking: None,
            expand_target: crate::ui::state::ExpandTarget::None,
            expansion_anchor: None,
            last_user_prompt: String::new(),
            active_plan: None,
        }
    }
}

/// Fire N ToolCall events and N ToolResult events in some `result_order`
/// permutation. Returns the rendered chambers. Bodies are
/// `BODY_TEMPLATE.replace("{i}", &i.to_string())` so each chamber has
/// unique, non-empty content.
async fn drive(n: usize, result_order: Vec<usize>) -> Vec<Chamber> {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();

    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    // Fire ToolCalls in dispatch order: c0, c1, ..., c(n-1).
    for i in 0..n {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
    }

    // Fire ToolResults in `result_order`. Body is "file {i} body line one\nfile {i} body line two".
    for i in result_order {
        let id = format!("call-{i}");
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");
    }

    let _ = ctx;
    collect_chambers(&renderer)
}

fn assert_all_chambers_have_body(chambers: &[Chamber], expected_count: usize) {
    assert_eq!(
        chambers.len(),
        expected_count,
        "expected {expected_count} chambers, got {}: {chambers:#?}",
        chambers.len()
    );
    let mut empties = Vec::new();
    for (i, c) in chambers.iter().enumerate() {
        if c.bottom_idx == usize::MAX {
            empties.push(format!(
                "chamber {i} ({}): UNCLOSED — TOP at line {} has no matching BOTTOM",
                c.name, c.top_idx
            ));
        } else if c.body_rows == 0 {
            empties.push(format!(
                "chamber {i} ({}): TOP+BOTTOM only — no body rows between lines {} and {}",
                c.name, c.top_idx, c.bottom_idx
            ));
        }
    }
    if !empties.is_empty() {
        panic!(
            "dirge-5h5 reproduced: {}/{} chambers have no body. Details:\n  {}",
            empties.len(),
            chambers.len(),
            empties.join("\n  ")
        );
    }
}

// ============================================================
// Scenarios
// ============================================================

/// Scenario A: 7 ToolCalls, 7 ToolResults in dispatch order.
/// Baseline — exercises the "id matches last_tool_call_id" path on
/// the FIRST result (because last_tool_call_id is the latest call's
/// id from the last ToolCall in the burst), and the dirge-jzj fresh-
/// chamber path on the rest.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_dispatch_order() {
    let chambers = drive(7, (0..7).collect()).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario B: 7 ToolCalls, 7 ToolResults in REVERSE order. The
/// FIRST result (c6) matches the last_tool_call_id (also c6 from
/// the last ToolCall in the burst). All subsequent results go
/// through the dirge-jzj path.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_reverse_order() {
    let chambers = drive(7, (0..7).rev().collect()).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario C: 7 ToolCalls, 7 ToolResults in a SCRAMBLED order. Tests
/// the case where the first result is neither the dispatch-first nor
/// dispatch-last — it ALWAYS hits the dirge-jzj path because the id
/// won't match last_tool_call_id.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_scrambled_order() {
    // Picked once, deterministic — no PRNG to keep CI repeatable.
    let order = vec![3, 0, 6, 1, 5, 2, 4];
    let chambers = drive(7, order).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario D: 2 ToolCalls, 2 ToolResults — minimum case that
/// exercises both paths (one id-match, one dirge-jzj). Useful for
/// localising the bug if only the larger N reproduces.
#[tokio::test]
async fn dirge_5h5_repro_two_parallel_reverse_order() {
    let chambers = drive(2, vec![0, 1]).await;
    assert_all_chambers_have_body(&chambers, 2);
}

/// Scenario E: 7 ToolCalls, 7 ToolResults, but the LAST result is
/// the one matching last_tool_call_id. Mirrors the user's report
/// shape (which seems to suggest the LAST chamber is the one that
/// rendered correctly).
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_match_arrives_last() {
    // ToolCall order: c0..c6 → last_tool_call_id = c6 after burst.
    // Result order: 0, 1, 2, 3, 4, 5, 6 — so c6 (the matching id)
    // arrives LAST. The first 6 (c0..c5) all go through dirge-jzj.
    let chambers = drive(7, vec![0, 1, 2, 3, 4, 5, 6]).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario F: Interleaved — alternate ToolCall and ToolResult.
/// Mirrors the SEQUENTIAL dispatch shape (one call, one result, etc.)
/// — confirms the chamber state machine handles that path cleanly.
/// Should always pass; if it fails we know the bug isn't parallel-
/// specific.
#[tokio::test]
async fn dirge_5h5_repro_interleaved_baseline() {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();
    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id.clone(), body)
            .await
            .expect("handle_tool_result");
    }
    let _ = ctx;
    let chambers = collect_chambers(&renderer);
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario G (Layer 4): 7 parallel reads, with `add_chat()` injected
/// at three points during the burst — before any ToolCall, mid-burst
/// (after 3 ToolCalls but before any ToolResult), and right after the
/// last ToolResult. Confirms that creating a subagent ChatSnapshot
/// while the parent chamber paint is in flight does NOT disturb the
/// active chat's buffer integrity. If this passes, `add_chat`'s
/// push-to-snapshot path is innocent and the empty-frames bug must
/// come from a layer above the renderer (event ordering / scheduling)
/// or from the on-screen viewport (paint_line dropping body rows).
#[tokio::test]
async fn dirge_5h5_repro_add_chat_during_burst() {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();
    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    // Subagent injected BEFORE the burst — exercises the path where a
    // chat is created cold and the active buffer should be untouched.
    let _pre = ctx.renderer.add_chat("subagent-pre");
    let buffer_len_after_pre = ctx.renderer.buffer_len();
    assert_eq!(
        buffer_len_after_pre, 0,
        "add_chat should not touch active buffer (pre)"
    );

    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
        // Inject a subagent mid-burst, right after the 4th ToolCall
        // (chamber TOP painted, no body yet — most fragile state for
        // close_tool_chamber_passive's drop-empty heuristic).
        if i == 3 {
            let len_before = ctx.renderer.buffer_len();
            let _mid = ctx.renderer.add_chat("subagent-mid");
            let len_after = ctx.renderer.buffer_len();
            assert_eq!(
                len_before, len_after,
                "add_chat mid-burst must not mutate active buffer length"
            );
        }
    }

    // Now drain results in dispatch order. add_chat after the last
    // result is the post-burst subagent-spawn shape.
    for i in 0..7 {
        let id = format!("call-{i}");
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");
    }
    let _post = ctx.renderer.add_chat("subagent-post");

    let _ = ctx;
    let chambers = collect_chambers(&renderer);
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario H (Layer 4): write_line returns Ok even when tui_terminal
/// is None (no TTY in tests). Confirms that the buffer storage path —
/// push_buffer_line → wrap_line → commit_partial — preserves every
/// line we ask it to store across a 7-chamber burst of mixed content
/// (header rows + body rows + bottom rows + blank spacers). Counts
/// emitted lines vs buffer.len() after the burst.
#[tokio::test]
async fn dirge_5h5_repro_buffer_integrity_after_burst() {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();
    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
    }
    for i in 0..7 {
        let id = format!("call-{i}");
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");
    }

    let _ = ctx;

    // Every chamber must have >= 1 body row distinct from TOP and BOTTOM.
    let chambers = collect_chambers(&renderer);
    assert_eq!(
        chambers.len(),
        7,
        "expected 7 chambers, got {}",
        chambers.len()
    );
    for (i, c) in chambers.iter().enumerate() {
        assert!(
            c.body_rows >= 1,
            "chamber {i} ({}) lost body rows: {:?}",
            c.name,
            c
        );
    }
}

/// Scenario G (Layer 2): simulates the `tokio::select!` race where the
/// `subagent_chat_rx` arm fires BETWEEN two consecutive parent
/// `AgentEvent::ToolResult` handlings. Recreates the dirge-5h5 shape
/// ("4 background subagents running concurrently with the parent")
/// by:
///   1. Adding a second (subagent) chat via `renderer.add_chat`.
///   2. Firing N parent ToolCalls (parent stays on chat 0).
///   3. Firing N parent ToolResults, but BETWEEN each one calling
///      `write_line_to_chat(subagent_idx, ...)` exactly as the
///      `subagent_chat_rx` arm does for `Spawn`/`Complete` events.
///
/// If the active-chat's chamber state survives the cross-chat write
/// untouched, all chambers render bodies. If `write_line_to_chat`
/// (or any helper it calls) leaks into the active `partial` /
/// `buffer` / `chamber_top_*` state, the assertion catches the
/// resulting empty TOP+BOTTOM chambers.
#[tokio::test]
async fn dirge_5h5_repro_subagent_writes_between_tool_results() {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();

    // Parent lives on chat 0 (the default). Pre-add a "subagent" chat
    // at idx 1 so the cross-write target exists. In production this
    // happens via `renderer.add_chat(name)` inside the
    // `SubagentChatEvent::Spawn` handler — the call is synchronous
    // and doesn't touch active chat state.
    let subagent_idx = renderer.add_chat("task: simulated subagent");
    assert_eq!(subagent_idx, 1, "subagent chat should be at idx 1");

    let mut state = State::new();
    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    // Fire 7 parent ToolCalls (parent is active_chat=0).
    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
    }

    // Fire 7 parent ToolResults in dispatch order. BETWEEN each pair,
    // simulate the subagent arm by writing to the inactive chat slot
    // — exactly as `subagent_chat_rx => write_line_to_chat(idx, ...)`
    // does in `ui/mod.rs:2884+`. Use the same `theme::user()` /
    // `theme::dim()` / `c_agent()` colors the production code uses
    // so any per-color side effects in `write_line_to_chat` would be
    // exercised here too.
    use crate::ui::colors::c_agent;
    use crate::ui::theme;
    for i in 0..7 {
        let id = format!("call-{i}");
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");
        // Cross-chat write, mimicking the subagent arm between events.
        let _ = ctx.renderer.write_line_to_chat(
            subagent_idx,
            "<you> simulated subagent prompt",
            theme::user(),
        );
        let _ = ctx
            .renderer
            .write_line_to_chat(subagent_idx, "(subagent running…)", theme::dim());
        let _ = ctx.renderer.write_line_to_chat(
            subagent_idx,
            "<dirge> simulated subagent result",
            c_agent(),
        );
    }

    let _ = ctx;
    let chambers = collect_chambers(&renderer);
    assert_all_chambers_have_body(&chambers, 7);
}

// ============================================================
// Layer 4 follow-up: `paint_line` only renders `text.lines[0]`.
// If `into_text()` ever produces > 1 line from a chamber-row
// string, the rest are silently dropped. These tests probe
// `ansi_to_tui::into_text` behaviour on real chamber-row
// outputs to confirm/refute that hypothesis.
// ============================================================

/// Drive `chamber_row` with realistic body content (1-line, plain
/// text, no SGR) — what `read`'s output looks like row-by-row after
/// `render_tool_output` slices it into single lines and calls
/// `chamber_row` on each. Asserts `into_text` produces exactly 1
/// rendered line per chamber-row string. If this ever returns > 1,
/// paint_line is silently dropping content.
#[test]
fn chamber_row_parses_to_single_line_under_into_text() {
    let inputs = [
        "1: hello world",
        "  fn main() {",
        "      let x = 42;",
        "",
        "    │ already-quoted │",
        "1: line with     spaces and tabs\t.",
        "let foo = bar; // comment",
        "// comment",
        "    return Ok(())",
        "}",
    ];
    for body in inputs {
        let row = crate::ui::box_render::row(crate::ui::box_render::BoxStyle::Rounded, body, 80);
        let parsed = row.as_str().into_text().expect("parse");
        assert_eq!(
            parsed.lines.len(),
            1,
            "chamber-row for {body:?} parsed to {} lines (paint_line only renders the first!): row={:?} parsed={:#?}",
            parsed.lines.len(),
            row,
            parsed,
        );
    }
}

/// Same as above but for `chamber_row_with_bg` — the edit-diff
/// rendering path (+/- rows with tinted backgrounds). bg_idx is
/// embedded as an SGR escape so into_text MUST parse it without
/// emitting a stray newline.
#[test]
fn chamber_row_with_bg_parses_to_single_line() {
    let inputs = ["+ added line", "- removed line", "  context line"];
    for body in inputs {
        let row = crate::ui::box_render::row_with_bg(
            crate::ui::box_render::BoxStyle::Rounded,
            body,
            80,
            22,
        );
        let parsed = row.as_str().into_text().expect("parse");
        assert_eq!(
            parsed.lines.len(),
            1,
            "chamber_row_with_bg for {body:?} parsed to {} lines: row={:?} parsed={:#?}",
            parsed.lines.len(),
            row,
            parsed,
        );
    }
}

/// Chamber TOP banner from `fit_banner_header` — exactly what the
/// production path writes for each parallel-read ToolCall. If
/// this ever parses to > 1 line, the chamber would render with a
/// blank TOP, not blank body — but worth checking either way.
#[test]
fn chamber_top_banner_parses_to_single_line() {
    for value in [
        "/tmp/file0.txt",
        "/tmp/very/long/path/with/many/segments/file.txt",
        "(no args)",
        "",
    ] {
        let header = fit_banner_header("READ", value, 80);
        let parsed = header.as_str().into_text().expect("parse");
        assert_eq!(
            parsed.lines.len(),
            1,
            "banner for value={value:?} parsed to {} lines: header={:?} parsed={:#?}",
            parsed.lines.len(),
            header,
            parsed,
        );
    }
}

/// The spacer row written before each chamber TOP — `write_line("",
/// Color::White)`. Empty string. Document the actual behaviour.
#[test]
fn empty_spacer_into_text_behaviour() {
    let parsed = "".into_text().expect("parse");
    assert!(
        parsed.lines.len() <= 1,
        "empty string parsed to {} lines",
        parsed.lines.len()
    );
}

/// Final aggressive stress test: mimic the issue's exact reproduction
/// shape as closely as possible. 7 parallel reads with REALISTIC
/// `read`-tool output (line-numbered prefixes from `read.rs`), 4
/// subagent-chat slots created and written to mid-burst, ToolResults
/// arriving in scrambled order. If THIS passes, the bug is genuinely
/// not reproducible at the chamber-state-machine layer.
#[tokio::test]
async fn dirge_5h5_repro_full_issue_shape() {
    use crate::ui::theme;
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();

    // Create 4 subagent chats up front, as if the parent has spawned
    // them and they're running concurrently (the issue's "4 background
    // subagents at the cap").
    let subagent_idxs: Vec<usize> = (0..4)
        .map(|i| {
            let idx = renderer.add_chat(format!("subagent-{i}"));
            // Seed each with a prompt line as the production
            // subagent_chat_rx spawn arm does.
            let _ = renderer.write_line_to_chat(
                idx,
                &format!("<you> subagent task {i}"),
                theme::user(),
            );
            idx
        })
        .collect();

    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    // Fire 7 parallel ToolCalls.
    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.rs")}),
        );
    }

    // Fire ToolResults in scrambled completion order, with subagent
    // events interleaved (a Complete event firing on each subagent
    // chat between parent results). Each result's body matches what
    // `read.rs` would produce for a small file — line-numbered.
    let result_order = [3, 0, 6, 1, 5, 2, 4];
    for (step, &i) in result_order.iter().enumerate() {
        let id = format!("call-{i}");
        // Realistic line-numbered body from read.rs's format!
        // pattern (`{:>width$}: {}`).
        let body = format!(
            "1: // file {i}\n2: fn main() {{\n3:     println!(\"hello from {i}\");\n4: }}\n5: "
        );
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");

        // Simulate a subagent_chat_rx event firing between parent
        // results — rotate through the 4 subagent slots.
        let sub_idx = subagent_idxs[step % subagent_idxs.len()];
        let _ = ctx.renderer.write_line_to_chat(
            sub_idx,
            &format!("<dirge> subagent step {step}"),
            c_agent(),
        );
    }

    let _ = ctx;
    let chambers = collect_chambers(&renderer);

    // Every one of the 7 parent reads must have a body row.
    assert_all_chambers_have_body(&chambers, 7);

    // And the body rows must actually contain the file content (not
    // just any text) — verify the first body row of each chamber
    // starts with `│ 1:` (the line-1 prefix from read.rs).
    for (i, c) in chambers.iter().enumerate() {
        let first = c.first_body.as_deref().unwrap_or("");
        assert!(
            first.contains("1:") || first.contains("//") || first.starts_with('\u{2502}'),
            "chamber {i} ({}) first body row doesn't look like read output: {first:?}",
            c.name
        );
    }
}

// Bring c_agent into scope for the test above.
use crate::ui::colors::c_agent;

/// Real `read`-tool output flattened through `sanitize_output` →
/// `chamber_row` → `into_text`. Simulates a realistic 7-line file
/// being rendered as a chamber body.
#[test]
fn realistic_read_body_lines_parse_one_each() {
    let body = "\
1: use std::fs;
2:
3: fn main() {
4:     let s = fs::read_to_string(\"x\").unwrap();
5:     println!(\"{}\", s);
6: }
7: ";
    let sanitized = sanitize_output(body).into_string();
    let lines: Vec<&str> = sanitized.lines().collect();
    assert_eq!(
        lines.len(),
        7,
        "expected 7 lines after sanitize_output, got {}: {:?}",
        lines.len(),
        lines
    );
    for line in &lines {
        let row = crate::ui::box_render::row(crate::ui::box_render::BoxStyle::Rounded, line, 80);
        let parsed = row.as_str().into_text().expect("parse");
        assert_eq!(
            parsed.lines.len(),
            1,
            "row {row:?} parsed to {} lines",
            parsed.lines.len()
        );
    }
}
