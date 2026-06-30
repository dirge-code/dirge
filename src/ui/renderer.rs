use std::io::{self, Write};

use compact_str::CompactString;
use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Color;
use crossterm::terminal::{Clear, ClearType};
// `MoveTo` / `Clear` / `ExecutableCommand` are still used by
// `clear_content` (resets the alt screen on `/clear`). The
// streaming + viewport paint no longer touches stdout directly —
// that's all routed through `tui_redraw` (ratatui).

/// Output sink for ratatui's CrosstermBackend. Prefers a fresh
/// `/dev/tty` handle (so painting is isolated from the process's
/// fd 1 — see `TerminalGuard`'s fd redirection); falls back to
/// stdout when there's no controlling terminal (CI tests).
pub enum BackendWriter {
    // In test builds the constructor is stubbed (cfg(test) at the
    // factory below returns None), so the variants are never
    // constructed — but the `impl Write` arms still need them.
    #[cfg_attr(test, allow(dead_code))]
    Tty(std::fs::File),
    #[cfg_attr(test, allow(dead_code))]
    Stdout(std::io::Stdout),
}

impl std::io::Write for BackendWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            BackendWriter::Tty(f) => f.write(b),
            BackendWriter::Stdout(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            BackendWriter::Tty(f) => f.flush(),
            BackendWriter::Stdout(s) => s.flush(),
        }
    }
}

/// Build the ratatui terminal and report whether its backend writer is a real
/// terminal (so synchronized-update brackets are worth emitting). `true` for a
/// `/dev/tty` handle; for the stdout fallback, follows `IsTerminal(stdout)`.
fn build_tui_terminal()
-> Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<BackendWriter>>> {
    // Never open /dev/tty or stdout for painting during tests.
    // cargo test captures stdout but /dev/tty still points at the
    // real terminal.  Multiple test threads calling tui_redraw
    // (via write_line / scroll_to_bottom / render_viewport) would
    // interleave ratatui escape sequences directly onto the user's
    // screen, corrupting the terminal and triggering spurious
    // behaviours (form-feed print dialogs, colour leaks, cursor
    // jumps).  Returning None makes tui_redraw a no-op.
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let writer = match crate::ui::terminal::open_tty_for_write() {
            Some(f) => BackendWriter::Tty(f),
            None => BackendWriter::Stdout(std::io::stdout()),
        };
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(writer)).ok()
    }
}

#[derive(Clone)]
pub struct LineEntry {
    pub text: CompactString,
    pub color: Color,
}

/// dirge-qy3y: width-independent source for one committed chat region.
/// The scrollback `buffer` is a *derived cache* of wrapped rows; on a
/// terminal resize it is regenerated from these blocks at the new width
/// (`Renderer::rebuild`) so markdown — tables especially — reflows instead
/// of keeping the column widths it was first rendered at.
#[derive(Clone)]
enum SourceBlock {
    /// Plain text (may contain `\n`). Re-rendered by splitting on `\n` and
    /// soft-wrapping each segment at the current content width — an empty
    /// segment renders as a blank row, matching `write_line`.
    Plain { text: String, color: Color },
    /// A markdown region re-rendered with `markdown_to_styled`. `handle`
    /// prepends the `<dirge> ` agent handle to the first row (the streamed
    /// reasoning/response register).
    Markdown {
        src: String,
        base_color: Color,
        handle: bool,
    },
    /// Pre-rendered rows that do NOT reflow — transient/modal content placed
    /// via `replace_from` (input editor, pickers, collapsed chambers). Stored
    /// verbatim so `source` stays a faithful mirror of `buffer` and `rebuild`
    /// reproduces it exactly; it just doesn't re-wrap (it's re-rendered by its
    /// own owner each interaction anyway).
    Raw { rows: Vec<LineEntry> },
}

/// A source block plus its cached rendered-row count at the width the `buffer`
/// currently reflects. The count lets `replace_from` / `enforce_cap` map a
/// buffer offset to a block boundary WITHOUT re-rendering (markdown re-parse)
/// every block — `replace_from` runs per keystroke in modal sub-loops, so an
/// O(scrollback) re-render there would lag typing. Kept in sync on every
/// append, on `stream`, and recomputed wholesale in `rebuild`.
#[derive(Clone)]
struct Block {
    src: SourceBlock,
    rows: usize,
}

/// Cap on how many logical input lines we'll show stacked at the bottom of
/// the screen before the input box starts internally scrolling. Beyond this
/// the chat-history viewport would be unreasonably squashed.
pub const MAX_INPUT_VISIBLE_LINES: usize = 8;

/// ui-redesign: the bottom [ALERT] panel wraps the input area in a
/// double-line frame. Two reserved rows = top border (with title)
/// plus bottom border. Side borders (│ ... │) are painted on every
/// input row so the entire input area reads as one framed card,
/// matching the mockup's bottom strip.
///
/// The frame title is `[ALERT]` permanently — input text and
/// permission prompts both live INSIDE the frame.
pub const ALERT_FRAME_ROWS: u16 = 2;

/// ui-redesign: chat area is wrapped in a heavy double-line frame
/// titled `[AGENT LOG STREAM]`. Two reserved rows = top border
/// (row 0) + bottom border (row 1 + visible_lines). Side borders
/// (│ … │) are painted at the chat-band edges on every visible
/// chat row when there's room (content_indent >= 1).
pub const CHAT_FRAME_ROWS: u16 = 2;

/// Minimum terminal width at which `PanelMode::Auto` decides to show
/// the side panels. Below this the chat is too narrow to spare any
/// margin for the AGENT STATUS / SYSTEM gutters.
///
/// dirge-8855: this is the REAL threshold, derived from the gutter math.
/// A side panel needs ≥15 cols of centered-layout margin
/// (`content_indent() >= 15`); since `content_width` caps at 120, a
/// non-trivial gutter only appears once `line_width (= cols - 2)` exceeds
/// 120, and `content_indent >= 15` ⇒ `line_width - 120 >= 30` ⇒
/// `cols >= 152`. The old value of 100 was dead — the `content_indent`
/// gate always bound first — and the README's "≥100 cols" was wrong.
const PANEL_AUTO_MIN_COLS: u16 = 152;

/// Max rows the live shell box (`!cmd`/`!!cmd`) may occupy, so a chatty
/// command never crowds out the whole chat. The painter clips the tail.
const SHELL_BOX_MAX_ROWS: u16 = 12;

/// Global terminal modes dirge owns and must keep asserted for its whole
/// session: SGR mouse capture (`?1000`/`?1002`/`?1003`/`?1006`) so wheel +
/// click reach the app, and bracketed paste (`?2004`). These are set once
/// at startup ([`crate::ui::terminal::TerminalGuard::new`]); this is the
/// exact same set, re-emitted periodically so a mid-session reset can't
/// leave them off permanently. Both are idempotent with no visual effect
/// when already enabled, so re-emitting on a throttle is safe. Notably this
/// does NOT include the alternate screen (`?1049h`) — re-entering it can
/// clear/flicker on some terminals — nor cursor visibility (managed per
/// frame by `draw_bottom`).
const TERMINAL_MODE_REASSERT: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h";

/// How often [`tui_redraw`](Renderer::tui_redraw) re-asserts the terminal
/// modes. Long enough that the extra `/dev/tty` write is negligible, short
/// enough that a leaked reset self-heals before it's annoying.
const MODE_REASSERT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Decide whether the terminal modes are due for re-assertion, returning
/// the bytes to emit (or `None` when the throttle hasn't elapsed). Pure so
/// the throttle + payload are unit-testable without a live terminal: a
/// `None` `last` (first paint) always re-asserts; otherwise it waits
/// [`MODE_REASSERT_INTERVAL`]. `saturating_duration_since` guards against a
/// non-monotonic clock.
fn mode_reassert_payload(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<&'static [u8]> {
    let due = match last {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= MODE_REASSERT_INTERVAL,
    };
    if due {
        Some(TERMINAL_MODE_REASSERT)
    } else {
        None
    }
}

#[cfg(feature = "experimental-ui-terminal-tab")]
fn format_terminal_title(state: crate::ui::avatar::AvatarState, tool_name: Option<&str>) -> String {
    use crate::ui::avatar::AvatarState;
    // PR #144 follow-up: strip control bytes from caller-supplied
    // tool names. Today the names come from the internal tool
    // registry (`bash`, `edit`, …) so this is purely defensive,
    // but a plugin or MCP server is one register-call away from
    // smuggling `\x07` (BEL) or `\x1b` (ESC) into a name — which
    // would prematurely close the OSC or inject further escape
    // sequences when concatenated below. Newlines also break the
    // title display.
    let sanitize = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control() && *c != '\u{0007}' && *c != '\u{001b}' && *c != '\u{009c}')
            .take(64)
            .collect()
    };
    match state {
        AvatarState::Idle | AvatarState::Done => "● dirge".to_string(),
        AvatarState::Thinking => "● dirge: thinking".to_string(),
        AvatarState::Speaking => "● dirge: responding".to_string(),
        AvatarState::Reading | AvatarState::Writing | AvatarState::Bash => {
            if let Some(name) = tool_name {
                let clean = sanitize(name);
                if clean.is_empty() {
                    "◌ dirge: working".to_string()
                } else {
                    format!("◌ dirge: {}", clean)
                }
            } else {
                "◌ dirge: working".to_string()
            }
        }
        AvatarState::Alert => "✗ dirge: needs input".to_string(),
        AvatarState::Error => "✗ dirge: ERROR".to_string(),
    }
}

