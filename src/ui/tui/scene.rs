//! `Scene` — the pure rendering input.
//!
//! Captures every piece of state needed to paint one UI frame.
//! `render_frame(&Scene, &mut Frame)` is the single integration
//! point: the runtime wraps a `ratatui::Terminal` and calls it on
//! every redraw; tests build a `Scene` directly and assert against
//! a `TestBackend`.
//!
//! Keeping rendering pure (state → buffer, no I/O) means we can
//! test the entire UI without a terminal, and the runtime hook
//! becomes trivial: collect state, build Scene, draw.

use ratatui::Frame;
use ratatui::style::{Color as RColor, Style};

use super::bottom::{AvatarSpec, BottomBody, BottomStrip};
use super::chat::{ChatPane, crossterm_to_ratatui};
use super::frame::{ChatBotFrame, TopFrame};
use super::layout::Layout;
use super::panels::{LeftPanel, RightPanel};
use crate::ui::renderer::{
    LeftPanelInfo, LineEntry, PanelData, PanelMode, SelectionRange, SubagentStatusRow,
};

#[allow(unused_imports)] // RColor stays in scope for the doctest example.
use ratatui::style::Color as _RColor;

/// All state needed to render one frame.
///
/// Borrowed references throughout so callers don't have to clone
/// the chat buffer or panel data on the redraw hot path.
pub struct Scene<'a> {
    /// Chat scrollback.
    pub chat_buffer: &'a [LineEntry],
    /// Rows to skip from the END of the buffer (0 = show newest).
    pub scroll_offset: usize,
    /// Number of input editor rows (clamped to MAX_INPUT_ROWS by Layout).
    pub input_rows: u16,
    /// Active selection range, if any. Lines inside this range render
    /// with REVERSED modifier so the user sees what they've highlighted.
    pub chat_selection: Option<SelectionRange>,
    /// Right panel data (MCP, LSP, TODOS, MODIFIED, sysload).
    pub panel_data: &'a PanelData,
    /// Debug panel data. When `right_panel_mode` is `Debug` and this is
    /// `Some`, the right panel shows debug info instead of system info.
    #[cfg(feature = "dap")]
    pub debug_panel_data: Option<&'a crate::dap::types::DebugPanelData>,
    /// Current right-panel mode — determines whether to show the debug panel
    /// or the normal system-info panel on the right side.
    pub right_panel_mode: PanelMode,
    /// dirge-b11: how many entries to skip from the *top* of the
    /// MODIFIED list (most-recent-first). Carried in Scene so the
    /// renderer can paint the scrolled view; persisted across
    /// redraws by `Renderer`. 0 means "show the most recent
    /// entries"; clamped at render time so it can't strand past
    /// the end of the list.
    pub modified_offset: usize,
    /// Left panel: idle card info (used when subagents is empty).
    pub left_info: &'a LeftPanelInfo,
    /// Left panel: subagent status rows (used when non-empty).
    pub subagents: &'a [SubagentStatusRow],
    /// Avatar face spec.
    pub avatar: Option<AvatarSpec<'a>>,
    /// Bottom strip body — editor input or overlay.
    pub body: BottomBody<'a>,
    /// Status row text.
    pub status: &'a str,
    /// Render the left side panel? (false when hidden via `/display`,
    /// `/panel off`, or on a too-narrow terminal.)
    pub show_left_panel: bool,
    /// Render the right side panel? (independent of the left.)
    pub show_right_panel: bool,
    /// Header / frame color.
    pub frame_color: crossterm::style::Color,
    /// Terminal background fill (theme-configurable). `Color::Reset` = no fill
    /// (keep the terminal's own background).
    pub background: crossterm::style::Color,
    /// Active picker overlay (file completion / rewind list), painted over the
    /// bottom rows of the chat region just above the input box. `None` when no
    /// picker is open [dirge-92em].
    pub picker: Option<&'a crate::ui::picker::PickerOverlay>,
    /// Brief tooltip text shown in the chat area (e.g. "Copied!").
    /// Empty string means no tooltip.
    pub tooltip: &'a str,
}

