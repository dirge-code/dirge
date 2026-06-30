//! `UiState` — the interactive event loop's data model (issue #387).
//!
//! The TUI is moving to a model-driven architecture: this struct is the
//! single source of truth for everything the event loop mutates, and the
//! rendered UI (status line, bottom input area, avatar, side panels, and
//! the deferred single paint) is derived from it as an **effect of the
//! model changing** — see [`crate::ui::render`]. Handlers update the
//! model; the loop renders once per event from the model. That replaces
//! the previous design where ~36 mutable locals were threaded through the
//! handlers and ~85 ad-hoc `render_viewport`/`draw_bottom`/`StatusLine`
//! call sites painted inline.
//!
//! Fields are grouped by concern. They are intentionally `pub(crate)` so
//! the event loop, the `run_handlers`, and the render effect can borrow
//! disjoint fields simultaneously (e.g. `&mut ui.stream` while reading
//! `&ui.loop_label`), which the borrow checker permits on distinct paths.
//!
//! NOTE: the chat scrollback buffer itself still lives in [`Renderer`]
//! (it is the *rendered* output, appended incrementally as the effect of
//! message/token/tool transitions). `UiState` holds the logical state
//! that *drives* what gets rendered, not the painted cells.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indexmap::IndexMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::AgentEvent;
use crate::session::ToolCallEntry;

use super::chat_state::ChatUiState;
use super::picker::ListPicker;
use super::tool_display::CollapsedToolResult;

/// Recent-tool-activity ticker capacity (left panel). Mirrors the prior
/// `TOOL_ACTIVITY_CAP` local.
pub(crate) const TOOL_ACTIVITY_CAP: usize = 8;

/// Which collapsed block the Ctrl+O expand/collapse toggle targets — the
/// most recently truncated thinking burst or tool/command output. `None`
/// means nothing has been truncated yet, so Ctrl+O is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ExpandTarget {
    #[default]
    None,
    Thinking,
    Tool,
}

/// What the Ctrl+O keypress should do this time, given the current toggle
/// state. Pure decision, applied by the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpandToggle {
    /// An expansion is showing — collapse it. `start` is where it was
    /// appended, `expected_len` the buffer length recorded at expand time,
    /// and `eviction_gen` the renderer's eviction counter then. The handler
    /// truncates back to `start` only if BOTH the length still matches AND
    /// the eviction counter is unchanged — so a front-eviction that shifted
    /// indices (even if the length coincidentally returns) can't truncate
    /// live content.
    Collapse {
        start: usize,
        expected_len: usize,
        eviction_gen: u64,
    },
    /// Collapsed with something to show — expand it.
    Expand,
    /// Nothing has been truncated yet — no-op.
    Nothing,
}

/// Decide the toggle action from the current anchor and whether any
/// expandable source exists.
pub(crate) fn expand_toggle(anchor: Option<(usize, usize, u64)>, has_source: bool) -> ExpandToggle {
    match anchor {
        Some((start, expected_len, eviction_gen)) => ExpandToggle::Collapse {
            start,
            expected_len,
            eviction_gen,
        },
        None if has_source => ExpandToggle::Expand,
        None => ExpandToggle::Nothing,
    }
}

/// Which block to expand when expanding. A thinking burst still streaming
/// (`live`) always wins; otherwise the most-recent target is preferred,
/// falling back to whichever of tool/thinking output is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpandSource {
    LiveThinking,
    Thinking,
    Tool,
    None,
}

pub(crate) fn select_expand_source(
    live: bool,
    target: ExpandTarget,
    has_tool: bool,
    has_thinking: bool,
) -> ExpandSource {
    if live {
        return ExpandSource::LiveThinking;
    }
    match target {
        ExpandTarget::Tool if has_tool => ExpandSource::Tool,
        ExpandTarget::Thinking if has_thinking => ExpandSource::Thinking,
        _ if has_tool => ExpandSource::Tool,
        _ if has_thinking => ExpandSource::Thinking,
        _ => ExpandSource::None,
    }
}