/// Build the OSC-0 byte sequence to set the terminal title. PR #144
/// follow-up: switch to ST (`\x1b\\`) terminator, which is the
/// RFC 1605 / xterm-canonical form and passes through tmux without
/// needing `set-option -g allow-passthrough on`. BEL works on most
/// terminals but tmux specifically prefers ST.
#[cfg(feature = "experimental-ui-terminal-tab")]
fn osc_set_title(title: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(title.len() + 5);
    out.extend_from_slice(b"\x1b]0;");
    out.extend_from_slice(title.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Emit an empty OSC-0 to release the terminal title back to the
/// shell's default. The TUI shutdown path in `terminal.rs` inlines
/// the same bytes alongside other reset escapes for efficiency;
/// this helper exists as a single source of truth for future
/// callers (signal handlers, panic-recovery, etc.) and to anchor
/// the unit test.
#[cfg(feature = "experimental-ui-terminal-tab")]
#[allow(dead_code)]
fn osc_reset_title() -> Vec<u8> {
    b"\x1b]0;\x1b\\".to_vec()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelMode {
    /// Show panel when terminal width >= PANEL_AUTO_MIN_COLS.
    Auto,
    /// Force panel on (still hidden if terminal is absurdly narrow).
    On,
    /// Force panel off regardless of width.
    Off,
    /// Show debug panel instead of system info (gated on ≥100 cols).
    /// Only meaningful when a DAP session is active.
    Debug,
}

/// Which side panels a `/display` spec (or the `display` config value)
/// asks for. The main conversation pane is always shown — the centered
/// chat band is the layout's anchor and can't be hidden — so only the
/// left and right gutters are toggled here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneVisibility {
    pub left: bool,
    pub right: bool,
}

/// Parse a `/display` / `display` spec into the set of side panels to show.
///
/// Tokens are the pane names `left`, `main`, `right`, separated by `|`,
/// `,`, or whitespace and matched case-insensitively — e.g.
/// `left|main|right`, `main`, `main right`, `MAIN, RIGHT`. `main` is
/// accepted but has no effect on layout (the conversation always shows);
/// listing it is how a user says "only the main pane" (`/display main`).
///
/// Returns `Err` with a user-facing message on an empty spec or an
/// unrecognized token, so the caller can surface it instead of silently
/// applying a wrong layout.
pub fn parse_display_spec(spec: &str) -> Result<PaneVisibility, String> {
    let mut vis = PaneVisibility {
        left: false,
        right: false,
    };
    let mut saw_token = false;
    for tok in spec.split(['|', ',', ' ', '\t']).filter(|t| !t.is_empty()) {
        saw_token = true;
        match tok.to_ascii_lowercase().as_str() {
            "left" => vis.left = true,
            "right" => vis.right = true,
            // `main` is always shown; accept it so the user can name the
            // full layout, but it doesn't toggle anything.
            "main" => {}
            other => {
                return Err(format!(
                    "unknown pane '{other}' (use left, main, and/or right, e.g. /display left|main|right)"
                ));
            }
        }
    }
    if !saw_token {
        return Err(
            "usage: /display <panes> where panes are left|main|right (e.g. /display main|right)"
                .to_string(),
        );
    }
    Ok(vis)
}

// Re-exported from submodules so existing imports don't break.
pub use crate::ui::panel_data::{LeftPanelInfo, PanelData, SubagentStatusRow};
/// Normalized selection range — `start <= end` in row-major order.
/// Coordinates are `(buffer_line_idx, char_offset_in_line)`. Used by
/// the chat pane to apply REVERSED styling to selected cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Order two selection endpoints into row-major (start, end) so the
/// renderer never has to handle the upward-drag case mid-paint.
/// Word-character test for double-click select. Matches the input
/// editor's definition (alphanumeric + underscore) so selecting a word
/// behaves consistently across the chat buffer and the input box.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

pub fn normalize_selection_range(a: (usize, usize), b: (usize, usize)) -> SelectionRange {
    if (a.0, a.1) <= (b.0, b.1) {
        SelectionRange { start: a, end: b }
    } else {
        SelectionRange { start: b, end: a }
    }
}

/// Per-chat state saved while a chat is INACTIVE. Mirrors the fields
/// the active chat uses on the `Renderer` itself; switching chats
/// swaps state in/out via `save_active` / `load_active`. Keeps the
/// hot-path rendering code unchanged — only chat-switch boundaries
/// pay the snapshot cost.
///
/// dirge-ov2 Phase A: enables multiple subagent chat windows. The
/// main session is always at index 0; subagent chats start at index 1.
/// Selection state lives per-chat because a selection in chat A would
/// be meaningless when chat B is on screen.
pub struct ChatSnapshot {
    pub name: String,
    buffer: Vec<LineEntry>,
    /// dirge-qy3y: per-chat source-of-truth, swapped with the active fields
    /// on chat switch so each chat reflows independently.
    source: Vec<Block>,
    streaming: bool,
    open_rows: usize,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    lines: u16,
    col: u16,
    selection_active: bool,
    selection_start: Option<(usize, usize)>,
    selection_end: Option<(usize, usize)>,
}

pub struct Renderer {
    lines: u16,
    col: u16,
    spinner_tick: bool,
    /// #387: dirty flag for the single-paint-per-event model. Mutators
    /// (write_line/write/scroll/render_viewport/set_bottom) set this
    /// instead of painting inline; [`Renderer::flush`] performs the one
    /// real `tui_redraw` per event iff it is set. See [`crate::ui::state`].
    needs_paint: bool,
    /// Timestamp of the most recent successful `tui_redraw`. Used by the
    /// 8ms repaint throttle to prevent /dev/tty write contention between
    /// keystroke-driven repaints — the root cause of typing stutter.
    last_paint: Option<std::time::Instant>,
    /// Timestamp of the last terminal-mode re-assertion (SGR mouse capture +
    /// bracketed paste). Those modes are enabled once at startup, but a
    /// child program run via the bash tool (a pager / TUI) can reset them
    /// mid-session by emitting `?1000l`/`?2004l` on exit — and there's no
    /// other path that turns them back on, so the loss is permanent (wheel
    /// scroll falls through to the terminal, selection stops reaching the
    /// app). `tui_redraw` re-emits them on a throttle so dirge self-heals
    /// within one interval of any such leak. `None` until the first paint.
    last_mode_reassert: Option<std::time::Instant>,
    /// Bumped each time scrollback eviction drains lines from the FRONT of
    /// `buffer`, which shifts every absolute line index down. Consumers
    /// holding an absolute index across appends (the Ctrl+O expansion
    /// anchor) capture this and bail if it changed — a buffer-length
    /// coincidence alone can't tell whether an eviction invalidated the
    /// index.
    eviction_generation: u64,
    /// Test-only forced terminal width (see `set_test_cols`); `None` in
    /// production, where the real tty size is queried.
    #[cfg(test)]
    test_cols: Option<u16>,
    buffer: Vec<LineEntry>,
    /// dirge-qy3y: width-independent source-of-truth for `buffer`. Every
    /// committed region appends a [`SourceBlock`]; `buffer` is the wrapped
    /// render cache derived from these. `rebuild` regenerates `buffer` from
    /// `source` at the current width so scrollback reflows on resize.
    source: Vec<Block>,
    /// True while the tail of `source`/`buffer` is an open (in-flight)
    /// streamed block being re-rendered per token (`stream`). Sealed by
    /// `commit_stream`.
    streaming: bool,
    /// Rows the open streamed block currently occupies at the buffer tail
    /// (only meaningful while `streaming`).
    open_rows: usize,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    /// dirge-ov2: snapshots of the OTHER chats — the active chat's
    /// state lives in the fields above. `chats[active_chat]` is the
    /// "free slot" (its name/buffer match what's on screen but the
    /// fields haven't been written into it yet; switching chats
    /// flushes them).
    chats: Vec<ChatSnapshot>,
    active_chat: usize,
    /// Number of rows the input area currently occupies (1 by default, grows
    /// up to MAX_INPUT_VISIBLE_LINES as the user adds newlines or types past
    /// the wrap width). The chat viewport shrinks by the same amount.
    input_rows: u16,
    pub selection_active: bool,
    /// Selection anchor as `(buffer_line_index, char_offset_in_line)`.
    /// Char offset is in *chars* (not bytes) so multi-byte UTF-8 glyphs
    /// behave the same as ASCII. `(line, line_len)` is a valid past-the-
    /// end position used when dragging past the line's right edge.
    pub selection_start: Option<(usize, usize)>,
    pub selection_end: Option<(usize, usize)>,
    /// Time + cell of the last mouse-down, for double-click detection
    /// (select-word). `None` until the first click or after a
    /// double-click is consumed.
    pub last_click: Option<(std::time::Instant, u16, u16)>,
    /// Set when a double-click selected a word: the following mouse-up
    /// must NOT extend/clear that selection (it would collapse the word
    /// to the click point). Consumed on the next mouse-up.
    pub suppress_next_mouseup: bool,
    /// Timestamp of the most recent clipboard copy, for "Copied!" tooltip.
    /// `None` when no copy has happened this session.
    copied_at: Option<std::time::Instant>,
    /// Visibility mode for the left / right side panels, controlled
    /// independently (`/display`, `/panel`, and the `display` config).
    /// The main conversation pane is always shown.
    left_panel_mode: PanelMode,
    right_panel_mode: PanelMode,
    /// Most-recently set panel snapshot. The UI rebuilds and pushes this
    /// before each redraw so render_viewport/draw_bottom can repaint the
    /// panel along with the rest of the screen.
    panel_data: PanelData,
    /// dirge-gek: subagent task summary rows for the LEFT gutter
    /// panel. Each entry surfaces one in-flight or recently-finished
    /// subagent so the user can glance at activity without switching
    /// chat windows. Set by the UI loop on each lifecycle event;
    /// rendered above the bottom-row avatar in `draw_left_panel`.
    subagent_status: Vec<SubagentStatusRow>,
    /// ui-redesign: idle-state info for the left panel. Painted when
    /// `subagent_status` is empty so the gutter never looks dead.
    left_panel_info: LeftPanelInfo,
    /// DAP debug panel snapshot — updated each UI tick when a
    /// DAP session is active and panel mode is Debug.
    #[cfg(feature = "dap")]
    debug_panel_data: Option<crate::dap::types::DebugPanelData>,
    /// ui-redesign Phase 6: when set, `draw_bottom` paints these
    /// lines inside the bottom frame INSTEAD of the input editor.
    /// Used by permission prompts and questionnaire prompts so the
    /// user can see the prompt without the input box obscuring it.
    /// Cleared after the ask handler resolves. Each entry is
    /// (text, color); painter centers text horizontally within the
    /// frame's inner band.
    alert_overlay: Option<Vec<(String, Color)>>,
    /// Live PTY output for an active `!cmd`/`!!cmd` shell session, shown in the
    /// input-box frame as an overlay titled `[shell]` while the session runs.
    shell_overlay: Option<Vec<(String, Color)>>,
    /// Picker candidate-list overlay fed to the scene each frame (file
    /// completion or rewind list). Recomputed by `draw_bottom` from the input
    /// editor's file picker, falling back to `rewind_overlay`; cached so
    /// `render_viewport` (which has no editor handle) repaints it too
    /// [dirge-92em].
    picker_overlay: Option<crate::ui::picker::PickerOverlay>,
    /// Rewind-mode list-picker overlay. Set/cleared explicitly by the rewind
    /// flow (it lives outside the input editor); folded into `picker_overlay`
    /// by `draw_bottom` when no file picker is active.
    rewind_overlay: Option<crate::ui::picker::PickerOverlay>,
    /// ui-redesign: title shown in the bottom-frame's top border
    /// when the alert overlay is active. Empty when no overlay (the
    /// idle input has no title, per the mockup). Caller of
    /// `set_alert_overlay` is expected to push this via
    /// `set_alert_title` so the frame label matches the prompt
    /// type (`[ALERT]`, `[QUESTION]`, etc.).
    alert_title: String,
    /// What the agent is doing — drives the bottom-left ASCII avatar.
    avatar_state: crate::ui::avatar::AvatarState,
    /// Animation flip; toggled by `tick_avatar()` so the avatar's
    /// eyes / mouth alternate between two poses per state.
    avatar_tick: bool,

    // ── ratatui migration (Phase 6) ────────────────────────────────
    /// The ratatui Terminal driving the new paint pipeline. `Option`
    /// because tests construct Renderer without a real stdout and
    /// must skip the actual draw call (the legacy paint paths kept
    /// no terminal handle either — this preserves the same testable
    /// shape).
    tui_terminal: Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<BackendWriter>>>,
    /// Cached input editor snapshot — used when `write_line` / `write`
    /// trigger a redraw and don't have the editor reference at
    /// hand, but the last `draw_bottom` did. Stored as pre-wrapped
    /// rows (one per visual line) so the widget can render multi-
    /// line input without re-wrapping each frame.
    cached_input_rows: Vec<String>,
    /// Cursor row within `cached_input_rows`.
    cached_input_cursor_row: u16,
    /// Cursor column on `cached_input_rows[cached_input_cursor_row]`,
    /// in display cells.
    cached_input_cursor_col: u16,
    /// Status string from the most recent `draw_bottom` call.
    cached_status: String,
    /// `is_running` from the most recent `draw_bottom` call.
    cached_is_running: bool,
    /// Completion preview string — formatted list of upcoming
    /// slash commands from the most recent `draw_bottom` call.
    /// Empty when no tab-completion is active.
    cached_completion_preview: String,
    /// Inline dark-gray ghost completion for an in-progress slash command
    /// (e.g. typing `/dis` shows `play`). Empty when not applicable;
    /// accepted with the Right arrow.
    cached_input_ghost: String,
    /// Chat content rect from the most recent `tui_redraw` call.
    /// Used by `buffer_pos_at` to map mouse `(row, col)` into the
    /// chat buffer using the actual ratatui layout, not the legacy
    /// row-1-is-chat-top assumption. `None` until the first paint
    /// (selection events before the first frame are dropped, which
    /// matches "no drag is possible because there's nothing on
    /// screen yet").
    cached_chat_rect: Option<ratatui::layout::Rect>,

    /// dirge-b11: user-driven scroll offset into the MODIFIED
    /// sub-panel. 0 = show the most recent entries (default). Walked
    /// by mouse-wheel events when the cursor hovers inside the
    /// modified region (see `panel_modified_scroll`); persisted
    /// across redraws so a stream of agent events doesn't reset
    /// the view. Resets to 0 when the underlying list grows (a new
    /// modification arrives) so the user always sees the newest
    /// entry without scrolling back.
    pub(crate) modified_offset: usize,
    /// dirge-b11: previous MODIFIED list length, used to detect
    /// growth so we can reset `modified_offset` to 0 on the next
    /// `set_panel_data` call. `None` before the first push.
    last_modified_len: Option<usize>,
    /// dirge-b11: MODIFIED sub-panel rect from the most recent
    /// paint, used by the mouse-event handler to decide whether a
    /// scroll wheel tick should walk the modified list or fall
    /// through to chat scrolling. `None` until the first paint or
    /// when the panel is hidden.
    pub(crate) cached_modified_rect: Option<ratatui::layout::Rect>,

    #[cfg(feature = "experimental-ui-terminal-tab")]
    cached_terminal_title: String,
    #[cfg(feature = "experimental-ui-terminal-tab")]
    last_tool_name: Option<String>,
}

impl Renderer {
    pub fn new() -> io::Result<Self> {
        let tui_terminal = build_tui_terminal();
        Ok(Renderer {
            lines: 0,
            col: 0,
            spinner_tick: false,
            shell_overlay: None,
            needs_paint: false,
            last_paint: None,
            last_mode_reassert: None,
            eviction_generation: 0,
            #[cfg(test)]
            test_cols: None,
            buffer: Vec::new(),
            source: Vec::new(),
            streaming: false,
            open_rows: 0,
            partial: CompactString::new(""),
            partial_color: Color::White,
            scroll_offset: 0,
            // dirge-ov2: one default "main" chat. Subagent chats are
            // appended via `add_chat`. Index 0 is always the main
            // session.
            chats: vec![ChatSnapshot::empty("main")],
            active_chat: 0,
            input_rows: 1,
            selection_active: false,
            selection_start: None,
            selection_end: None,
            last_click: None,
            suppress_next_mouseup: false,
            copied_at: None,
            left_panel_mode: PanelMode::Auto,
            right_panel_mode: PanelMode::Auto,
            panel_data: PanelData::default(),
            subagent_status: Vec::new(),
            left_panel_info: LeftPanelInfo::default(),
            #[cfg(feature = "dap")]
            debug_panel_data: None,
            alert_overlay: None,
            picker_overlay: None,
            rewind_overlay: None,
            alert_title: String::new(),
            avatar_state: crate::ui::avatar::AvatarState::Idle,
            avatar_tick: false,
            // ratatui's backend writes to /dev/tty (a fresh fd
            // pointing at the controlling terminal) rather than the
            // process's stdout. With stdout/stderr redirected to
            // the log file by TerminalGuard, this is the only path
            // that can paint the screen — no rogue (print …),
            // println!, panic, or child-process output can reach
            // the TTY anymore. Falls back to stdout when /dev/tty
            // isn't available (CI tests, headless).
            tui_terminal,
            cached_input_rows: vec![String::new()],
            cached_input_cursor_row: 0,
            cached_input_cursor_col: 0,
            cached_status: String::new(),
            cached_is_running: false,
            cached_completion_preview: String::new(),
            cached_input_ghost: String::new(),
            cached_chat_rect: None,
            modified_offset: 0,
            last_modified_len: None,
            cached_modified_rect: None,

            #[cfg(feature = "experimental-ui-terminal-tab")]
            cached_terminal_title: String::new(),
            #[cfg(feature = "experimental-ui-terminal-tab")]
            last_tool_name: None,
        })
    }

    /// Record that text was just copied to clipboard, so a "Copied!"
    /// tooltip appears on the next redraw.
    pub fn notify_copied(&mut self) {
        self.copied_at = Some(std::time::Instant::now());
    }

    /// If text was copied within the last 2 seconds, returns the
    /// tooltip text to display (e.g. `"Copied!"`). Otherwise returns
    /// `None` so the tooltip disappears.
    fn current_tooltip(&self) -> Option<&'static str> {
        match self.copied_at {
            Some(t) if t.elapsed() < std::time::Duration::from_secs(2) => Some("Copied!"),
            _ => None,
        }
    }

    /// Phase 6 paint entry point. Builds a `Scene` from current
    /// Renderer state and calls `render_frame` through the ratatui
    /// Terminal. Every legacy paint method funnels here.
    ///
    /// Returns `Ok(())` (no-op) when no ratatui Terminal was
    /// initialised — keeps tests that construct `Renderer::new()`
    /// against captured stdout from blowing up on `draw`.
    pub(crate) fn tui_redraw(&mut self) -> io::Result<()> {
        use crate::ui::avatar;
        use crate::ui::tui::bottom::{AvatarSpec, BottomBody};
        use crate::ui::tui::scene::{Scene, render_frame};

        // Self-heal the global terminal modes (SGR mouse capture + bracketed
        // paste). They're enabled once at startup, but a child program run
        // through the bash tool — a pager or TUI (`git log` → `less`, `fzf`,
        // `vim`, …) — opens /dev/tty and on exit emits `?1000l`/`?2004l` to
        // restore ITS state, silently turning dirge's off. With no other
        // re-enable path the loss is permanent: wheel scroll falls through to
        // the terminal (the whole UI scrolls) and click/drag selection stops
        // reaching the app. Re-emitting on a throttle (writing to a fresh
        // /dev/tty, the same sink the guard's setup uses) heals it within one
        // interval. Done before the 8ms paint throttle below so it keeps
        // firing even while paints are coalesced.
        let now = std::time::Instant::now();
        if let Some(bytes) = mode_reassert_payload(self.last_mode_reassert, now) {
            if let Some(mut tty) = crate::ui::terminal::open_tty_for_write() {
                let _ = tty.write_all(bytes);
                let _ = tty.flush();
            }
            self.last_mode_reassert = Some(now);
        }

        // Re-clamp the scroll offset to the CURRENT geometry every frame. The
        // scroll mutators clamp at mutation time, but a terminal RESIZE changes
        // `visible_lines()` (hence `max_offset`) without going through any of
        // them — leaving a stale `scroll_offset > max_offset`. The chat would
        // then render a short window with blank rows below it and the newest
        // output unreachable until the user manually scrolls.
        let max_offset = self.buffer.len().saturating_sub(self.visible_lines());
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }

        #[cfg(feature = "experimental-ui-terminal-tab")]
        let new_title = {
            let tool = self.last_tool_name.as_deref();
            format_terminal_title(self.avatar_state, tool)
        };

        // panel-visibility borrows &self via terminal_size, so compute
        // it BEFORE we take the split mutable borrow on tui_terminal.
        let show_left_panel = self.left_panel_visible();
        let show_right_panel = self.right_panel_visible();
        let frame_color = crate::ui::theme::header();

        // Compute tooltip before the split borrow destructuring.
        let tooltip = self.current_tooltip().unwrap_or("");

        // Split borrows on Self so we can hold &mut tui_terminal
        // and immutable references to the data fields at the same
        // time. Rust's borrow checker requires we name each field
        // we intend to read here.
        let Self {
            buffer,
            scroll_offset,
            input_rows,
            panel_data,
            left_panel_info,
            subagent_status,
            alert_overlay,
            picker_overlay,
            alert_title,
            avatar_state,
            avatar_tick,
            cached_input_rows,
            cached_input_cursor_row,
            cached_input_cursor_col,
            cached_status,
            cached_is_running,
            cached_completion_preview,
            cached_input_ghost,
            cached_chat_rect,
            modified_offset,
            cached_modified_rect,
            tui_terminal,
            selection_active,
            selection_start,
            selection_end,
            right_panel_mode,
            ..
        } = self;

        let Some(terminal) = tui_terminal.as_mut() else {
            return Ok(());
        };

        // ── repaint throttle: skip if last paint was < 8ms ago ──────
        // Without this, every keystroke triggers a terminal.draw() —
        // those escape-sequence writes to /dev/tty compete with the
        // input reader, causing typing stutter. 8ms ≈ 125 fps.
        if let Some(last) = self.last_paint {
            let elapsed = last.elapsed();
            if elapsed < std::time::Duration::from_millis(8) {
                return Ok(());
            }
        }

        let face = avatar::art(*avatar_state, *avatar_tick);
        let avatar_color = crate::ui::tui::chat::crossterm_to_ratatui(avatar::color(*avatar_state));
        let avatar = Some(AvatarSpec {
            face,
            color: avatar_color,
        });

        let body = if let Some(lines) = alert_overlay.as_ref() {
            BottomBody::Overlay {
                title: alert_title.as_str(),
                lines: lines.as_slice(),
            }
        } else if let Some(lines) = self.shell_overlay.as_ref() {
            BottomBody::Overlay {
                title: "[shell]",
                lines: lines.as_slice(),
            }
        } else {
            // dirge-5w9v: scroll the editor so the cursor's wrapped row
            // stays visible once the content exceeds the capped box
            // height. The painter draws from row 0 and `.take()`s the
            // window, so without this the newest/cursor lines fell off
            // the bottom and the user's typing appeared to vanish.
            let completion_extra = if cached_completion_preview.is_empty() {
                0
            } else {
                1
            };
            let window = (*input_rows as usize)
                .saturating_sub(completion_extra)
                .max(1);
            let offset = editor_scroll_offset(
                cached_input_rows.len(),
                *cached_input_cursor_row as usize,
                window,
            );
            BottomBody::Editor {
                rows: &cached_input_rows[offset..],
                cursor_row: cached_input_cursor_row.saturating_sub(offset as u16),
                cursor_col: *cached_input_cursor_col,
                is_running: *cached_is_running,
                completion_preview: cached_completion_preview.as_str(),
                ghost: cached_input_ghost.as_str(),
            }
        };

        // Size the input box to fit the overlay (or, for the
        // editor, the wrapped editor row count). For overlays we
        // bypass MAX_INPUT_VISIBLE_LINES because the user
        // **must** see the action keys row regardless of how
        // long the alert body is — clipping at 8 was hiding
        // [y]/[a]/[n]/[ESC]. The chat shrinks to accommodate, with
        // a floor of 4 rows so the user still sees recent context
        // above the alert. The editor stays clamped at MAX so the
        // user can't accidentally crowd the chat by pasting a 50-
        // line block.
        let (cols_q, rows_q) = crate::ui::terminal::tty_size();
        let effective_input_rows = if let Some(lines) = alert_overlay.as_ref() {
            let probe = crate::ui::tui::layout::Layout::with_panels(
                cols_q,
                rows_q,
                1,
                show_left_panel,
                show_right_panel,
            );
            let wrapped =
                crate::ui::tui::bottom::overlay_wrapped_row_count(lines, probe.input_box.width);
            // Leave at least 4 rows for the chat (+ 5 fixed rows
            // of frames/status), so input_rows ≤ rows - 9.
            let ceiling = (rows_q as i32 - 9).max(1) as u16;
            (wrapped as u16).clamp(1, ceiling)
        } else if let Some(lines) = self.shell_overlay.as_ref() {
            let probe = crate::ui::tui::layout::Layout::with_panels(
                cols_q,
                rows_q,
                1,
                show_left_panel,
                show_right_panel,
            );
            let wrapped =
                crate::ui::tui::bottom::overlay_wrapped_row_count(lines, probe.input_box.width);
            let ceiling = (rows_q as i32 - 9).max(1) as u16;
            // Cap the shell box height so long sessions don't crowd out the
            // chat; the painter clips, and the box shows the tail of output.
            (wrapped as u16).clamp(1, ceiling.min(SHELL_BOX_MAX_ROWS))
        } else {
            *input_rows
        };

        // Compute the layout once so we can stash the chat rect for
        // mouse-coordinate mapping (selection::handle reads
        // cached_chat_rect to translate row/col → buffer line/char).
        // render_frame computes its own from the frame's area, but
        // with the same `(cols, rows, effective_input_rows)` inputs
        // they're identical. The terminal::size() probe used here
        // matches what render_frame sees because both go through the
        // same /dev/tty winsize.
        let layout_now = crate::ui::tui::layout::Layout::with_panels(
            cols_q,
            rows_q,
            effective_input_rows,
            show_left_panel,
            show_right_panel,
        );
        let chat_rect_now = layout_now.chat;
        *cached_chat_rect = Some(chat_rect_now);

        // dirge-b11: compute the MODIFIED sub-panel rect from the
        // current layout + panel data so the mouse handler can do
        // hit-testing before the next paint. Also clamp the offset
        // here so a list that shrunk since the last redraw doesn't
        // leave the offset stranded past the visible window.
        // Mirrors the math in `RightPanel::render` — kept in sync
        // via the shared `compute_modified_rect` helper.
        let modified_rect_now = if show_right_panel && layout_now.right_panel.width >= 16 {
            crate::ui::tui::panels::compute_modified_rect(panel_data, layout_now.right_panel)
        } else {
            None
        };
        *cached_modified_rect = modified_rect_now;
        if let Some(r) = modified_rect_now {
            let inner_rows = (r.height as usize).saturating_sub(2);
            let head_rows = inner_rows.saturating_sub(1).max(1);
            let total = panel_data.modified.len();
            let max_off = total.saturating_sub(head_rows);
            if *modified_offset > max_off {
                *modified_offset = max_off;
            }
        } else {
            *modified_offset = 0;
        }

        let chat_selection = if *selection_active {
            match (*selection_start, *selection_end) {
                (Some(s), Some(e)) => Some(normalize_selection_range(s, e)),
                _ => None,
            }
        } else {
            None
        };

        let scene = Scene {
            chat_buffer: buffer,
            scroll_offset: *scroll_offset,
            input_rows: effective_input_rows,
            chat_selection,
            panel_data,
            modified_offset: *modified_offset,
            left_info: left_panel_info,
            subagents: subagent_status,
            avatar,
            body,
            status: cached_status.as_str(),
            show_left_panel,
            show_right_panel,
            frame_color,
            background: crate::ui::theme::background(),
            picker: picker_overlay.as_ref(),
            right_panel_mode: *right_panel_mode,
            tooltip,
            #[cfg(feature = "dap")]
            debug_panel_data: self.debug_panel_data.as_ref(),
        };

        // Wrap the draw in Begin/EndSynchronizedUpdate. Modern
        // terminals (iTerm2, kitty, foot, recent xterm, Windows
        // Terminal) buffer the bracketed escape sequences and
        // present the resulting frame atomically — eliminates the
        // flicker we'd otherwise see as ratatui emits one escape
        // per changed cell sequentially. Terminals that don't
        // implement the sequence ignore it (it's a private DECSET
        // ?2026), so the bracket is harmless backwards-compat.
        use crossterm::ExecutableCommand as _;
        use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
        // dirge-wk7m: emit the brackets through the SAME backend writer
        // Synchronized-update brackets eliminate flicker on modern terminals.
        // The sandbox always paints through /dev/tty so sync is always viable.
        let _ = terminal.backend_mut().execute(BeginSynchronizedUpdate);
        // `.map(|_| ())` drops the returned `CompletedFrame` (which borrows
        // `terminal`) right away, so the End bracket below can re-borrow the
        // backend.
        let draw_result = terminal.draw(|f| render_frame(&scene, f)).map(|_| ());
        let _ = terminal.backend_mut().execute(EndSynchronizedUpdate);
        draw_result?;
        self.last_paint = Some(std::time::Instant::now());
        self.needs_paint = false;

        #[cfg(feature = "experimental-ui-terminal-tab")]
        {
            if new_title != self.cached_terminal_title {
                self.cached_terminal_title.clone_from(&new_title);
                let osc = osc_set_title(&new_title);
                let _ = terminal.backend_mut().write_all(&osc);
            }
        }

        Ok(())
    }

    /// dirge-ov2: append a new chat (typically a subagent) with the
    /// supplied display name. Returns the new chat's index, which the
    /// caller stores so it can target events at this chat later via
    /// `switch_chat`.
    ///
    /// The new chat starts empty — no buffer entries, no selection,
    /// no scroll. Does NOT switch to it; the caller chooses when to
    /// surface the new chat in the UI.
    pub fn add_chat(&mut self, name: impl Into<String>) -> usize {
        self.chats.push(ChatSnapshot::empty(name.into()));
        self.needs_paint = true;
        self.chats.len() - 1
    }

    /// dirge-ov2: switch the active chat. Saves the current chat's
    /// state to its snapshot, loads the target chat's snapshot into
    /// the Renderer's hot fields, and triggers a viewport repaint via
    /// the next render call. No-op if `idx == active_chat`.
    pub fn switch_chat(&mut self, idx: usize) {
        if idx == self.active_chat || idx >= self.chats.len() {
            return;
        }
        self.save_active();
        self.active_chat = idx;
        self.load_active();
        self.needs_paint = true;
    }

    /// Cycle to the next chat (wraps from last → first).
    /// No-op when there's only one chat.
    #[allow(dead_code)]
    pub fn next_chat(&mut self) {
        if self.chats.len() <= 1 {
            return;
        }
        let next = if self.active_chat + 1 >= self.chats.len() {
            0
        } else {
            self.active_chat + 1
        };
        self.switch_chat(next);
    }

    /// Cycle to the previous chat (wraps from first → last).
    /// No-op when there's only one chat.
    #[allow(dead_code)]
    pub fn prev_chat(&mut self) {
        if self.chats.len() <= 1 {
            return;
        }
        let prev = if self.active_chat == 0 {
            self.chats.len() - 1
        } else {
            self.active_chat - 1
        };
        self.switch_chat(prev);
    }

    /// Remove a chat by index. The active chat is adjusted:
    /// - If `idx < active`, active shifts down by 1.
    /// - If `idx == active`, moves to idx (which becomes the next
    ///   chat after removal) or wraps to 0 if at the end.
    /// - If `idx > active`, active stays unchanged.
    ///
    /// Refuses to remove the last remaining chat.
    pub fn remove_chat(&mut self, idx: usize) {
        if self.chats.len() <= 1 || idx >= self.chats.len() {
            return;
        }
        self.chats.remove(idx);
        if idx < self.active_chat {
            self.active_chat -= 1;
        } else if idx == self.active_chat && self.active_chat >= self.chats.len() {
            self.active_chat = 0;
        }
        self.needs_paint = true;
    }

    pub fn active_chat(&self) -> usize {
        self.active_chat
    }

    pub fn chat_count(&self) -> usize {
        self.chats.len()
    }

    pub fn chat_names(&self) -> Vec<String> {
        // Active chat's name lives in `chats[active_chat]` too (kept
        // in sync at add-time; mutations of the active chat's name
        // would go through a dedicated setter if added later).
        self.chats.iter().map(|c| c.name.clone()).collect()
    }

    /// dirge-ov2: snapshot the current hot fields into the active
    /// chat's slot. Called before switching chats and when the
    /// caller wants a consistent persistent state (e.g. session
    /// save).
    fn save_active(&mut self) {
        let slot = &mut self.chats[self.active_chat];
        slot.buffer = std::mem::take(&mut self.buffer);
        slot.source = std::mem::take(&mut self.source);
        slot.streaming = self.streaming;
        slot.open_rows = self.open_rows;
        slot.partial = std::mem::take(&mut self.partial);
        slot.partial_color = self.partial_color;
        slot.scroll_offset = self.scroll_offset;
        slot.lines = self.lines;
        slot.col = self.col;
        slot.selection_active = self.selection_active;
        slot.selection_start = self.selection_start;
        slot.selection_end = self.selection_end;
    }

    /// dirge-ov2: load the active chat's snapshot into the hot
    /// fields. Inverse of `save_active`. Called after `switch_chat`
    /// updates `active_chat`.
    fn load_active(&mut self) {
        let slot = &mut self.chats[self.active_chat];
        self.buffer = std::mem::take(&mut slot.buffer);
        self.source = std::mem::take(&mut slot.source);
        self.streaming = slot.streaming;
        self.open_rows = slot.open_rows;
        self.partial = std::mem::take(&mut slot.partial);
        self.partial_color = slot.partial_color;
        self.scroll_offset = slot.scroll_offset;
        self.lines = slot.lines;
        self.col = slot.col;
        self.selection_active = slot.selection_active;
        self.selection_start = slot.selection_start;
        self.selection_end = slot.selection_end;
    }
}