/// Paint the entire UI into `f`. Computes layout from the frame's
/// area + the scene's input_rows.
pub fn render_frame(scene: &Scene, f: &mut Frame<'_>) {
    let area = f.area();
    let layout = Layout::with_panels(
        area.width,
        area.height,
        scene.input_rows,
        scene.show_left_panel,
        scene.show_right_panel,
    );
    let frame_style = Style::default().fg(crossterm_to_ratatui(scene.frame_color));

    // Top frame (full width, across left panel + chat + right panel).
    f.render_widget(TopFrame::new(&layout).style(frame_style), area);

    // Left panel — idle card or subagent list. Skip on narrow terminals.
    if scene.show_left_panel && layout.left_panel.width >= 12 {
        f.render_widget(
            LeftPanel::new(scene.left_info, scene.subagents).border_style(frame_style),
            layout.left_panel,
        );
    }

    // Chat region (content + │ verticals).
    let mut chat = ChatPane::new(&layout, scene.chat_buffer, scene.scroll_offset)
        .border_style(frame_style)
        .tooltip(scene.tooltip);
    if let Some(sel) = scene.chat_selection {
        chat = chat.selection(sel);
    }
    f.render_widget(chat, area);

    // Right panel — stacked sub-panels. Skip on narrow terminals.
    if scene.show_right_panel && layout.right_panel.width >= 16 {
        #[allow(unused_variables)]
        let is_debug = scene.right_panel_mode == PanelMode::Debug;
        #[cfg(feature = "dap")]
        if is_debug {
            if let Some(dbg_data) = scene.debug_panel_data {
                use super::panels::debug::DebugRightPanel;
                f.render_widget(DebugRightPanel::new(dbg_data), layout.right_panel);
            }
        }
        // Render normal right panel only when NOT in debug mode (or when
        // dap feature is off).
        #[cfg(feature = "dap")]
        if !is_debug || scene.debug_panel_data.is_none() {
            f.render_widget(
                RightPanel::new(scene.panel_data).modified_offset(scene.modified_offset),
                layout.right_panel,
            );
        }
        #[cfg(not(feature = "dap"))]
        f.render_widget(
            RightPanel::new(scene.panel_data)
                .border_style(frame_style)
                .modified_offset(scene.modified_offset),
            layout.right_panel,
        );
    }

    // Chat bottom frame (╰───╯ in chat band only).
    f.render_widget(ChatBotFrame::new(&layout).style(frame_style), area);

    // Bottom strip (avatar + input box / overlay + status).
    let mut strip = BottomStrip::new(&layout)
        .status(scene.status)
        .border_style(frame_style)
        .body(scene.body);
    if let Some(avatar) = &scene.avatar {
        strip = strip.avatar(AvatarSpec {
            face: avatar.face,
            color: avatar.color,
        });
    }
    f.render_widget(strip, area);

    // Picker overlay (file completion / rewind list) — painted over the bottom
    // rows of the chat content area, just above the input box. Rendered here
    // (after the chat + strip, before the bg fill) so it overlays chat content
    // and still inherits the theme background fill below [dirge-92em].
    if let Some(picker) = scene.picker {
        paint_picker_overlay(f, &layout, picker);
    }

    // Theme background fill. Every widget above sets foreground only (selection
    // uses the REVERSED modifier, never an explicit bg), so patching the whole
    // area with `bg` sets each cell's background while leaving foregrounds — and
    // REVERSED swaps — intact. `Color::Reset` (plain theme / opt-out) skips the
    // fill so the terminal's own background shows through.
    if scene.background != crossterm::style::Color::Reset {
        f.buffer_mut().set_style(
            area,
            Style::default().bg(crossterm_to_ratatui(scene.background)),
        );
    }

    // Show the hardware cursor at the editor's (row, col). The
    // terminal blinks it naturally.
    if let BottomBody::Editor {
        cursor_row,
        cursor_col,
        is_running,
        ..
    } = scene.body
    {
        // Must match `paint_editor_box`'s prompt zone (2 cells idle,
        // 3 while running) so the cursor sits on the painted text.
        let prompt_w = super::bottom::input_prompt_width(is_running);
        let cursor_x = layout
            .input_box
            .x
            .saturating_add(1) // skip the │ border
            .saturating_add(prompt_w)
            .saturating_add(cursor_col);
        let cursor_y = layout
            .input_box
            .y
            .saturating_add(1)
            .saturating_add(cursor_row);
        let cursor_x_max = layout
            .input_box
            .x
            .saturating_add(layout.input_box.width)
            .saturating_sub(2);
        let cursor_y_max = layout
            .input_box
            .y
            .saturating_add(layout.input_box.height)
            .saturating_sub(2);
        f.set_cursor_position((cursor_x.min(cursor_x_max), cursor_y.min(cursor_y_max)));
    }
}