/// The event loop's model — single source of truth for the interactive UI.
pub(crate) struct UiState {
    // ── Agent-run lifecycle ──────────────────────────────────────────
    /// Master flag: is an agent run currently streaming?
    pub(crate) is_running: bool,
    /// Receiver for the live agent's events (`None` when idle).
    pub(crate) agent_rx: Option<mpsc::Receiver<AgentEvent>>,
    /// Join handle for the agent task, so Ctrl+C can `.abort()` it.
    pub(crate) agent_abort: Option<JoinHandle<()>>,
    /// Signal channel that wakes the runner to pick up a queued
    /// mid-execution interjection at the next tool-result boundary.
    pub(crate) agent_interject: Option<mpsc::Sender<()>>,
    /// Cooperative-cancel signal to the runner (sent on Ctrl+C before
    /// the hard `.abort()`).
    pub(crate) agent_cancel: Option<mpsc::Sender<()>>,
    /// Whether the agent has emitted a non-empty line this run.
    pub(crate) agent_line_started: bool,
    /// The most recent user prompt text (for session persistence).
    pub(crate) last_user_prompt: String,
    /// Count of ToolCall events in the current run.
    pub(crate) tool_calls_this_run: u32,
    /// Structured tool-call records, attached to the session on Done.
    pub(crate) tool_calls_buf: Vec<ToolCallEntry>,

    // ── Streaming (current turn's render-relevant text) ──────────────
    /// Accumulated assistant response text for the in-flight turn.
    pub(crate) response_buf: String,
    /// Buffer line index where the streamed response was inserted.
    pub(crate) response_start_line: Option<usize>,
    /// Accumulated reasoning/thinking text for the in-flight turn.
    pub(crate) reasoning_buf: String,
    /// Buffer line index where the streamed reasoning was inserted.
    pub(crate) reasoning_start_line: Option<usize>,
    /// Whether a thinking burst is currently in progress.
    pub(crate) was_reasoning: bool,
    /// Timestamp of the last token-stream paint (60 fps coalescing).
    pub(crate) last_token_render: Option<Instant>,

    // ── In-flight tool chamber ───────────────────────────────────────
    pub(crate) last_tool_name: Option<String>,
    pub(crate) last_tool_call_id: Option<String>,
    /// Chamber TOP painted but BOTTOM not yet drawn?
    pub(crate) tool_chamber_open: bool,
    pub(crate) chamber_top_start: Option<usize>,
    pub(crate) chamber_top_end: Option<usize>,
    /// Last truncated tool output (Ctrl+O reprints it in full).
    pub(crate) last_collapsed: Option<CollapsedToolResult>,
    /// Most recent COMPLETED thinking burst, retained after `reasoning_buf`
    /// is cleared at the turn boundary so Ctrl+O can still expand it once
    /// the response is showing (it couldn't before — `reasoning_buf` was
    /// gone).
    pub(crate) last_thinking: Option<String>,
    /// Which truncated block Ctrl+O targets — the most recent of the
    /// thinking burst (`last_thinking`) and tool output (`last_collapsed`).
    pub(crate) expand_target: ExpandTarget,
    /// Drives the expand ↔ collapse toggle. `None` when collapsed. When
    /// expanded, holds `(start, expected_len, eviction_gen)`: the buffer
    /// index where the full block was appended, the buffer length right
    /// after, and the renderer's front-eviction counter then. Collapse
    /// truncates back to `start` only if the length still matches AND the
    /// eviction counter is unchanged — so streamed output or buffer-cap
    /// eviction after expanding can't make collapse delete real content.
    pub(crate) expansion_anchor: Option<(usize, usize, u64)>,
    /// dirge #444: `true` when the current `expansion_anchor` block is showing
    /// LIVE (still-streaming) thinking, so new reasoning deltas re-render it in
    /// place instead of leaving a frozen snapshot. Cleared on collapse, on a
    /// new turn, or when the expansion targets a completed burst / tool output.
    pub(crate) live_thinking_expanded: bool,

    // ── User toggles ─────────────────────────────────────────────────
    pub(crate) show_reasoning: bool,
    pub(crate) todo_tools_enabled: bool,