impl ChatSnapshot {
    fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            buffer: Vec::new(),
            source: Vec::new(),
            streaming: false,
            open_rows: 0,
            partial: CompactString::new(""),
            partial_color: Color::White,
            scroll_offset: 0,
            lines: 0,
            col: 0,
            selection_active: false,
            selection_start: None,
            selection_end: None,
        }
    }
}

impl Renderer {
    #[allow(dead_code)]
    fn _ov2_phase_a_anchor() {}

    /// dirge-ov2 Phase E: append a line to a SPECIFIC chat's buffer
    /// without disturbing the active chat's on-screen state. If
    /// `idx` is the active chat, falls through to the regular
    /// `write_line` so the line is also painted to stdout. For
    /// inactive chats the line is pushed to the snapshot's buffer
    /// only — visible the next time the user switches to that
    /// chat via Ctrl-N/P/X.
    pub fn write_line_to_chat(&mut self, idx: usize, text: &str, color: Color) -> io::Result<()> {
        if idx == self.active_chat {
            return self.write_line(text, color);
        }
        if let Some(slot) = self.chats.get_mut(idx) {
            for line in text.split('\n') {
                slot.buffer.push(LineEntry {
                    text: CompactString::from(line),
                    color,
                });
                slot.lines = slot.lines.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Update the avatar state and trigger a repaint of the bottom-left
    /// pixels. Cheap when the state hasn't changed — only the existing
    /// 3-row × 5-col patch is re-drawn.
    pub fn set_avatar_state(&mut self, state: crate::ui::avatar::AvatarState) {
        if self.avatar_state != state {
            self.avatar_state = state;
            self.needs_paint = true;
        }
    }

    #[cfg(feature = "experimental-ui-terminal-tab")]
    pub fn set_last_tool_name(&mut self, name: &str) {
        self.last_tool_name = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
    }

    /// Set BOTH side panels to the same mode (the `/panel on|off|auto`
    /// command and any caller that toggles the sidebar as a unit).
    pub fn set_panel_mode(&mut self, mode: PanelMode) {
        self.left_panel_mode = mode;
        self.right_panel_mode = mode;
    }

    /// Set only the right panel mode (used by `/panel debug`).
    pub fn set_right_panel_mode(&mut self, mode: PanelMode) {
        self.right_panel_mode = mode;
    }

    /// Apply a parsed `/display` selection (or the `display` config
    /// value): each listed side panel is forced on, each omitted one
    /// forced off — an explicit user choice, so `On`/`Off` rather than
    /// `Auto`.
    pub fn set_pane_visibility(&mut self, vis: PaneVisibility) {
        self.left_panel_mode = if vis.left {
            PanelMode::On
        } else {
            PanelMode::Off
        };
        self.right_panel_mode = if vis.right {
            PanelMode::On
        } else {
            PanelMode::Off
        };
    }

    pub fn left_panel_mode(&self) -> PanelMode {
        self.left_panel_mode
    }

    pub fn right_panel_mode(&self) -> PanelMode {
        self.right_panel_mode
    }

    /// dirge-gek: replace the subagent panel data. UI loop calls this
    /// on each subagent lifecycle event (Spawn / Complete / Failed)
    /// and on Ctrl-N/P chat switch so the panel reflects current
    /// state. Cheap — just swaps the Vec; the next `render_viewport`
    /// repaints the gutter.
    pub fn set_subagent_status(&mut self, rows: Vec<SubagentStatusRow>) {
        self.subagent_status = rows;
    }

    /// ui-redesign: set the idle-state info shown in the left panel
    /// (DIRGE logo + agent metadata). The UI loop calls this at
    /// session start + on `/model` / `/prompt` switches so the
    /// gutter stays current.
    pub fn set_left_panel_info(&mut self, info: LeftPanelInfo) {
        self.left_panel_info = info;
    }

    /// Update the DAP debug panel snapshot. Called each UI tick
    /// when the DAP feature is enabled. When a debug session becomes
    /// active (data transitions from None to Some), auto-switches
    /// the right panel to Debug mode so the user sees the session
    /// state without needing /panel debug or /debug panel.
    #[cfg(feature = "dap")]
    pub fn set_debug_panel_data(&mut self, data: Option<crate::dap::types::DebugPanelData>) {
        let was_active = self.debug_panel_data.is_some();
        self.debug_panel_data = data;
        if !was_active && self.debug_panel_data.is_some() {
            self.right_panel_mode = PanelMode::Debug;
        }
    }

    /// ui-redesign Phase 6: set the alert overlay. While `Some`, the
    /// `[ALERT]` frame contains the supplied lines instead of the
    /// input editor. The ask handler builds the lines, pushes them
    /// here on prompt-open, and calls `clear_alert_overlay` on
    /// response.
    ///
    /// Lines are painted centered horizontally within the frame's
    /// inner band. Caller is responsible for keeping line count
    /// within `MAX_INPUT_VISIBLE_LINES` — taller overlays clip.
    pub fn set_alert_overlay(&mut self, rows: Vec<(String, Color)>) {
        self.alert_overlay = Some(rows);
        if self.alert_title.is_empty() {
            self.alert_title = "[ALERT]".to_string();
        }
        self.last_paint = None;
        self.needs_paint = true;
    }

    pub fn clear_alert_overlay(&mut self) {
        self.alert_overlay = None;
        self.alert_title.clear();
        self.last_paint = None;
        self.needs_paint = true;
    }

    /// Set the live shell-session overlay (bottom box showing PTY output while
    /// a `!cmd`/`!!cmd` run is mounted). Cleared when the shell exits.
    pub fn set_shell_overlay(&mut self, rows: Vec<(String, Color)>) {
        self.shell_overlay = Some(rows);
        self.last_paint = None;
        self.needs_paint = true;
    }

    pub fn clear_shell_overlay(&mut self) {
        self.shell_overlay = None;
        self.last_paint = None;
        self.needs_paint = true;
    }

    /// Set (or clear, with `None`) the rewind-mode list-picker overlay. The
    /// file-completion picker syncs itself from the input editor in
    /// `draw_bottom`; the rewind picker lives outside the editor, so its flow
    /// sets this explicitly on enter/update and clears it on exit [dirge-92em].
    pub fn set_rewind_overlay(&mut self, overlay: Option<crate::ui::picker::PickerOverlay>) {
        self.rewind_overlay = overlay;
        self.needs_paint = true;
    }

    pub fn set_panel_data(&mut self, data: PanelData) {
        // dirge-b11: when the MODIFIED list GROWS (a new file
        // modification just entered the tracker) reset the user's
        // scroll offset so they immediately see the newest entry.
        // Shrinkage (entries pruned out the back at 256-cap) leaves
        // the offset alone; the render-time clamp handles the case
        // where the offset would otherwise point past the end of
        // the list. First push (last_modified_len is None) is not a
        // growth event.
        let new_len = data.modified.len();
        if let Some(prev) = self.last_modified_len
            && new_len > prev
        {
            self.modified_offset = 0;
        }
        self.last_modified_len = Some(new_len);
        self.panel_data = data;
    }

    /// dirge-b11: walk the MODIFIED sub-panel scroll offset by
    /// `delta` lines. Positive = older (offset increases), negative
    /// = newer. No-op when the list is shorter than `visible_rows`.
    /// Clamps so the offset can't strand past the end of the list —
    /// `offset.clamp(0, list_len.saturating_sub(visible_rows))`.
    /// Returns true when the offset actually changed so the caller
    /// can decide whether to repaint.
    pub fn panel_modified_scroll(&mut self, delta: isize, visible_rows: usize) -> bool {
        let total = self.panel_data.modified.len();
        if total <= visible_rows {
            // List fits — nothing to scroll. Reset just in case the
            // user had scrolled the list when it was longer.
            let was = self.modified_offset;
            self.modified_offset = 0;
            return was != 0;
        }
        let max_off = total.saturating_sub(visible_rows);
        let prev = self.modified_offset as isize;
        let next = (prev + delta).clamp(0, max_off as isize);
        let next = next as usize;
        let changed = next != self.modified_offset;
        self.modified_offset = next;
        if changed {
            self.needs_paint = true;
        }
        changed
    }

    /// Resolve a single side panel's mode against the current terminal
    /// size. Hidden when `Off`, or when the terminal is too narrow to fit
    /// both the panel and a usable content area (content_indent reflects
    /// each side's width in the centered layout, so require ~15 cols min).
    fn side_panel_visible(&self, mode: PanelMode) -> bool {
        let (cols, _) = self.terminal_size();
        match mode {
            PanelMode::Off => false,
            PanelMode::On => self.content_indent() >= 15,
            PanelMode::Auto => cols >= PANEL_AUTO_MIN_COLS && self.content_indent() >= 15,
            PanelMode::Debug => cols >= PANEL_AUTO_MIN_COLS && self.content_indent() >= 15,
        }
    }

    pub fn left_panel_visible(&self) -> bool {
        self.side_panel_visible(self.left_panel_mode)
    }

    pub fn right_panel_visible(&self) -> bool {
        self.side_panel_visible(self.right_panel_mode)
    }

    fn terminal_size(&self) -> (u16, u16) {
        #[cfg(test)]
        if let Some(cols) = self.test_cols {
            return (cols, 24);
        }
        crate::ui::terminal::tty_size()
    }

    /// Test-only: force the reported terminal width so width-dependent paths
    /// (wrap, `content_width`, reflow) can be exercised at a chosen size.
    #[cfg(test)]
    pub(crate) fn set_test_cols(&mut self, cols: u16) {
        self.test_cols = Some(cols);
    }

    /// Width chat text wraps to before pushing into the buffer. Matches
    /// the painted chat band (`chat_band_width`) minus the 1-col right
    /// margin `ChatPane` reserves, so chat text fills the band to the
    /// right │ exactly like the chamber boxes — no capped-then-padded
    /// dead zone on wide terminals. When side panels are visible the band
    /// is itself capped (panels take the gutter), so this still honors
    /// the readability cap in that mode.
    pub(crate) fn max_line_width(&self) -> usize {
        self.chat_band_width().saturating_sub(1).max(1)
    }

    /// The display width the compose buffer is soft-wrapped to in the
    /// input box (content width minus the 3-col prompt prefix). Mirrors
    /// the `wrap_w` computed in `draw_bottom`; pushed into the editor so
    /// Up/Down can move by wrapped display rows (dirge-5w9v).
    pub fn input_wrap_w(&self) -> usize {
        self.content_width().saturating_sub(3).max(1)
    }

    /// Raw width of the chat band (terminal width minus 2 cols for
    /// the chat frame's left + right │). Used for *positioning*
    /// math (`content_indent`, panel widths) — chat text wrapping
    /// should go through `max_line_width` / `content_width` so it
    /// honors the 120-col cap.
    pub fn line_width(&self) -> usize {
        let (cols, _) = self.terminal_size();
        cols.saturating_sub(2) as usize
    }

    /// Target width for chat content. Caps at 120 cols so wide
    /// terminals don't stretch chambers + chat lines into sprawling
    /// rivers of text. Still drives `content_indent` (centering) and,
    /// through it, side-panel visibility — so this MUST stay capped even
    /// though chamber/chat rendering now spans the full band (see
    /// `chat_band_width`).
    pub fn content_width(&self) -> usize {
        self.line_width().min(120)
    }

    /// Width of the chat band actually painted by `ChatPane`, i.e.
    /// `Layout::chat.width` for the current terminal + panel visibility.
    /// Unlike `content_width` this is NOT capped at 120 and reclaims the
    /// gutter of any hidden side panel — so it matches the real paint
    /// rect. Chamber boxes and chat text wrap to this so they span to the
    /// right │ instead of stopping at a capped width and leaving a dead
    /// band on the right (where stale glyphs showed). The horizontal
    /// layout depends only on `cols` + panel flags, so the rows/input
    /// args are placeholders (dirge: chamber-width fix).
    pub fn chat_band_width(&self) -> usize {
        let (cols, _) = self.terminal_size();
        let layout = crate::ui::tui::layout::Layout::with_panels(
            cols,
            1,
            1,
            self.left_panel_visible(),
            self.right_panel_visible(),
        );
        layout.chat.width as usize
    }

    /// Left padding in columns to horizontally center the chat
    /// content area (`content_width`) within the visible chat band
    /// (`line_width`). Zero when content already fills the band.
    pub fn content_indent(&self) -> usize {
        let band = self.line_width();
        let target = self.content_width();
        band.saturating_sub(target) / 2
    }

    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Counter of front-eviction events. A held absolute line index is
    /// only valid while this is unchanged (see `eviction_generation`).
    pub fn eviction_generation(&self) -> u64 {
        self.eviction_generation
    }

    #[allow(dead_code)]
    pub fn buffer_lines(&self) -> Vec<&str> {
        self.buffer.iter().map(|e| e.text.as_str()).collect()
    }

    pub fn replace_from(&mut self, start: usize, lines: Vec<LineEntry>) {
        self.commit_partial();
        let old_len = self.buffer.len();
        let start = start.min(old_len);
        // dirge-qy3y: keep `source` a faithful mirror of `buffer` so `rebuild`
        // (resize reflow) reproduces this. Drop whole source blocks past the
        // boundary at/under `start`; capture any kept partial-block tail and
        // the incoming `lines` as non-reflowing `Raw` blocks (this primitive's
        // callers — modal editors, pickers, collapsed chambers — re-render
        // their own content, so it needn't reflow). The streamed register uses
        // `stream`, not this, so it still reflows as markdown.
        // Cached row counts only — NO re-render here. `replace_from` runs per
        // keystroke in modal sub-loops, so an O(scrollback) markdown re-parse
        // would lag typing.
        let mut acc = 0usize;
        let mut keep_blocks = 0usize;
        for b in &self.source {
            if acc + b.rows <= start {
                acc += b.rows;
                keep_blocks += 1;
            } else {
                break;
            }
        }
        let carry: Vec<LineEntry> = self.buffer[acc..start].to_vec();
        self.source.truncate(keep_blocks);
        if !carry.is_empty() {
            let rows = carry.len();
            self.source.push(Block {
                src: SourceBlock::Raw { rows: carry },
                rows,
            });
        }
        if !lines.is_empty() {
            self.source.push(Block {
                src: SourceBlock::Raw {
                    rows: lines.clone(),
                },
                rows: lines.len(),
            });
        }
        // A tail replace ends any open streamed block (its rows are now part of
        // the captured prefix).
        self.streaming = false;
        self.open_rows = 0;
        self.buffer.truncate(start);
        self.buffer.extend(lines);
        let new_len = self.buffer.len();
        self.lines = new_len as u16;
        self.col = 0;
        self.partial.clear();
        let visible = self.visible_lines();
        let max_offset = new_len.saturating_sub(visible);
        // When the user is scrolled up, keep the view anchored to the same
        // absolute content by shifting scroll_offset to match the size delta.
        if self.scroll_offset > 0 {
            let delta = new_len as isize - old_len as isize;
            let new_offset = (self.scroll_offset as isize + delta).max(0) as usize;
            self.scroll_offset = new_offset.min(max_offset);
        } else if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
        // #387: replacing displayed content is an explicit visible change —
        // mark dirty so the render effect repaints. Without this the modal
        // sub-loops (question/permission/dialog) that rebuild their content
        // via `replace_from` each keystroke wouldn't repaint and would look
        // frozen.
        self.needs_paint = true;
    }

    /// Number of rows reserved for chat history above the input area.
    /// Subtracts the input box (`input_rows`) and the status line (1 row).
    pub fn visible_lines(&self) -> usize {
        let (_, rows) = self.terminal_size();
        rows.saturating_sub(self.input_rows + 1 + ALERT_FRAME_ROWS + CHAT_FRAME_ROWS) as usize
    }

    /// Map a screen `(row, col)` to a `(line_idx, char_col)` anchor for
    /// granular selection. Uses the ratatui chat rect cached by
    /// `tui_redraw` so the mapping matches the actual on-screen
    /// layout (including side-panel gutters on wide terminals).
    /// Falls back to legacy math when no rect has been cached yet —
    /// pre-paint events and tests that bypass `tui_redraw`.
    pub fn buffer_pos_at(&self, row: u16, col: u16) -> Option<(usize, usize)> {
        let line_idx = self.buffer_line_at_row(row)?;
        let entry = self.buffer.get(line_idx)?;
        let clean = crate::ui::ansi::strip_ansi(&entry.text);
        let chat_x = self
            .cached_chat_rect
            .map(|r| r.x)
            .unwrap_or(self.content_indent() as u16);
        let display_col = if col < chat_x {
            0
        } else {
            (col - chat_x) as usize
        };
        let char_col = display_col_to_char_index(&clean, display_col);
        Some((line_idx, char_col))
    }

    pub fn buffer_line_at_row(&self, row: u16) -> Option<usize> {
        let total = self.buffer.len();
        if total == 0 {
            return None;
        }

        // Prefer the cached chat rect (ratatui layout); fall back to
        // legacy math only when the renderer hasn't painted yet.
        let (chat_y, visible) = if let Some(rect) = self.cached_chat_rect {
            (rect.y, rect.height as usize)
        } else {
            let (_, rows) = self.terminal_size();
            let v = rows.saturating_sub(self.input_rows + 1 + ALERT_FRAME_ROWS + CHAT_FRAME_ROWS)
                as usize;
            (1, v)
        };
        if visible == 0 {
            return None;
        }

        let chat_row = row.checked_sub(chat_y)? as usize;
        if chat_row >= visible {
            return None;
        }
        let start = if self.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));
        let idx = start + chat_row;
        if idx < total { Some(idx) } else { None }
    }

    /// Cached chat rect from the most recent `tui_redraw` call.
    /// `None` until the first paint.
    #[allow(dead_code)]
    pub fn chat_rect(&self) -> Option<ratatui::layout::Rect> {
        self.cached_chat_rect
    }

    /// Test-only setter for the cached chat rect. Lets unit tests
    /// (selection::handle, buffer_pos_at across rect shapes) drive
    /// the coordinate mapping without going through a full paint.
    #[cfg(test)]
    pub fn set_chat_rect_for_test(&mut self, rect: ratatui::layout::Rect) {
        self.cached_chat_rect = Some(rect);
    }

    /// Word-selection bounds (start inclusive, end exclusive, both as
    /// `(line, char)`) around a buffer position, for double-click select.
    /// Returns `None` when the position isn't on a word character (e.g.
    /// whitespace / punctuation), so a double-click on a gap selects
    /// nothing rather than a stray glyph.
    pub fn word_bounds_at(&self, pos: (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
        let (line, ch) = pos;
        let entry = self.buffer.get(line)?;
        let chars: Vec<char> = crate::ui::ansi::strip_ansi(&entry.text).chars().collect();
        if chars.is_empty() {
            return None;
        }
        let i = ch.min(chars.len() - 1);
        if !is_word_char(chars[i]) {
            return None;
        }
        let mut start = i;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = i;
        while end + 1 < chars.len() && is_word_char(chars[end + 1]) {
            end += 1;
        }
        Some(((line, start), (line, end + 1)))
    }

    pub fn clear_selection(&mut self) {
        self.selection_active = false;
        self.selection_start = None;
        self.selection_end = None;
        self.needs_paint = true;
    }

    pub fn selected_text(&self) -> Option<String> {
        // Normalize (start, end) so start <= end in row-major order:
        // earlier row wins; same row → earlier column wins.
        let (start, end) = match (self.selection_start, self.selection_end) {
            (Some(s), Some(e)) if (s.0, s.1) <= (e.0, e.1) => (s, e),
            (Some(s), Some(e)) => (e, s),
            _ => return None,
        };
        // Markdown rendering bakes SGR escapes into `LineEntry::text`
        // (see markdown.rs:291 — inline emphasis / code spans embed
        // `\x1b[…m` directly in the line text). The selection
        // columns are user-perceived character offsets, NOT byte
        // offsets into the escape-laden source — slicing the raw
        // text would either land mid-escape or include the escape
        // in the clipboard. Strip per-row first, then index into
        // the cleaned form.
        let row_clean = |i: usize| -> Option<Vec<char>> {
            self.buffer
                .get(i)
                .map(|e| crate::ui::ansi::strip_ansi(&e.text).chars().collect())
        };
        let mut result = String::new();
        if start.0 == end.0 {
            if let Some(chars) = row_clean(start.0) {
                let lo = start.1.min(chars.len());
                let hi = end.1.min(chars.len());
                if lo < hi {
                    result.extend(&chars[lo..hi]);
                }
            }
        } else {
            // Join rows the renderer soft-wrapped back into one line so
            // prose copies as a single line (dirge-el8o). `word_wrap`
            // keeps the breaking space on the PRIOR row, so a row whose
            // predecessor ends in whitespace is a wrap continuation —
            // append it with no separator. A predecessor that ends
            // without whitespace is a real line break (paragraph break,
            // blank line, or hard newline) and keeps its newline.
            let start_chars = row_clean(start.0);
            let start_content: String = match &start_chars {
                Some(chars) => {
                    let lo = start.1.min(chars.len());
                    chars[lo..].iter().collect()
                }
                None => String::new(),
            };
            // Base the wrap-continuation test on the FULL row's last
            // visible char, not just the selected suffix, so a selection
            // that begins at the very end of a wrapped row still joins
            // (the suffix would be empty and lose the trailing space).
            let mut prev_ended_ws = start_chars
                .as_ref()
                .and_then(|chars| chars.last())
                .is_some_and(|c| c.is_whitespace());
            result.push_str(&start_content);

            for i in (start.0 + 1)..end.0 {
                let content: String = match row_clean(i) {
                    Some(chars) => chars.into_iter().collect(),
                    None => String::new(),
                };
                if !prev_ended_ws {
                    result.push('\n');
                }
                prev_ended_ws = content.chars().next_back().is_some_and(char::is_whitespace);
                result.push_str(&content);
            }

            if !prev_ended_ws {
                result.push('\n');
            }
            if let Some(chars) = row_clean(end.0) {
                let hi = end.1.min(chars.len());
                result.extend(&chars[..hi]);
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn wrap_line(&self, line: &str, max_width: usize) -> Vec<CompactString> {
        // Every plain `write_line` ultimately routes through here.
        // Centralise on `wrap::soft_wrap` so the whole UI shares one
        // wrap policy: word-aware where possible, hard-break for
        // unbreakable runs, display-width-aware (CJK/emoji),
        // preserving hard newlines. Was previously a char-chunk
        // hard wrap that broke mid-word.
        crate::ui::wrap::soft_wrap(line, max_width, "")
            .into_iter()
            .map(CompactString::new)
            .collect()
    }

    fn commit_partial(&mut self) {
        if !self.partial.is_empty() {
            let max_width = self.max_line_width();
            let c = self.partial_color;
            // dirge-qy3y: record the flushed partial as a source block so it
            // reflows on resize like any other committed line.
            let text = self.partial.to_string();
            for chunk in self.wrap_line(&self.partial, max_width) {
                self.push_buffer_line(LineEntry {
                    text: chunk,
                    color: c,
                });
            }
            self.partial.clear();
            let block = SourceBlock::Plain { text, color: c };
            let rows = self.render_block(&block).len();
            self.source.push(Block { src: block, rows });
            self.enforce_cap();
        }
    }

    /// dirge-qy3y: render one source block to wrapped rows at the CURRENT
    /// width. The inverse direction of an append — used to (re)derive
    /// `buffer` from `source` in `rebuild` and `enforce_cap`.
    fn render_block(&self, block: &SourceBlock) -> Vec<LineEntry> {
        match block {
            SourceBlock::Plain { text, color } => {
                // `wrap_line` -> `soft_wrap` already splits on `\n` and yields a
                // single empty row for an empty line, matching `write_line`.
                self.wrap_line(text, self.max_line_width())
                    .into_iter()
                    .map(|t| LineEntry {
                        text: t,
                        color: *color,
                    })
                    .collect()
            }
            SourceBlock::Markdown {
                src,
                base_color,
                handle,
            } => {
                // The streamed register reserves the 8-col "<dirge> " handle
                // + 1 space on the first row, matching `render_agent_stream`.
                let w = if *handle {
                    self.content_width().saturating_sub(9)
                } else {
                    self.content_width()
                };
                let mut styled = crate::ui::markdown::markdown_to_styled(src, w, *base_color);
                if *handle && !styled.is_empty() {
                    styled[0].text = CompactString::from(format!("<dirge> {}", styled[0].text));
                }
                styled
            }
            SourceBlock::Raw { rows } => rows.clone(),
        }
    }

    /// dirge-qy3y: append a committed source block and its rendered rows.
    /// Seals any open streamed block first so block order matches buffer
    /// order.
    fn append_source_block(&mut self, block: SourceBlock) {
        self.commit_stream();
        let rendered = self.render_block(&block);
        let rows = rendered.len();
        for row in rendered {
            self.push_buffer_line(row);
        }
        self.source.push(Block { src: block, rows });
        self.enforce_cap();
    }

    /// dirge-qy3y: update the open in-flight streamed block (or open a new
    /// one) with the full accumulated `src`, re-rendering its rows at the
    /// buffer tail. This is the source-tracked equivalent of the old
    /// `render_agent_stream` -> `replace_from(start_line, …)` path: the open
    /// block is always the last region of the buffer, so a resize can rebuild
    /// it like any other block.
    pub fn stream(&mut self, src: &str, base_color: Color, handle: bool) {
        self.commit_partial();
        let block = SourceBlock::Markdown {
            src: src.to_string(),
            base_color,
            handle,
        };
        let rows = self.render_block(&block);
        // Replace the open block's current rows (if any) at the tail.
        let start =
            self.buffer
                .len()
                .saturating_sub(if self.streaming { self.open_rows } else { 0 });
        let old_len = self.buffer.len();
        self.buffer.truncate(start);
        self.buffer.extend(rows.iter().cloned());
        let entry = Block {
            src: block,
            rows: rows.len(),
        };
        if self.streaming {
            // Update the open block in place.
            if let Some(last) = self.source.last_mut() {
                *last = entry;
            } else {
                self.source.push(entry);
            }
        } else {
            self.source.push(entry);
            self.streaming = true;
        }
        self.open_rows = rows.len();
        self.lines = self.buffer.len() as u16;
        self.col = 0;
        self.partial.clear();
        let new_len = self.buffer.len();
        self.anchor_after_resize_delta(old_len, new_len);
        self.needs_paint = true;
    }

    /// dirge-qy3y: seal the open streamed block (no-op when not streaming).
    /// The block stays in `source`; subsequent appends start after it.
    pub fn commit_stream(&mut self) {
        if self.streaming {
            self.streaming = false;
            self.open_rows = 0;
            self.enforce_cap();
        }
    }

    /// dirge-qy3y: keep the scroll view anchored to the same content when a
    /// tail re-render changes the buffer length (mirrors the old
    /// `replace_from` logic).
    fn anchor_after_resize_delta(&mut self, old_len: usize, new_len: usize) {
        let visible = self.visible_lines();
        let max_offset = new_len.saturating_sub(visible);
        if self.scroll_offset > 0 {
            let delta = new_len as isize - old_len as isize;
            let new_offset = (self.scroll_offset as isize + delta).max(0) as usize;
            self.scroll_offset = new_offset.min(max_offset);
        } else if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    /// dirge-qy3y: block-granular scrollback cap. Drops whole front source
    /// blocks (never the open streamed tail) and the matching rows so
    /// `source` and `buffer` stay in lockstep.
    fn enforce_cap(&mut self) {
        const MAX_SCROLLBACK: usize = 20_000;
        if self.buffer.len() <= MAX_SCROLLBACK {
            return;
        }
        let mut dropped_rows = 0usize;
        loop {
            if self.buffer.len() - dropped_rows.min(self.buffer.len()) <= MAX_SCROLLBACK {
                break;
            }
            let sealed = if self.streaming {
                self.source.len().saturating_sub(1)
            } else {
                self.source.len()
            };
            if sealed == 0 {
                break;
            }
            dropped_rows += self.source[0].rows;
            self.source.remove(0);
        }
        if dropped_rows == 0 {
            return;
        }
        let dropped_rows = dropped_rows.min(self.buffer.len());
        self.buffer.drain(..dropped_rows);
        // Front eviction shifts every absolute index down — invalidate any
        // held line anchor (see `eviction_generation`).
        self.eviction_generation = self.eviction_generation.wrapping_add(1);
        if let Some(s) = self.selection_start.as_mut() {
            s.0 = s.0.saturating_sub(dropped_rows);
        }
        if let Some(e) = self.selection_end.as_mut() {
            e.0 = e.0.saturating_sub(dropped_rows);
        }
        let visible = self.visible_lines();
        let max_offset = self.buffer.len().saturating_sub(visible);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    /// dirge-qy3y: regenerate `buffer` from `source` at the current width.
    /// Called on terminal resize so scrollback — markdown tables especially —
    /// reflows to the new width instead of keeping its original wrap. The
    /// scroll anchor (lines-from-bottom) is preserved across the rebuild.
    pub fn rebuild(&mut self) {
        let old_len = self.buffer.len();
        let mut blocks = std::mem::take(&mut self.source);
        let mut buffer = Vec::new();
        let mut open_rows = 0usize;
        let last = blocks.len().saturating_sub(1);
        for (i, block) in blocks.iter_mut().enumerate() {
            let rows = self.render_block(&block.src);
            block.rows = rows.len();
            if self.streaming && i == last {
                open_rows = rows.len();
            }
            buffer.extend(rows);
        }
        self.source = blocks;
        self.buffer = buffer;
        self.open_rows = if self.streaming { open_rows } else { 0 };
        self.lines = self.buffer.len() as u16;
        let new_len = self.buffer.len();
        self.anchor_after_resize_delta(old_len, new_len);
        self.enforce_cap();
        self.needs_paint = true;
    }

    /// Append a line to the scrollback buffer. If the user is currently
    /// scrolled up (scroll_offset > 0), bumps the offset by one so the
    /// view stays anchored to the same absolute content rather than drifting
    /// forward as new lines arrive. The selection (which uses absolute
    /// indices) is unaffected.
    fn push_buffer_line(&mut self, entry: LineEntry) {
        self.buffer.push(entry);
        // dirge-qy3y: scrollback cap moved to `enforce_cap` (block-granular,
        // keeps `source` and `buffer` in lockstep). Callers that append a
        // committed region (`write_line`, `commit_partial`, `commit_stream`)
        // run `enforce_cap` after.
        if self.scroll_offset > 0 {
            let visible = self.visible_lines();
            let max_offset = self.buffer.len().saturating_sub(visible);
            self.scroll_offset = (self.scroll_offset + 1).min(max_offset);
        }
        // #387: centralize dirty-marking at the buffer primitive so no
        // higher-level appender can forget it. Gated on being at the bottom
        // (scrolled-up views don't auto-jump on new content, matching prior
        // behavior).
        if self.scroll_offset == 0 {
            self.needs_paint = true;
        }
    }

    pub fn is_scrolling(&self) -> bool {
        self.scroll_offset > 0
    }

    pub fn scroll_line_up(&mut self) {
        let visible = self.visible_lines();
        let max_offset = self.buffer.len().saturating_sub(visible);
        if self.scroll_offset < max_offset {
            self.scroll_offset += 1;
        }
        self.needs_paint = true;
    }

    pub fn scroll_line_down(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
        self.needs_paint = true;
    }

    /// True when the chat is scrolled up off the newest content
    /// (`scroll_offset > 0`). Lets the input loop snap back to the bottom the
    /// moment the user starts interacting with the input.
    pub fn is_scrolled_up(&self) -> bool {
        self.scroll_offset > 0
    }

    pub fn scroll_page_up(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        let max_offset = self.buffer.len().saturating_sub(visible);
        self.scroll_offset = (self.scroll_offset + page).min(max_offset);
        self.needs_paint = true;
    }

    pub fn scroll_page_down(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        if self.scroll_offset <= page {
            self.scroll_offset = 0;
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(page);
        }
        self.needs_paint = true;
    }

    pub fn scroll_to_top(&mut self) {
        let visible = self.visible_lines();
        self.scroll_offset = self.buffer.len().saturating_sub(visible);
        self.needs_paint = true;
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        self.scroll_offset = 0;
        self.sync_to_buffer()
    }

    fn sync_to_buffer(&mut self) -> io::Result<()> {
        self.commit_partial();
        self.col = 0;
        self.lines = self.buffer.len() as u16;
        self.render_viewport()
    }

    pub fn render_viewport(&mut self) -> io::Result<()> {
        // #387: defer. The event loop flushes once per event (model-driven
        // render effect); this just marks the frame dirty.
        self.needs_paint = true;
        Ok(())
    }

    pub fn write_line(&mut self, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        // dirge-qy3y: record as a width-independent source block (which renders
        // + appends the wrapped rows and seals any open stream) so it reflows
        // on resize. push_buffer_line still gates needs_paint on being at the
        // bottom, matching prior behavior.
        self.append_source_block(SourceBlock::Plain {
            text: text.to_string(),
            color,
        });
        Ok(())
    }

    /// dirge-qy3y: append PRE-FORMATTED rows that must NOT be re-wrapped on
    /// resize — tool-chamber borders/rows already laid out to a fixed inner
    /// width. Stored as a `Raw` block so `rebuild` reproduces them verbatim
    /// (they don't re-box yet — future work — but they don't wrap-break on a
    /// narrowing resize either). One buffer row per `\n`-split line, so the
    /// chamber's `buffer_len()` index bookkeeping is unchanged vs `write_line`.
    pub fn write_line_raw(&mut self, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        self.commit_stream();
        let rows: Vec<LineEntry> = text
            .split('\n')
            .map(|l| LineEntry {
                text: CompactString::from(l),
                color,
            })
            .collect();
        let n = rows.len();
        for row in &rows {
            self.push_buffer_line(row.clone());
        }
        self.source.push(Block {
            src: SourceBlock::Raw { rows },
            rows: n,
        });
        self.enforce_cap();
        Ok(())
    }

    pub fn write(&mut self, text: &str, color: Color) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let max_width = self.max_line_width();
        if max_width == 0 {
            return Ok(());
        }
        // ratatui path: token-by-token streaming just appends to the
        // partial line buffer + commits on newlines / wrap. The
        // ratatui Buffer diff handles which cells actually changed;
        // no direct stdout writes, no per-token MoveTo, no manual
        // CRLF handling, no Clear(CurrentLine) collateral on side
        // panels. Soft-wrap math stays here so wrapped-line counts
        // remain consistent with render math.
        let parts: Vec<&str> = text.split('\n').collect();
        let last = parts.len() - 1;
        for (i, segment) in parts.iter().enumerate() {
            if i < last {
                let len_before = self.buffer.len();
                self.commit_partial();
                let had_content = len_before < self.buffer.len();
                if !segment.is_empty() {
                    self.partial_color = color;
                    self.partial.push_str(segment);
                    self.commit_partial();
                } else if !had_content {
                    self.push_buffer_line(LineEntry {
                        text: CompactString::new(""),
                        color,
                    });
                }
                self.col = 0;
            } else if !segment.is_empty() {
                let chars: Vec<char> = segment.chars().collect();
                let mut idx = 0;
                while idx < chars.len() {
                    let avail = max_width.saturating_sub(self.col as usize);
                    if avail == 0 {
                        self.commit_partial();
                        self.col = 0;
                        continue;
                    }
                    let end = (idx + avail).min(chars.len());
                    let chunk: String = chars[idx..end].iter().collect();
                    self.partial_color = color;
                    self.partial.push_str(&chunk);
                    self.col = self.col.saturating_add(chunk.chars().count() as u16);
                    idx = end;
                    if idx < chars.len() {
                        self.commit_partial();
                        self.col = 0;
                    }
                }
            }
        }
        // #387: defer paint (see write_line). The token handler gates how
        // often this lands a dirty frame (60 fps coalescing); the loop's
        // render effect performs the single flush.
        if self.scroll_offset == 0 {
            self.needs_paint = true;
        }
        Ok(())
    }

    pub fn clear_content(&mut self) -> io::Result<()> {
        self.buffer.clear();
        self.source.clear();
        self.streaming = false;
        self.open_rows = 0;
        self.partial.clear();
        self.scroll_offset = 0;
        self.clear_selection();
        let mut stdout = io::stdout();
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(MoveTo(0, 0))?;
        stdout.flush()?;
        self.lines = 0;
        self.col = 0;
        Ok(())
    }

    /// Update the cached bottom-area state (input rows, status text,
    /// ghost/preview, picker overlay, spinner) from the editor + status.
    /// Does NOT paint — callers either paint immediately ([`draw_bottom`])
    /// or defer to the next [`flush`] ([`set_bottom`], the #387 model-
    /// driven path). Split out so the single-paint refactor can reuse the
    /// exact cached-state derivation.
    fn cache_bottom(
        &mut self,
        editor: &crate::ui::input::InputEditor,
        status: &str,
        is_running: bool,
    ) {
        // Use the editor's display projection so paste markers
        // (`\x01<idx>\x01` blocks) appear as `[N lines pasted]`
        // placeholders rather than bare digits between invisible
        // SOH bytes. `display()` also maps the cursor byte into
        // the projected string.
        // When Ctrl+R reverse-i-search is active, show the search
        // mini-buffer instead of the normal editor buffer.
        // #387: snapshot the visible bottom state so we can mark the frame
        // dirty ONLY when it actually changes. The loop calls this once per
        // event via the render effect; without change-detection that would
        // force a paint every iteration and defeat the token-stream
        // coalescing (the spinner animation is driven separately by the
        // timeout arm's request_repaint).
        let prev_status = self.cached_status.clone();
        let prev_running = self.cached_is_running;
        let prev_rows = self.cached_input_rows.clone();
        let prev_cursor = (self.cached_input_cursor_row, self.cached_input_cursor_col);
        let prev_ghost = self.cached_input_ghost.clone();
        let prev_preview = self.cached_completion_preview.clone();
        let prev_picker = self.picker_overlay.is_some();

        let (display_buf, cursor_byte) = if editor.is_in_search() {
            editor.search_display()
        } else {
            editor.display()
        };
        let full = display_buf.as_str();
        let cursor_byte = cursor_byte.min(full.len());
        // Wrap to chat-content width minus 3 cols of prompt prefix.
        let wrap_w = self.content_width().saturating_sub(3).max(1);
        let (rows, cursor_row, cursor_col) = wrap_editor(full, cursor_byte, wrap_w);
        let total_rows = rows.len() as u16;
        self.cached_input_rows = rows;
        self.cached_input_cursor_row = cursor_row;
        self.cached_input_cursor_col = cursor_col;
        // Inline ghost completion: only when the cursor is at the very end
        // of an in-progress slash command (so the ghost paints right after
        // the typed text and Right-to-accept is unambiguous).
        #[cfg(feature = "slash-completion")]
        {
            self.cached_input_ghost = if cursor_byte == full.len() {
                crate::ui::slash::ghost_suffix(full).unwrap_or_default()
            } else {
                String::new()
            };
        }
        #[cfg(not(feature = "slash-completion"))]
        {
            self.cached_input_ghost = String::new();
        }
        self.cached_status = status.to_string();
        self.cached_is_running = is_running;
        self.input_rows = total_rows.clamp(1, MAX_INPUT_VISIBLE_LINES as u16);

        // Build slash-command completion preview if active.
        #[cfg(feature = "slash-completion")]
        {
            self.cached_completion_preview =
                crate::ui::slash::format_completion_preview(editor.completion.as_ref(), wrap_w);
        }
        #[cfg(not(feature = "slash-completion"))]
        {
            self.cached_completion_preview = String::new();
        }
        let completion_extra: u16 = if self.cached_completion_preview.is_empty() {
            0
        } else {
            1
        };
        self.input_rows = (total_rows + completion_extra).clamp(1, MAX_INPUT_VISIBLE_LINES as u16);

        if is_running {
            self.spinner_tick = !self.spinner_tick;
            self.avatar_tick = !self.avatar_tick;
        }

        // Sync the picker overlay from the editor's file picker (the source of
        // truth — auto-clears when it deactivates), falling back to a
        // rewind-mode overlay set externally. Cached so `render_viewport`
        // (no editor handle) repaints it too [dirge-92em].
        self.picker_overlay = editor
            .picker
            .as_ref()
            .filter(|p| p.active)
            .map(|p| p.overlay())
            .or_else(|| self.rewind_overlay.clone());

        // Mark dirty iff a visible bottom element changed.
        if prev_status != self.cached_status
            || prev_running != self.cached_is_running
            || prev_rows != self.cached_input_rows
            || prev_cursor != (self.cached_input_cursor_row, self.cached_input_cursor_col)
            || prev_ghost != self.cached_input_ghost
            || prev_preview != self.cached_completion_preview
            || prev_picker != self.picker_overlay.is_some()
        {
            self.needs_paint = true;
        }
    }

    /// Cache the bottom state and mark the frame dirty on change, WITHOUT
    /// painting (the #387 model-driven path). The event loop builds the
    /// status line once from the model, calls this, then [`flush`] paints.
    /// `draw_bottom` is retained as an alias for the many existing call
    /// sites; both defer now.
    pub fn draw_bottom(
        &mut self,
        editor: &crate::ui::input::InputEditor,
        status: &str,
        is_running: bool,
    ) -> io::Result<()> {
        self.cache_bottom(editor, status, is_running);
        Ok(())
    }

    /// #387: model-driven bottom update — alias of the deferred `draw_bottom`
    /// with a `()` return for new call sites.
    pub fn set_bottom(
        &mut self,
        editor: &crate::ui::input::InputEditor,
        status: &str,
        is_running: bool,
    ) {
        self.cache_bottom(editor, status, is_running);
    }

    /// #387: mark the frame dirty so the next [`flush`] repaints. Mutators
    /// that change on-screen content call this instead of painting inline.
    pub fn request_repaint(&mut self) {
        self.needs_paint = true;
    }

    /// Whether a frame is marked dirty but not yet painted — e.g. the
    /// `tui_redraw` paint throttle deferred it. The event loop polls
    /// this so a throttled frame (the tail of a fast wheel/PageUp scroll
    /// burst) gets flushed shortly instead of being stranded until the
    /// next unrelated event.
    pub fn needs_paint(&self) -> bool {
        self.needs_paint
    }

    /// #387: the single paint per event. Performs one `tui_redraw` iff the
    /// frame is dirty. The flag is cleared inside `tui_redraw` only
    /// after a successful `terminal.draw()`, so a throttled paint or
    /// draw failure retries on the next event-loop iteration. A no-op
    /// when nothing changed (preserves token-stream coalescing — the
    /// token handler only marks dirty at frame intervals).
    pub fn flush(&mut self) -> io::Result<()> {
        if self.needs_paint {
            self.tui_redraw()
        } else {
            Ok(())
        }
    }

    /// Flag the renderer for a full repaint (session + viewport + bottom)
    /// on the next main-loop iteration.
    #[cfg(unix)]
    pub fn set_needs_repaint(&mut self) {
        self.needs_paint = true;
    }

    /// Re-create the ratatui Terminal with a fresh backend and empty
    /// diff buffer — forces a full paint on the next frame, identical
    /// to what happens at startup. Used after `/sandbox attach` restores
    /// the TUI so the screen is completely repainted instead of diff'd
    /// against a stale pre-attach buffer.
    #[cfg(unix)]
    pub fn reset_tui(&mut self) {
        self.tui_terminal = build_tui_terminal();
    }
}

/// One visible row of the input box after soft-wrapping. A logical line
/// (between newlines in the buffer) may produce multiple visual rows when
/// it exceeds the terminal's wrap width.
///
/// Currently unused by production code (the ratatui BottomStrip renders
/// one input row only). Kept because multi-row input is the next likely
/// feature to land — re-using this `wrap_input` + tests means we don't
/// have to re-derive the cursor-placement-at-wrap-boundary logic.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub logical_line: usize,
    pub char_start: usize,
    pub char_end: usize,
}

/// Wrap pre-rendered display lines to `wrap_width` columns and locate the
/// cursor in the resulting visual grid. Returns `(rows, cursor_row, cursor_col)`.
///
/// Cursor placement at exact wrap boundaries (cursor sits at end-of-line
/// where chars exactly fill the row) keeps the cursor at the right edge of
/// the filled row rather than jumping to an empty phantom row beneath it,
/// matching what most line editors do.
#[allow(dead_code)]
pub(crate) fn wrap_input(
    display_lines: &[String],
    cursor_line_idx: usize,
    cursor_display_col: usize,
    wrap_width: usize,
) -> (Vec<VisualRow>, usize, usize) {
    let wrap_width = wrap_width.max(1);
    let mut rows: Vec<VisualRow> = Vec::new();
    let mut cursor_visual_row = 0usize;
    let mut cursor_visual_col = 0usize;

    for (li, line) in display_lines.iter().enumerate() {
        // B3-8 (audit fix): the cursor end-of-line detection
        // previously compared `cursor_display_col == char_count`,
        // misfiring on lines containing wide chars (CJK / emoji)
        // because col is a DISPLAY column and char_count is a
        // CHAR count. For a line like "日本" with cursor at the
        // end, col=4 (display cells) but char_count=2 — the
        // comparison failed and the cursor wrapped to row 1.
        // Compare against the line's display WIDTH instead.
        //
        // Row count and char_start/char_end slicing remain in
        // CHAR units (callers slice the chars vector). For pure
        // ASCII this is equivalent. Lines with wide chars + soft-
        // wrap can still split mid-double-width but the cursor
        // position math is correct.
        use unicode_width::UnicodeWidthStr;
        let char_count = line.chars().count();
        let display_width = UnicodeWidthStr::width(line.as_str());
        let row_count = if char_count == 0 {
            1
        } else {
            char_count.div_ceil(wrap_width)
        };

        let base = rows.len();
        let mut emitted = row_count;

        if li == cursor_line_idx {
            let col = cursor_display_col;
            let (vr, vc) = if col > 0 && col == display_width && col % wrap_width == 0 {
                // End of a line that exactly fills the last row — stay on
                // the filled row, position cursor past its last char.
                (col / wrap_width - 1, wrap_width)
            } else {
                (col / wrap_width, col % wrap_width)
            };
            cursor_visual_row = base + vr;
            cursor_visual_col = vc;
            // Empty or short logical line still needs a row for the cursor.
            if vr + 1 > emitted {
                emitted = vr + 1;
            }
        }

        for r in 0..emitted {
            let cs = (r * wrap_width).min(char_count);
            let ce = ((r + 1) * wrap_width).min(char_count);
            rows.push(VisualRow {
                logical_line: li,
                char_start: cs,
                char_end: ce,
            });
        }
    }

    (rows, cursor_visual_row, cursor_visual_col)
}

/// B3-8: map a DISPLAY column on `s` to its CHAR index. ASCII-only
/// strings return `display_col` verbatim; lines containing CJK /
/// emoji compress to half the char count for full-width glyphs.
/// Clamps to the line's char count when `display_col` overshoots.
///
/// Used by `Renderer::buffer_pos_at` so mouse drag → clipboard
/// selection lines up with the visible characters on screen,
/// not the raw char positions which would mis-land in the middle
/// of double-width glyphs.
pub(crate) fn display_col_to_char_index(s: &str, display_col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut acc = 0usize;
    for (char_idx, ch) in s.chars().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc >= display_col {
            return char_idx;
        }
        // If adding this char's width would cross the target,
        // anchor on the boundary BEFORE the char (so a click in
        // the middle of a 2-cell glyph lands at the glyph's start,
        // not after it).
        if acc + w > display_col {
            return char_idx;
        }
        acc += w;
    }
    s.chars().count()
}

/// Truncate a string from the LEFT so the tail survives when content
/// overflows. Useful for paths where the filename matters more than
/// the prefix: `…clj/yourname/foo.rs` reads better than `src/clj/…`.
/// Returns the input verbatim when `s` fits in `max` chars.
/// Wrap the input editor's buffer into visual rows + locate the
/// cursor. Splits on `\n` (logical lines), then soft-wraps each
/// logical line to `wrap_w` display cells. Returns the wrapped
/// rows and the cursor's (row, col) position within them.
///
/// `cursor_byte` is the byte offset into `full`; conversion to
/// display cells handles multi-byte UTF-8 (the cursor column is
/// the display width of the row prefix up to the byte).
pub(crate) fn wrap_editor(
    full: &str,
    cursor_byte: usize,
    wrap_w: usize,
) -> (Vec<String>, u16, u16) {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let wrap_w = wrap_w.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row: u16 = 0;
    let mut cursor_col: u16 = 0;
    let cursor_byte = cursor_byte.min(full.len());

    let mut byte_idx: usize = 0;
    for logical in full.split('\n') {
        let logical_start = byte_idx;
        let _logical_end = logical_start + logical.len();

        // Word-aware soft wrapping for this logical line.
        let mut cur = String::new();
        let mut cur_w: usize = 0;
        let mut local_byte: usize = 0;

        for ch in logical.chars() {
            let w = ch.width().unwrap_or(0);
            if cur_w + w > wrap_w && !cur.is_empty() {
                // Find last whitespace to break at a word boundary.
                let break_at = cur.rfind([' ', '\t']);
                match break_at {
                    Some(ws_idx) => {
                        // Word-boundary break.  Split at the whitespace:
                        // prefix stays on this row, suffix (whitespace +
                        // trailing text) moves to the continuation row.
                        let prefix: String = cur[..ws_idx].to_string();
                        let suffix: String = cur[ws_idx..].trim_start().to_string();

                        let row_start = logical_start + local_byte - cur.len();
                        let row_end = row_start + prefix.len();
                        rows.push(prefix);
                        if cursor_byte >= row_start && cursor_byte <= row_end {
                            cursor_row = rows.len() as u16 - 1;
                            cursor_col =
                                UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)])
                                    as u16;
                        }
                        // Start continuation row with the dangling suffix.
                        cur = suffix;
                        cur_w = UnicodeWidthStr::width(cur.as_str());
                    }
                    None => {
                        // No whitespace — a single token is wider than the
                        // row budget.  Fall back to character-level break.
                        let row_start = logical_start + local_byte - cur.len();
                        let row_end = row_start + cur.len();
                        rows.push(std::mem::take(&mut cur));
                        if cursor_byte >= row_start && cursor_byte <= row_end {
                            cursor_row = rows.len() as u16 - 1;
                            cursor_col =
                                UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)])
                                    as u16;
                        }
                        cur_w = 0;
                    }
                }
            }
            cur.push(ch);
            cur_w += w;
            local_byte += ch.len_utf8();
        }

        // Remaining characters on this logical line form the last row.
        let row_start = logical_start + local_byte - cur.len();
        let row_end = logical_start + local_byte;
        rows.push(cur);
        if cursor_byte >= row_start && cursor_byte <= row_end {
            cursor_row = rows.len() as u16 - 1;
            cursor_col = UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)]) as u16;
        }

        // Advance past this logical line + the '\n'.
        byte_idx += logical.len() + 1;
    }

    if rows.is_empty() {
        rows.push(String::new());
    }
    (rows, cursor_row, cursor_col)
}