/// Paint a picker's candidate list over the bottom rows of the chat content
/// area (anchored just above the input box). Each painted row is space-padded
/// to the full chat width so it fully covers the chat text underneath; the
/// theme background fill in `render_frame` then colors every cell. The
/// highlighted row uses the accent color with a `▸` marker, others use dim.
fn paint_picker_overlay(
    f: &mut Frame<'_>,
    layout: &Layout,
    picker: &crate::ui::picker::PickerOverlay,
) {
    let chat = layout.chat;
    if chat.width == 0 || chat.height == 0 {
        return;
    }
    let accent = Style::default().fg(crossterm_to_ratatui(crate::ui::theme::accent()));
    let dim = Style::default().fg(crossterm_to_ratatui(crate::ui::theme::dim()));
    let width = chat.width as usize;

    // Build the (text, style) lines bottom-up: candidate rows, then an optional
    // title header above them.
    let mut lines: Vec<(String, Style)> = Vec::new();

    if picker.rows.is_empty() {
        let hint = picker.empty_hint.as_deref().unwrap_or("");
        if !hint.is_empty() {
            lines.push((hint.to_string(), dim));
        }
    } else {
        // Reserve a row for the title (if any) within the height budget.
        let title_rows = usize::from(picker.title.is_some());
        let max_list = (chat.height as usize).min(10).saturating_sub(title_rows);
        if max_list > 0 {
            let n = max_list.min(picker.rows.len());
            // Window the visible slice so `selected` stays in view.
            let start = picker
                .selected
                .saturating_sub(n / 2)
                .min(picker.rows.len().saturating_sub(n));
            for i in start..(start + n) {
                let marker = if i == picker.selected { "▸ " } else { "  " };
                let style = if i == picker.selected { accent } else { dim };
                lines.push((format!("{marker}{}", picker.rows[i]), style));
            }
        }
        if let Some(title) = &picker.title {
            lines.push((format!("  {title}"), accent));
        }
    }

    if lines.is_empty() {
        return;
    }
    // Anchor the block at the bottom of the chat area; `lines` is bottom-up.
    let block_h = (lines.len() as u16).min(chat.height);
    let buf = f.buffer_mut();
    for (offset, (text, style)) in lines.iter().take(block_h as usize).enumerate() {
        let y = chat.bottom().saturating_sub(1 + offset as u16);
        // Truncate to width, then pad with spaces so the whole row is overwritten.
        let mut padded: String = text.chars().take(width).collect();
        let cells = padded.chars().count();
        if cells < width {
            padded.push_str(&" ".repeat(width - cells));
        }
        buf.set_stringn(chat.x, y, &padded, width, *style);
    }
}

// `BottomBody` is Copy so `render_frame` can pass it to BottomStrip
// directly without a clone helper.

/// Single empty editor row, used as the default `rows` slice when
/// no input has been typed yet.
#[allow(dead_code)]
pub const EMPTY_ROWS: &[String] = &[];

/// Convenience builder for a Scene with sensible defaults — useful
/// in tests and in early-startup paths where most state is empty.
#[allow(dead_code)]
pub fn empty_scene<'a>(
    chat_buffer: &'a [LineEntry],
    panel_data: &'a PanelData,
    left_info: &'a LeftPanelInfo,
    subagents: &'a [SubagentStatusRow],
    status: &'a str,
) -> Scene<'a> {
    Scene {
        chat_buffer,
        scroll_offset: 0,
        input_rows: 1,
        chat_selection: None,
        panel_data,
        #[cfg(feature = "dap")]
        debug_panel_data: None,
        right_panel_mode: PanelMode::Auto,
        modified_offset: 0,
        left_info,
        subagents,
        avatar: None,
        body: BottomBody::Editor {
            rows: EMPTY_ROWS,
            cursor_row: 0,
            cursor_col: 0,
            is_running: false,
            completion_preview: "",
            ghost: "",
        },
        status,
        show_left_panel: true,
        show_right_panel: true,
        frame_color: crossterm::style::Color::Green,
        background: crossterm::style::Color::Reset,
        picker: None,
        tooltip: "",
    }
}