    // ── Loop / phased-plan workflow ──────────────────────────────────
    /// `/loop` active label, shown in the status line (`None` = inactive).
    pub(crate) loop_label: Option<String>,
    /// In-flight `/plan` explore→plan task handle.
    pub(crate) plan_phase: Option<crate::agent::plan::runtime::PlanPhaseHandle>,
    /// Reviewer-loop state between implement turns.
    pub(crate) active_plan: Option<crate::agent::plan::runtime::ActivePlan>,
    /// In-flight non-blocking compaction (summarizer LLM on a spawned task);
    /// the `compaction_phase` select! arm installs the result. dirge-tv3p.
    pub(crate) compaction_phase: Option<crate::ui::compaction::CompactionPhaseHandle>,
    /// In-flight non-blocking `/plan` reviewer (the write-disabled reviewer runs
    /// code on a spawned task); the `review_phase` arm applies the verdict.
    /// dirge-4koy.
    pub(crate) review_phase: Option<crate::agent::plan::runtime::ReviewPhaseHandle>,
    /// In-flight non-blocking `/btw` side query (one-shot LLM on a spawned task);
    /// the `btw_phase` arm renders the answer. dirge-nret.
    pub(crate) btw_phase: Option<crate::ui::btw::BtwPhaseHandle>,
    /// In-flight non-blocking `/wt-merge` (git merge on a blocking thread); the
    /// `wt_merge_phase` arm runs the post-merge continuation. dirge-iagk.
    /// Unconditional so the select! arm can be (select! rejects `#[cfg]` arms);
    /// stays `None` in non-worktree builds.
    pub(crate) wt_merge_phase: Option<crate::ui::wt_merge_phase::WtMergePhaseHandle>,

    // ── PTY-backed interactive shell session (!cmd / !!cmd) ──────────
    /// A live PTY-backed shell session for `!cmd` / `!!cmd` (no terminal
    /// takeover — the TUI stays live). `Some` while the command runs: raw
    /// keystrokes are forwarded to the PTY while the shell box is mounted,
    /// and `Esc` `SIGKILL`s the whole process group. The session resolves on
    /// its own when the child exits.
    pub(crate) shell_session: Option<crate::ui::shell_session::ShellSession>,
    /// VT100 screen parser fed the raw (escapes-intact) PTY output, rendered
    /// into the live `[shell]` box. A real screen parser (rather than
    /// concatenating ansi-stripped text) lets cursor-moving apps like
    /// `gh auth login` redraw in place. None whenever no session is active.
    pub(crate) shell_parser: Option<vt100::Parser>,
    /// True once the bottom shell box has replaced the input box (mounted
    /// after a short grace window so quick commands never flash a box).
    pub(crate) shell_box_visible: bool,
    /// Mount deadline; `Some` from spawn until the box mounts (grace window).
    pub(crate) shell_mount_deadline: Option<std::time::Instant>,

    // ── Chats / subagents ────────────────────────────────────────────
    /// Per-chat-tab UI state (response/reasoning/chamber buffers).
    pub(crate) chat_ui_states: Vec<ChatUiState>,
    /// task_id → chat tab index.
    pub(crate) subagent_chat_map: HashMap<String, usize>,
    /// chat tab index → task_id (reverse, for Ctrl+K kill).
    pub(crate) chat_idx_to_subagent: HashMap<usize, String>,
    /// Left-panel subagent status rows: id → (state, prompt, files).
    pub(crate) subagent_panel_rows: IndexMap<String, (String, String, Vec<String>)>,
    /// Recent tool-name ticker (left panel), capped at [`TOOL_ACTIVITY_CAP`].
    pub(crate) tool_activity: VecDeque<String>,

    // ── Interjection queue (shared with the runner) ──────────────────
    /// Messages typed while the agent runs; drained at turn boundaries.
    /// `Arc<Mutex<…>>` because the runner side also reads it.
    pub(crate) interjection_queue: Arc<Mutex<VecDeque<String>>>,

    // ── Modal pickers ────────────────────────────────────────────────
    pub(crate) rewind_picker: ListPicker,
    /// Timestamp of the last Esc (double-tap detection).
    pub(crate) last_esc: Option<Instant>,

    // ── Unified input mode (#387 follow-up) ──────────────────────────
    /// What the next user input event applies to. `Compose` is the normal
    /// prompt editor; the modal variants own their reply channel + UI
    /// state, so the central event loop dispatches input to them instead
    /// of spinning a nested blocking read loop (which could park the UI).
    pub(crate) input_mode: InputMode,
}