/// Top scroll offset for the editor box so the cursor's wrapped row
/// stays visible within a `window`-row viewport (dirge-5w9v). Returns
/// the index of the first row to paint. `0` when everything fits.
///
/// Pre-fix the painter always drew from row 0 and `.take(window)`'d, so
/// once the wrapped content exceeded the capped box height the newest /
/// cursor lines fell off the bottom and the user's typing "vanished".
pub(crate) fn editor_scroll_offset(total_rows: usize, cursor_row: usize, window: usize) -> usize {
    if window == 0 || total_rows <= window {
        return 0;
    }
    let max_offset = total_rows - window;
    // Scroll just enough to land the cursor on the last visible row when
    // it's past the window; clamp so we never scroll past the end.
    cursor_row.saturating_sub(window - 1).min(max_offset)
}

// Used by the legacy modified-files panel; the new SubPanel widget
// doesn't truncate paths the same way (set_stringn clips at width).
// Kept because multi-line input wrap will likely need a similar
// shortening helper once it lands.
#[allow(dead_code)]
fn left_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // Reserve 1 char for the leading `…`; keep the last `max-1` chars.
    let start = chars.len() - (max - 1);
    let mut out = String::with_capacity(max);
    out.push('…');
    out.extend(&chars[start..]);
    out
}

pub fn copy_to_clipboard(text: &str) {
    let cmds: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("pbcopy", &[]),
        ("clip.exe", &[]),
    ];
    for &(cmd, args) in cmds {
        if let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
                let _ = stdin.flush();
            }
            // Bounded wait so a wedged helper (broken XWayland,
            // frozen compositor, missing $DISPLAY for xclip) can't
            // freeze the TUI on a copy keystroke. ~2s is generous —
            // a healthy `pbcopy`/`wl-copy`/`xclip` returns in ms.
            // On expiry we SIGKILL the child and move on; the user
            // sees no immediate feedback but the editor stays
            // responsive.
            const CLIP_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_millis(2000);
            let poll_interval = std::time::Duration::from_millis(25);
            let deadline = std::time::Instant::now() + CLIP_WAIT_LIMIT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            // Reap the now-killed child so we don't
                            // leave a zombie behind. Ignore errors —
                            // best-effort cleanup.
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(poll_interval);
                    }
                    Err(_) => break,
                }
            }
            return;
        }
    }
}

#[cfg(test)]
#[path = "renderer_tests.rs"]
mod tests;
