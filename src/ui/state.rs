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

            show_reasoning: false,
            todo_tools_enabled: false,

            loop_label: None,
            plan_phase: None,
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