// Keep RColor in scope so the example-style doctest in this module
// doesn't have to re-import it.
const _: RColor = RColor::Green;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// End-to-end render: empty buffer, no overlay, defaults.
    /// Verifies the top frame title shows up and the chat band
    /// renders │ borders.
    #[test]
    fn renders_empty_scene_with_frames_and_borders() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let scene = empty_scene(&buf, &pd, &info, &subs, "ready");

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();

        // Top frame row contains all three titles.
        let row0: String = (0..160)
            .map(|x| backend.buffer().cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(row0.contains("[AGENT STATUS]"));
        assert!(row0.contains("[AGENT LOG STREAM]"));
        assert!(row0.contains("[SYSTEM]"));

        // Chat │ verticals on row 1.
        let layout = Layout::new(160, 30, 1);
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_left_col, 1))
                .unwrap()
                .symbol(),
            "│"
        );
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_right_col, 1))
                .unwrap()
                .symbol(),
            "│"
        );

        // Status row contains the status text.
        let status_row: String = (0..160)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, layout.status.y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(status_row.starts_with("ready"));
    }

    /// A configured (non-Reset) theme background fills every cell's bg;
    /// `Color::Reset` leaves the terminal default untouched.
    #[test]
    fn theme_background_fills_cells_only_when_set() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let mut scene = empty_scene(&buf, &pd, &info, &subs, "ready");

        let bg = crossterm::style::Color::Rgb {
            r: 0x22,
            g: 0x22,
            b: 0x22,
        };
        scene.background = bg;
        let mut t = Terminal::new(TestBackend::new(160, 30)).unwrap();
        t.draw(|f| render_frame(&scene, f)).unwrap();
        assert_eq!(
            t.backend().buffer().cell((5, 5)).unwrap().bg,
            crossterm_to_ratatui(bg),
            "configured background must fill cells",
        );

        scene.background = crossterm::style::Color::Reset;
        let mut t2 = Terminal::new(TestBackend::new(160, 30)).unwrap();
        t2.draw(|f| render_frame(&scene, f)).unwrap();
        assert_eq!(
            t2.backend().buffer().cell((5, 5)).unwrap().bg,
            RColor::Reset,
            "Reset background must NOT fill — terminal default shows through",
        );
    }

    /// When an overlay is active, the editor is REPLACED inside
    /// the bottom frame — no second box anywhere.
    #[test]
    fn overlay_replaces_input_editor() {
        use crossterm::style::Color as CC;
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let overlay_lines: Vec<(String, CC)> = vec![
            ("⚠ PERMISSION REQUIRED".into(), CC::Yellow),
            ("tool: read_file".into(), CC::Yellow),
        ];
        let scene = Scene {
            chat_buffer: &buf,
            scroll_offset: 0,
            input_rows: 4,
            chat_selection: None,
            panel_data: &pd,
            #[cfg(feature = "dap")]
            debug_panel_data: None,
            right_panel_mode: PanelMode::Auto,
            modified_offset: 0,
            left_info: &info,
            subagents: &subs,
            avatar: None,
            body: BottomBody::Overlay {
                title: "[ALERT]",
                lines: &overlay_lines,
            },
            status: "permission required",
            show_left_panel: true,
            show_right_panel: true,
            frame_color: crossterm::style::Color::Green,
            background: crossterm::style::Color::Reset,
            picker: None,
            tooltip: "",
        };

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(160, 30, 4);

        // The input box top border should have "[ALERT]" centered.
        let top_y = layout.input_box.y;
        let top_row: String = (layout.input_box.x..layout.input_box.x + layout.input_box.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, top_y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(top_row.contains("[ALERT]"), "got top {:?}", top_row);

        // First overlay line ("⚠ PERMISSION REQUIRED") shows in row 1.
        let body_row: String = (layout.input_box.x..layout.input_box.x + layout.input_box.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, top_y + 1))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(
            body_row.contains("PERMISSION REQUIRED"),
            "got body {:?}",
            body_row
        );
    }

    /// Read the whole TestBackend buffer into one newline-joined string.
    fn dump(backend: &TestBackend, w: u16, h: u16) -> String {
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(backend.buffer().cell((x, y)).unwrap().symbol());
            }
            s.push('\n');
        }
        s
    }

    /// dirge-92em: an active picker paints its candidate list (with the `▸`
    /// marker on the selected row) through the scene, so it actually reaches
    /// the screen instead of the redirected stdout fd.
    #[test]
    fn picker_overlay_paints_candidate_list() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let overlay = crate::ui::picker::PickerOverlay {
            title: None,
            rows: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "Cargo.toml".into(),
            ],
            selected: 1,
            empty_hint: Some("no matches".into()),
        };
        let mut scene = empty_scene(&buf, &pd, &info, &subs, "ready");
        scene.picker = Some(&overlay);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        let dumped = dump(terminal.backend(), 120, 30);

        assert!(dumped.contains("src/main.rs"), "non-selected row missing");
        assert!(dumped.contains("Cargo.toml"), "non-selected row missing");
        assert!(
            dumped.contains("▸ src/lib.rs"),
            "selected row + marker missing"
        );
    }

    /// An empty match set surfaces the "no matches" hint.
    #[test]
    fn picker_overlay_empty_shows_hint() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let overlay = crate::ui::picker::PickerOverlay {
            title: None,
            rows: vec![],
            selected: 0,
            empty_hint: Some("no matches".into()),
        };
        let mut scene = empty_scene(&buf, &pd, &info, &subs, "ready");
        scene.picker = Some(&overlay);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        assert!(dump(terminal.backend(), 120, 30).contains("no matches"));
    }

    /// Side panel suppression: a narrow terminal (line_w ≤
    /// CHAT_CONTENT_MAX_W) has zero-width left/right panels, so
    /// LeftPanel / RightPanel widgets shouldn't paint into them.
    /// The top frame still draws — just without the side titles.
    #[test]
    fn narrow_terminal_skips_side_panels() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let mut scene = empty_scene(&buf, &pd, &info, &subs, "narrow");
        // request side panels even though they collapse on a narrow term
        scene.show_left_panel = true;
        scene.show_right_panel = true;

        let mut backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(60, 20, 1);

        // Side panels have zero width — no DIRGE banner anywhere.
        assert_eq!(layout.left_panel.width, 0);
        assert_eq!(layout.right_panel.width, 0);
        let mut found_dirge = false;
        for y in 0..20 {
            let r: String = (0..60)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect();
            if r.contains("D I R G E") {
                found_dirge = true;
                break;
            }
        }
        assert!(
            !found_dirge,
            "DIRGE banner should not appear on narrow term"
        );
    }

    /// `/display` granularity: the left and right panels toggle
    /// independently, and a hidden panel's gutter is reclaimed by the
    /// chat band (not left blank). With only the left shown the left
    /// gutter draws and the chat expands rightward to the edge; with
    /// only the right shown the chat expands leftward.
    #[test]
    fn left_and_right_panels_toggle_independently() {
        fn region_has_content(backend: &TestBackend, r: ratatui::layout::Rect) -> bool {
            for y in r.y..r.y.saturating_add(r.height) {
                for x in r.x..r.x.saturating_add(r.width) {
                    if let Some(cell) = backend.buffer().cell((x, y))
                        && cell.symbol().trim() != ""
                    {
                        return true;
                    }
                }
            }
            false
        }

        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let both = Layout::new(160, 30, 1);
        assert!(both.left_panel.width >= 12 && both.right_panel.width >= 16);

        let render = |show_left: bool, show_right: bool| {
            let mut scene = empty_scene(&buf, &pd, &info, &subs, "ready");
            scene.show_left_panel = show_left;
            scene.show_right_panel = show_right;
            let backend = TestBackend::new(160, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| render_frame(&scene, f)).unwrap();
            terminal.backend().clone()
        };

        // Left only: left gutter draws; the chat reclaims the right
        // gutter so it is wider than the both-visible chat.
        let left_only = Layout::with_panels(160, 30, 1, true, false);
        assert_eq!(left_only.right_panel.width, 0);
        assert_eq!(
            left_only.chat.width,
            both.chat.width + both.right_panel.width
        );
        let b = render(true, false);
        assert!(
            region_has_content(&b, left_only.left_panel),
            "left should draw"
        );

        // Right only: right gutter draws; the chat reclaims the left
        // gutter and runs flush to the left edge.
        let right_only = Layout::with_panels(160, 30, 1, false, true);
        assert_eq!(right_only.left_panel.width, 0);
        assert_eq!(
            right_only.chat.width,
            both.chat.width + both.left_panel.width
        );
        let b = render(false, true);
        assert!(
            region_has_content(&b, right_only.right_panel),
            "right should draw"
        );
    }

    /// Typing into the input field: render the same Scene twice
    /// with different editor text and assert the input box content
    /// updates on the second draw. This is the smoke test for the
    /// "typing doesn't work" bug — if it passes, the widget +
    /// scene path is correct and any runtime regression must be in
    /// the integration layer (event loop, draw_bottom caching).
    #[test]
    fn editor_text_updates_between_draws() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();

        // First draw: empty input.
        let s1 = Scene {
            chat_buffer: &buf,
            scroll_offset: 0,
            input_rows: 1,
            chat_selection: None,
            panel_data: &pd,
            #[cfg(feature = "dap")]
            debug_panel_data: None,
            right_panel_mode: PanelMode::Auto,
            modified_offset: 0,
            left_info: &info,
            subagents: &subs,
            avatar: None,
            body: BottomBody::Editor {
                rows: EMPTY_ROWS,
                cursor_row: 0,
                cursor_col: 0,
                is_running: false,
                completion_preview: "",
                ghost: "",
            },
            status: "",
            show_left_panel: true,
            show_right_panel: true,
            frame_color: crossterm::style::Color::Green,
            background: crossterm::style::Color::Reset,
            picker: None,
            tooltip: "",
        };
        terminal.draw(|f| render_frame(&s1, f)).unwrap();

        // Second draw: "hello" typed.
        let hello_rows: Vec<String> = vec!["hello".to_string()];
        let s2 = Scene {
            chat_buffer: &buf,
            scroll_offset: 0,
            input_rows: 1,
            chat_selection: None,
            panel_data: &pd,
            #[cfg(feature = "dap")]
            debug_panel_data: None,
            right_panel_mode: PanelMode::Auto,
            modified_offset: 0,
            left_info: &info,
            subagents: &subs,
            avatar: None,
            body: BottomBody::Editor {
                rows: &hello_rows,
                cursor_row: 0,
                cursor_col: 5,
                is_running: false,
                completion_preview: "",
                ghost: "",
            },
            status: "",
            show_left_panel: true,
            show_right_panel: true,
            frame_color: crossterm::style::Color::Green,
            background: crossterm::style::Color::Reset,
            picker: None,
            tooltip: "",
        };
        terminal.draw(|f| render_frame(&s2, f)).unwrap();
        backend = terminal.backend().clone();

        // Locate the input box's first inner row and assert "hello"
        // is present somewhere on it.
        let layout = Layout::new(160, 30, 1);
        let inner_y = layout.input_box.y + 1;
        let row: String = (layout.input_box.x..layout.input_box.x + layout.input_box.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, inner_y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(
            row.contains("hello"),
            "input row should contain typed text; got {row:?}"
        );
    }

    /// Chat content from the scene's buffer paints into the chat
    /// region with the expected text in the expected rows.
    #[test]
    fn chat_buffer_paints_into_chat_region() {
        let buf: Vec<LineEntry> = vec![
            LineEntry {
                text: "first line".into(),
                color: crossterm::style::Color::Green,
            },
            LineEntry {
                text: "second line".into(),
                color: crossterm::style::Color::Cyan,
            },
        ];
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let scene = empty_scene(&buf, &pd, &info, &subs, "");

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(160, 30, 1);

        // Lines paint top-anchored at chat.y, chat.y + 1.
        let read = |y: u16| -> String {
            (layout.chat.x..layout.chat.x + layout.chat.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert!(read(layout.chat.y).starts_with("first line"));
        assert!(read(layout.chat.y + 1).starts_with("second line"));
    }
}