/// The active input context — see [`UiState::input_mode`]. Each modal
/// variant owns the request's reply channel and the modal's UI state, so
/// the one `user_rx` arm can drive it event-by-event and the render effect
/// can paint it, with no nested blocking loop.
pub(crate) enum InputMode {
    /// Normal prompt editing.
    Compose,
    /// `/plan` (or `plan_enter`/`plan_exit`) confirmation: y/n.
    PlanSwitch {
        reply: tokio::sync::oneshot::Sender<crate::agent::tools::plan::PlanSwitchResponse>,
        /// Prompt name to activate on accept (`"plan"` / `"code"`).
        prompt_name: &'static str,
        /// Human label for the confirmation/result lines.
        label: &'static str,
    },
    /// `question` tool: walk the questionnaire one option-picker at a time.
    Question(QuestionState),
    /// Plugin `harness/confirm` dialog: y/n (Esc / Ctrl+C = no).
    DialogConfirm {
        reply: std::sync::mpsc::Sender<crate::plugin::DialogReply>,
    },
    /// Plugin `harness/select` dialog: pick option 1-9 (Esc / Ctrl+C = none).
    DialogSelect {
        reply: std::sync::mpsc::Sender<crate::plugin::DialogReply>,
        /// The selectable option labels, in display order.
        options: Vec<String>,
    },
    /// Tool permission prompt: y (allow once) / a (allow always) / n / Esc.
    /// The `(O_O)` alert overlay is already painted; the dispatcher reads
    /// the keystroke, replies, and runs the chamber-reopen / cascade-deny
    /// / allowlist-save post-work.
    Permission(PermissionState),
}

/// In-flight state for the tool-permission modal — replaces the former
/// nested `loop { select! { user_rx … } }`. Holds the request (tool +
/// input + reply) and the chamber that must be reopened if the user
/// allows the tool.
pub(crate) struct PermissionState {
    /// The permission request (tool, input, and the decision reply).
    pub(crate) req: crate::permission::ask::AskRequest,
    /// If a tool chamber was closed to make room for the alert, the name
    /// to reopen it under once the user allows (`None` = nothing to reopen).
    pub(crate) pending_chamber_tool: Option<String>,
}

/// In-flight state for the `question` tool modal — replaces the former
/// triple-nested blocking loop (questions → option-select → custom-text).
/// The dispatcher drives one keystroke at a time against these fields and
/// re-renders the option block in place.
pub(crate) struct QuestionState {
    /// The request (questions + the reply channel).
    pub(crate) req: crate::agent::tools::question::QuestionRequest,
    /// Confirmed answers, one inner Vec per already-answered question.
    pub(crate) answers: Vec<Vec<String>>,
    /// Index of the question currently being answered.
    pub(crate) qi: usize,
    /// Cursor row within the current question's options (`num_options`
    /// itself addresses the "(custom)" row when the question allows it).
    pub(crate) cursor: usize,
    /// Per-option toggle state (multi-select).
    pub(crate) selected: Vec<bool>,
    /// Pending custom answer for the current question, if typed.
    pub(crate) custom_text: Option<String>,
    /// Buffer line index where the current question's option block starts
    /// (so re-renders `replace_from` here on every keystroke).
    pub(crate) anchor: usize,
    /// `Some` while the user is typing a free-form custom answer.
    pub(crate) entry: Option<CustomEntry>,
}

/// Free-form custom-answer text entry, the innermost former loop.
pub(crate) struct CustomEntry {
    /// Text typed so far.
    pub(crate) buf: String,
    /// Buffer line index where the typed answer renders.
    pub(crate) input_anchor: usize,
}

impl CustomEntry {
    /// Append pasted text to the single-line answer buffer. The typed path
    /// only ever pushes printable `Char`s (Enter submits), so a paste must
    /// match that shape: whitespace controls (`\n`/`\r`/`\t`) flatten to a
    /// space and other control bytes are dropped. Without this, a paste
    /// while answering leaks into the main compose input (dirge-7543).
    pub(crate) fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for c in normalized.chars() {
            match c {
                '\n' | '\t' => self.buf.push(' '),
                c if c.is_control() => {}
                c => self.buf.push(c),
            }
        }
    }
}

/// Copy discriminant of [`InputMode`]. The dispatcher routes on this so it
/// can read the active mode without holding a borrow on `input_mode` (which
/// would block the `mem::replace` used to take ownership of the reply
/// channel on resolution).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKind {
    Compose,
    PlanSwitch,
    Question,
    DialogConfirm,
    DialogSelect,
    Permission,
}

impl InputMode {
    /// True when a modal input mode is active (not normal compose).
    pub(crate) fn is_modal(&self) -> bool {
        !matches!(self, InputMode::Compose)
    }

    /// The Copy discriminant — see [`ModalKind`].
    pub(crate) fn kind(&self) -> ModalKind {
        match self {
            InputMode::Compose => ModalKind::Compose,
            InputMode::PlanSwitch { .. } => ModalKind::PlanSwitch,
            InputMode::Question(_) => ModalKind::Question,
            InputMode::DialogConfirm { .. } => ModalKind::DialogConfirm,
            InputMode::DialogSelect { .. } => ModalKind::DialogSelect,
            InputMode::Permission(_) => ModalKind::Permission,
        }
    }
}

impl UiState {
    /// Build the initial model for a fresh interactive session.
    pub(crate) fn new() -> Self {
        Self {
            is_running: false,
            agent_rx: None,
            agent_abort: None,
            agent_interject: None,
            agent_cancel: None,
            agent_line_started: false,
            last_user_prompt: String::new(),
            tool_calls_this_run: 0,
            tool_calls_buf: Vec::new(),

            response_buf: String::new(),
            response_start_line: None,
            reasoning_buf: String::new(),
            reasoning_start_line: None,
            was_reasoning: false,
            last_token_render: None,

            last_tool_name: None,
            last_tool_call_id: None,
            tool_chamber_open: false,
            chamber_top_start: None,
            chamber_top_end: None,
            last_collapsed: None,
            last_thinking: None,
            expand_target: ExpandTarget::None,
            expansion_anchor: None,
            live_thinking_expanded: false,

            show_reasoning: false,
            todo_tools_enabled: false,

            loop_label: None,
            plan_phase: None,
            compaction_phase: None,
            review_phase: None,
            btw_phase: None,
            wt_merge_phase: None,
            shell_session: None,
            shell_parser: None,
            shell_box_visible: false,
            shell_mount_deadline: None,
            active_plan: None,

            chat_ui_states: vec![ChatUiState::empty()],
            subagent_chat_map: HashMap::new(),
            chat_idx_to_subagent: HashMap::new(),
            subagent_panel_rows: IndexMap::new(),
            tool_activity: VecDeque::with_capacity(TOOL_ACTIVITY_CAP),

            interjection_queue: Arc::new(Mutex::new(VecDeque::new())),

            rewind_picker: ListPicker::new(),
            last_esc: None,

            input_mode: InputMode::Compose,
        }
    }

    /// Current pending-interjection count (for the status line). Takes the
    /// lock briefly; ignores poisoning.
    pub(crate) fn interjection_len(&self) -> usize {
        self.interjection_queue.lock().map(|q| q.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_expands_when_collapsed_with_a_source() {
        assert_eq!(expand_toggle(None, true), ExpandToggle::Expand);
    }

    /// dirge-7543: pasting into a Q&A custom answer appends to the entry
    /// buffer (single line) rather than leaking into the main compose input.
    #[test]
    fn custom_entry_paste_appends_and_flattens_whitespace() {
        let mut e = CustomEntry {
            buf: "ab".to_string(),
            input_anchor: 0,
        };
        e.paste("cd\r\nef\tgh");
        assert_eq!(e.buf, "abcd ef gh");
    }

    #[test]
    fn custom_entry_paste_drops_non_whitespace_control_bytes() {
        let mut e = CustomEntry {
            buf: String::new(),
            input_anchor: 0,
        };
        // \u{1} (PASTE_MARK-class), \u{7} bell — dropped; printable kept.
        e.paste("x\u{1}y\u{7}z");
        assert_eq!(e.buf, "xyz");
    }

    #[test]
    fn toggle_is_noop_when_nothing_truncated() {
        assert_eq!(expand_toggle(None, false), ExpandToggle::Nothing);
    }

    #[test]
    fn toggle_collapses_when_shown() {
        assert_eq!(
            expand_toggle(Some((10, 18, 3)), true),
            ExpandToggle::Collapse {
                start: 10,
                expected_len: 18,
                eviction_gen: 3,
            }
        );
    }

    #[test]
    fn live_thinking_always_wins() {
        assert_eq!(
            select_expand_source(true, ExpandTarget::Tool, true, true),
            ExpandSource::LiveThinking
        );
    }

    #[test]
    fn source_prefers_target_then_falls_back() {
        // Honor the most-recent target when it's available.
        assert_eq!(
            select_expand_source(false, ExpandTarget::Tool, true, true),
            ExpandSource::Tool
        );
        assert_eq!(
            select_expand_source(false, ExpandTarget::Thinking, true, true),
            ExpandSource::Thinking
        );
        // Target says Tool but there's no tool output → fall back to thinking.
        assert_eq!(
            select_expand_source(false, ExpandTarget::Tool, false, true),
            ExpandSource::Thinking
        );
        // No target set → prefer tool when present.
        assert_eq!(
            select_expand_source(false, ExpandTarget::None, true, false),
            ExpandSource::Tool
        );
        // Nothing available.
        assert_eq!(
            select_expand_source(false, ExpandTarget::None, false, false),
            ExpandSource::None
        );
    }
}
