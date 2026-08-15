//! `run_loop`, `run_agent_loop`, `run_agent_loop_continue` —
//! THE KEYSTONE.
//!
//! Faithful port of pi's `runLoop` (agent-loop.ts:155-269) plus
//! the two public entry points `runAgentLoop` (95-118) and
//! `runAgentLoopContinue` (120-143).
//!
//! Pi's algorithm in one pass (the bones we replicate):
//!
//! ```text
//! runLoop(currentContext, newMessages, config, signal, emit, streamFn):
//!   first_turn = true
//!   pending_messages = getSteeringMessages?() || []
//!
//!   OUTER:
//!     has_more_tool_calls = true
//!     INNER while has_more_tool_calls OR pending_messages not empty:
//!       if !first_turn: emit turn_start; else first_turn = false
//!       inject pending_messages into context + newMessages; emit
//!         message_start + message_end for each
//!       msg = streamAssistantResponse(...)
//!       newMessages.push(msg)
//!       if msg.stopReason in [error, aborted]:
//!         emit turn_end (toolResults=[]); emit agent_end; return
//!       tool_calls = filter msg.content for type=toolCall
//!       tool_results = []; has_more_tool_calls = false
//!       if tool_calls non-empty:
//!         batch = executeToolCalls(...)
//!         tool_results = batch.messages
//!         has_more_tool_calls = !batch.terminate
//!         push each tool_result to context + newMessages
//!       emit turn_end (msg, tool_results)
//!       snapshot = prepareNextTurn?(ctx)
//!       if snapshot: context = ?? newCtx, model = ?? newModel, ...
//!       if shouldStopAfterTurn?(ctx): emit agent_end; return
//!       pending_messages = getSteeringMessages?() || []
//!     // INNER end
//!     follow_up = getFollowUpMessages?() || []
//!     if follow_up non-empty: pending_messages = follow_up; continue OUTER
//!     break OUTER
//!   emit agent_end
//! ```

use serde_json::Value;
use tokio::sync::mpsc;

use super::context_manager::{self, PostUsageDecisionKind};
use super::gate_state::{GateInputs, GateStates};
use super::gate_tally::{BoundaryNudge, GateSource, GateTally};
use super::inflight::InflightSet;
use super::message::{
    AssistantMessage, ContentBlock, LoopEvent, LoopMessage, StopReason, TokenUsage,
    ToolResultMessage, loop_message_to_value, tool_result_to_value,
};
use super::storm::StormBreaker;
use super::stream::{StreamFn, stream_assistant_response};
use super::tool::AbortSignal;
use super::types::{CodeReviewMode, Context, GateMode, LoopConfig};
use super::verifier::VERIFY_TAG;
use crate::sync_util::LockExt;

/// Poll the user's steering queue. USER INPUT ONLY.
///
/// Until dirge-5mtx.2 this also produced the file-touch reminder and the
/// safe-state / recovery checkpoint, which is how those three came to stack
/// with the three nudges emitted earlier in the same iteration. They are
/// harness nudges and now go through [`poll_boundary_nudge`], which picks at
/// most one. Steering is not a nudge — it is the human talking — so it keeps
/// its own path and is never suppressed by an arbiter.
///
/// The bool reports whether anything came from genuine user steering; it
/// drives the turn-budget reset (dirge-st8r). Ambient harness reminders never
/// reset the budget, which is why they must not flow through here.
async fn poll_steering(config: &LoopConfig) -> (Vec<LoopMessage>, bool) {
    let out = match &config.get_steering_messages {
        Some(get) => get().await,
        None => Vec::new(),
    };
    let had_user_steering = !out.is_empty();
    (out, had_user_steering)
}

/// The repo the safe-state coverage check runs against (dirge-uw2l.6).
///
/// `code_review_repo` is an explicit override that only tests set; in
/// production it is `None` and the intended root is the process CWD — the
/// same fallback the code-review diff capture uses (run.rs, dirge-9b2k).
/// Reading the field directly without this fallback would leave auto with
/// no repo in every real session, so it would silently decline forever and
/// look like a feature that simply never fires.
fn safe_state_repo(config: &LoopConfig) -> Option<std::path::PathBuf> {
    config
        .code_review_repo
        .clone()
        .or_else(|| std::env::current_dir().ok())
}

/// Restore the tree to its last verified-green state — but ONLY after
/// proving the snapshot store can put back everything that changed
/// (dirge-uw2l.6). Returns the number of files restored, or `None` when it
/// declined and the caller should fall back to the advisory wording.
///
/// This is the one function in the safe-state rung that writes to the user's
/// files, so every precondition is a hard decline rather than a best effort:
///
/// - no repo, or not a git work tree → decline (no ground truth to check)
/// - no green fingerprint → decline (nothing to diff against)
/// - a file changed since green that the store never captured → decline
///
/// That last case is the whole reason auto was deferred in R3.
/// `snapshots::capture` is wired into the edit tools and not into `bash`, so
/// a `sed -i`, a `>` redirect or an in-place formatter mutates a file with
/// no pre-state recorded. Restoring the captured edits while leaving those
/// alone yields a tree in a state that never existed — likely not
/// compiling — which is strictly worse than the broken tree we started
/// from, and arrived at behind the model's back. Detecting it via git and
/// declining is what makes auto safe to ship at all.
fn coverage_verified_restore(
    repo: Option<&std::path::Path>,
    green_fp: Option<&super::worktree_probe::TreeFingerprint>,
    green_turn: &str,
) -> Option<usize> {
    let repo = repo?;
    let green_fp = green_fp?;
    let now = super::worktree_probe::fingerprint(repo)?;
    let mutated = super::worktree_probe::changed_between(green_fp, &now);
    if mutated.is_empty() {
        // Nothing changed since green — there is nothing to restore, and
        // claiming a restore would be a lie.
        return None;
    }
    let restorable = crate::agent::tools::snapshots::restorable_paths_after(green_turn);
    if !super::worktree_probe::coverage_is_complete(&mutated, &restorable) {
        tracing::info!(
            target: "dirge::loop",
            mutated = mutated.len(),
            restorable = restorable.len(),
            "safe-state auto declined: snapshot coverage incomplete \
             (a file changed that the store never captured — likely a bash \
             write); falling back to advisory"
        );
        return None;
    }
    let restored = crate::agent::tools::snapshots::restore_after_green_turn(green_turn);
    if restored.is_empty() {
        return None;
    }
    tracing::info!(
        target: "dirge::loop",
        files = restored.len(),
        "safe-state auto restored the tree to its last verified-green state"
    );
    Some(restored.len())
}

/// The harness tag `text` carries, if any. Delegates to the registry in
/// [`super::intervention`], which both this mirror and the TUI's attribution
/// read so the two can't drift apart (dirge-x4se).
fn harness_tag_of(text: &str) -> Option<&'static str> {
    super::intervention::tag_of(text)
}

/// Mirror harness-injected messages to a `SystemNotice` (dirge-uw2l.7).
///
/// These are injected as USER-role messages so the model acts on them, and
/// the TUI renders them under a system handle by matching their tag. Headless
/// consumers had no equivalent: `--print` shows only the final answer, and
/// the stream-json `user` event carries tool results only. So a `--print` or
/// `--loop` user saw the model abruptly change course — run a check, re-plan,
/// abandon an approach — with nothing explaining why, and no way to tell an
/// injected steer from the model's own judgement.
///
/// Emitting a notice costs nothing when no tag matches (an ordinary user
/// message or steering text mirrors nothing), so this is additive.
///
/// dirge-x4se: the notice leads with a short human summary of what the harness
/// did, then the model-facing body. The body alone is an imperative addressed to
/// the model — a human watching a headless run wants to know why the run
/// changed course, which is a different sentence than the one the model needs.
async fn emit_harness_notices(emit: &mpsc::Sender<LoopEvent>, msgs: &[LoopMessage]) {
    for m in msgs {
        let LoopMessage::User(u) = m else { continue };
        let text = u.text_joined();
        let Some(tag) = harness_tag_of(&text) else {
            continue;
        };
        let body = super::intervention::strip_tag(&text).unwrap_or(&text);
        let _ = emit
            .send(LoopEvent::SystemNotice {
                content: format!(
                    "harness intervention: {}\n{body}",
                    super::intervention::summary_for_user(tag)
                ),
            })
            .await;
    }
}

/// Joined text of a tool result's content blocks — fed to the failure
/// tracker as the error excerpt quoted back in a recovery checkpoint.
fn tool_result_excerpt(content: &[super::message::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            super::message::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a `StormBreaker` from `LoopConfig`, merging custom
/// mutating/exempt tool name lists with the built-in defaults.
// The two `Option<Box<dyn Fn ...>>` predicates match `StormBreaker::new`
// exactly; aliasing once here would only force readers to jump to find
// the same shape they'd otherwise read inline. Silence locally.
#[allow(clippy::type_complexity)]
fn storm_for_config(config: &LoopConfig) -> StormBreaker {
    let has_custom = config.storm_mutating_tools.is_some() || config.storm_exempt_tools.is_some();
    if !has_custom {
        return StormBreaker::default();
    }
    let mutating: Option<Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>> =
        config.storm_mutating_tools.as_ref().map(|extras| {
            let extra_set: std::collections::HashSet<String> = extras.iter().cloned().collect();
            Box::new(move |c: &super::tools::ToolCall| {
                super::storm::default_mutating(c) || extra_set.contains(&c.name)
            }) as Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>
        });
    let exempt: Option<Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>> =
        config.storm_exempt_tools.as_ref().map(|extras| {
            let extra_set: std::collections::HashSet<String> = extras.iter().cloned().collect();
            Box::new(move |c: &super::tools::ToolCall| {
                super::storm::default_exempt(c) || extra_set.contains(&c.name)
            }) as Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>
        });
    StormBreaker::new(6, 3, mutating, exempt)
}

/// Upper bound on consecutive unfinished-todo nudges, so a deliberately
/// abandoned todo list can't trap the loop in an endless "finish your todos"
/// cycle.
const MAX_TODO_NUDGES: u8 = 3;

/// Upper bound on consecutive open-issues nudges, so the agent can't loop
/// forever if it can't or won't close the remaining issues.
const MAX_OPEN_ISSUES_NUDGES: u8 = 2;

/// One-shot: fire at most once per run when the model edits files but has
/// no active todo — a gap the normal unfinished-todo nudge can't cover.
const MAX_TRACK_NUDGES: u8 = 1;

/// Judge calls the one-shot critic may ATTEMPT per run (dirge-2m68).
///
/// The one shot is spent by a verdict, not by an attempt: a judge that timed
/// out or errored fails open, and letting that consume the shot deleted the
/// completeness backstop for the rest of the run exactly when the provider was
/// unhealthy. So a failed attempt is retried at the next finalization — and
/// this is the ceiling that keeps a persistently broken judge from being
/// retried at every one. Three, matching `MAX_REVIEW_REACT`: a judge that has
/// failed three times in one run is not going to answer on the fourth, and
/// each attempt is a real LLM call.
const MAX_CRITIC_JUDGE_ATTEMPTS: u32 = 3;

/// One-shot: fire at most once per run when code edits pile up with no
/// verification since (dirge-uw2l.2). Bounded to a single message so the
/// mid-run reminder can never become nagging.
const MAX_VERIFY_NUDGES: u8 = 1;

/// Code edits since the last verification before the mid-run fast-check
/// reminder fires. One or two edits may be mid-sequence; three-plus with
/// nothing run is integrating without testing — the pattern RAX's
/// front-line-tester finding says to interrupt early.
const FAST_VERIFY_EDIT_THRESHOLD: u32 = 3;

/// Consecutive errored tool results before the failure tracker injects a
/// recovery checkpoint. Tuned low — the tool-repair literature finds the
/// gains from corrective reflection concentrate over the first few
/// attempts (dirge-opdt).
const FAILURE_REFLECTION_THRESHOLD: usize = 3;

/// Which finalization gate produced the interjection this turn. The loop
/// injects at most ONE follow-up per finalization, chosen in strict priority
/// order — see [`poll_finalization_follow_up`]. Centralizing the precedence
/// into a single enum + function replaced four scattered
/// `if follow_up.is_empty()` blocks that each implicitly encoded their rank
/// [dirge-vcsn].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowUpSource {
    /// The model ended its turn blocked on the user — it asked a clarifying
    /// question and is awaiting their decision. Finalize and hand control
    /// back; do NOT run the lower "are we done?" gates (they'd re-enter until
    /// the model gives up waiting and guesses). [dirge-g2ex]
    AwaitingUser,
    /// Caller-supplied `get_followup_messages` hook (e.g. the `/plan`
    /// reviewer loop). Highest priority among the work-gates.
    Hook,
    /// Deterministic resume: the model's last action was a failed tool call
    /// and it stopped without retrying. Cheap, no LLM call, always-on.
    ResumeAfterFailure,
    /// Verifier gate: code was edited but nothing was run to check it.
    Verifier,
    /// Deterministic claim/evidence gate (dirge-d0e5.2): the final answer
    /// claimed a verification result or a change the run's evidence does not
    /// support. No LLM call.
    ClaimGate,
    /// Deterministic artifact-scope sourcing gate (dirge-lavc GAP 1): an
    /// added comment in the run's diff asserted consulting an external
    /// source while no fetch/search tool ran. No LLM call. Off by default.
    SourceGate,
    /// Deterministic completeness gate (dirge-2m68): the final answer stated
    /// first-person work the model still intended to do, while the run was
    /// finalizing and nothing else had anything to say. No LLM call. The
    /// last-resort backstop, so it sits below every other gate.
    CompletenessGate,
    /// Unified finalization judge (dirge-8v98): completeness verdict + diff
    /// findings in one call. One-shot (Off/Advisory) or persistent up to
    /// [`super::code_review::MAX_REVIEW_REACT`] (Blocking).
    Critic,
    /// Goal gate: user-defined stop condition not yet met. Re-enters the
    /// loop, bounded by [`super::goal::MAX_GOAL_REACT`].
    Goal,
    /// Unfinished-todo nudge (bounded by [`MAX_TODO_NUDGES`]).
    Todo,
    /// Open-issues nudge — this session left tracked issues open
    /// (bounded by [`MAX_OPEN_ISSUES_NUDGES`]).
    OpenIssues,
    /// No gate fired — the run may finalize.
    None,
}

impl From<FollowUpSource> for GateSource {
    fn from(source: FollowUpSource) -> Self {
        match source {
            FollowUpSource::AwaitingUser => GateSource::AwaitingUser,
            FollowUpSource::Hook => GateSource::Hook,
            FollowUpSource::ResumeAfterFailure => GateSource::ResumeAfterFailure,
            FollowUpSource::Verifier => GateSource::Verifier,
            FollowUpSource::ClaimGate => GateSource::ClaimGate,
            FollowUpSource::SourceGate => GateSource::SourceGate,
            FollowUpSource::CompletenessGate => GateSource::CompletenessGate,
            FollowUpSource::Critic => GateSource::Critic,
            FollowUpSource::Goal => GateSource::Goal,
            FollowUpSource::Todo => GateSource::Todo,
            FollowUpSource::OpenIssues => GateSource::OpenIssues,
            FollowUpSource::None => GateSource::None,
        }
    }
}

/// Display tag prefixing the unfinished-todo nudge. The UI keys on this to
/// attribute the message to the system/critic rather than the user — it's
/// injected as a user-role message so the model responds, but it isn't user
/// input [dirge-i75f].
pub(crate) const TODO_NUDGE_TAG: &str = "[todo]";

/// Display tag prefixing the open-issues nudge, so the UI can strip it and
/// attribute the injected message to the system rather than the user.
pub(crate) const OPEN_ISSUES_NUDGE_TAG: &str = "[open-issues]";

/// Display tag prefixing the resume-after-failure nudge, so the UI can strip
/// it and attribute the injected message to the system rather than the user.
pub(crate) const RESUME_NUDGE_TAG: &str = "[resume]";

/// Display tag prefixing the early track-work reminder, so the UI can strip
/// it and attribute the injected message to the system rather than the user.
pub(crate) const TRACK_WORK_TAG: &str = "[track]";

/// Upper bound on consecutive resume-after-failure nudges, so a model that
/// repeatedly stops after broken tool calls can't loop forever.
const MAX_RESUME_NUDGE: u8 = 3;

/// Run-level recovery nudge reinjected (bounded by
/// [`MAX_TRANSIENT_RECOVERIES`]) when a transient mid-stream error
/// ("error decoding response body", network blip, rate-limit) kills an
/// assistant turn AFTER content has already streamed. The streaming
/// retry layer can't replay the turn (the partial is already on
/// screen), so the run loop recovers instead: the preserved partial
/// stays in the transcript and this nudge tells the model to continue
/// rather than restart from scratch.
const TRANSIENT_RECOVERY_NUDGE: &str = "Your previous response was cut off by a transient connection error before it finished. Continue from where you left off — do not repeat what you already said.";

/// Upper bound on consecutive transient-error recoveries, so a
/// genuinely dead network can't loop the run forever. Past this the
/// error surfaces as terminal (the run ends, as it did before recovery).
const MAX_TRANSIENT_RECOVERIES: u8 = 3;

/// Stable prefix of the max-agent-turns truncation notice. The
/// headless result path (`provider::run`) matches on this to mark the
/// run truncated in its JSON envelope (dirge-18v2) — sharing the
/// constant keeps emitter and detector from drifting.
pub(crate) const MAX_TURNS_NOTICE_PREFIX: &str = "[dirge] Max agent turns";

/// Build the max-turns truncation notice, appending the residual-objectives
/// block (dirge-uw2l.5) when the live board has outstanding work. Pure: takes
/// the board so the prefix-survival property — the headless detector in
/// `provider::run` matches `content.starts_with(MAX_TURNS_NOTICE_PREFIX)` — is
/// unit-testable (see `max_turns_notice_keeps_truncation_prefix`). Empty board
/// → no block → byte-identical to the old notice.
fn max_turns_notice(cap: usize, board: &[crate::agent::tools::todo::TodoItem]) -> String {
    let mut notice = format!(
        "{MAX_TURNS_NOTICE_PREFIX} ({cap}) reached. Stopping the run. Increase --max-agent-turns or `max_agent_turns` in config.json to allow more."
    );
    if let Some(block) = super::residual::residual_block(board) {
        notice.push_str("\n\n");
        notice.push_str(&block);
    }
    notice
}

/// The unfinished-todo nudge message. Pure (no globals) so the singular/plural
/// wording is unit-testable independent of the todo store.
///
/// dirge-uw2l.5: when at least one outstanding item is low-priority the nudge
/// names them as the explicit cancel candidates. RAX's planner treated
/// rejecting a low-priority unachievable goal as a validation objective, not a
/// failure (paper §3.1b), and the todo store already carries priority that
/// nothing surfaced back. When `low == 0` the message is byte-identical to the
/// pre-uw2l.5 wording, so the common case changes nothing.
fn todo_nudge_message(unfinished: usize, low: usize) -> LoopMessage {
    let mut body = format!(
        "{TODO_NUDGE_TAG} You still have {unfinished} unfinished todo{} (pending or in progress). \
         Finish the remaining work, or if it's genuinely done or no longer needed, \
         update the todo list (mark items completed/cancelled) before stopping.",
        if unfinished == 1 { "" } else { "s" }
    );
    if low > 0 {
        body.push_str(&format!(
            " You have {low} low-priority item{} left — if one won't fit, cancel it with a one-line reason rather than leaving it open.",
            if low == 1 { "" } else { "s" }
        ));
    }
    LoopMessage::User(super::message::UserMessage::text(body))
}

/// The plan-only variant of the nudge (dirge-u1ay): the turn wrote a todo
/// list and touched no files. Deliberately worded away from list maintenance
/// — the failure it catches is a model that answers "do the work" with
/// another `write_todo_list` call, so re-reading this must not read as an
/// invitation to restate the plan.
fn plan_only_nudge_message(unfinished: usize) -> LoopMessage {
    LoopMessage::User(super::message::UserMessage::text(format!(
        "{TODO_NUDGE_TAG} You planned {unfinished} item{} this turn but changed no files. \
         The plan is not the work — start the first item now with `write` / `edit` \
         (or `bash` if it's a command). Do not call write_todo_list again until \
         something is actually done.",
        if unfinished == 1 { "" } else { "s" }
    )))
}

/// True when the model's most recent action was a FAILED tool call and it
/// then stopped: the tail of the run is a contiguous group of ToolResult
/// messages containing at least one `is_error == true`, immediately
/// followed by a final Assistant turn that made NO tool calls. Returns
/// false otherwise. This is deliberately narrow so it only fires on a
/// definitive failure-stop and CANNOT loop: once the model replies to the
/// nudge with text (no new tool call), the error group is no longer
/// immediately before the final Assistant turn, so it stops matching.
fn last_action_failed_and_stopped(new_messages: &[LoopMessage]) -> bool {
    if new_messages.is_empty() {
        return false;
    }
    // The LAST message must be an Assistant with NO tool calls.
    let Some(LoopMessage::Assistant(last)) = new_messages.last() else {
        return false;
    };
    if !extract_tool_calls_from(last).is_empty() {
        return false;
    }
    // Walk backwards from the message before the final Assistant,
    // collecting the contiguous run of ToolResult messages.
    let mut error_tail = false;
    for msg in new_messages[..new_messages.len() - 1].iter().rev() {
        match msg {
            LoopMessage::ToolResult(tr) => {
                if is_retryable_failure(tr) {
                    error_tail = true;
                }
            }
            _ => break,
        }
    }
    error_tail
}

/// A failed tool result the model could plausibly fix by retrying. Excludes
/// permission/approval refusals (Outcome::Denied — only the user can unblock)
/// and storm-breaker backfill stubs (already "do not repeat"), so the
/// resume-after-failure nudge never re-issues a denied or suppressed call
/// (dirge-g3xv).
fn is_retryable_failure(tr: &super::message::ToolResultMessage) -> bool {
    if !tr.is_error {
        return false;
    }
    let excerpt = tool_result_excerpt(&tr.content);
    if super::activity::Outcome::classify(true, &excerpt) == super::activity::Outcome::Denied {
        return false;
    }
    if excerpt.contains(super::tools::SUPPRESSED_CALL_NOTE) {
        return false;
    }
    true
}

/// Goal gate (dirge-g2ex): the user-defined stop-condition judge, extracted
/// verbatim from its old inline position in [`poll_finalization_follow_up`] so
/// the step-0 awaiting-user gate can reuse it without duplicating the
/// [`super::goal::MAX_GOAL_REACT`] bound or the `*goal_reacts` accounting.
///
/// Returns `Some((msgs, FollowUpSource::Goal))` when the goal is unmet (the
/// judge surfaced a reason to re-enter), `None` otherwise (no goal armed, goal
/// met, budget exhausted, or judge error). Pure refactor of the step-3.5 block:
/// same conditions, same react-counting, same source.
async fn poll_goal_gate(
    config: &LoopConfig,
    system_prompt: &str,
    new_messages: &[LoopMessage],
    goal_reacts: &mut u8,
) -> Option<(Vec<LoopMessage>, FollowUpSource)> {
    if *goal_reacts < super::goal::MAX_GOAL_REACT
        && let Some(goal) = &config.goal
        && let Some(judge) = &config.goal_fn
    {
        let transcript = build_critic_transcript(new_messages);
        // dirge-6q3w: same read-only verification signal as the critic, but
        // the goal judge treats it as a SOFT advisory (see
        // `goal_verification_note`) so a non-testable task can't trap the
        // bounded goal loop.
        let verification = config
            .verifier
            .as_ref()
            .map(|v| v.status(config.verification_tiers_mode));
        let msgs =
            super::goal::run_goal_gate(judge, goal, system_prompt, &transcript, verification).await;
        if !msgs.is_empty() {
            *goal_reacts += 1;
            return Some((msgs, FollowUpSource::Goal));
        }
    }
    None
}

/// dirge-5mtx.4: is the run BLOCKED on the user, or merely OFFERING more work?
///
/// Gate 0 of [`poll_finalization_follow_up`] finalizes immediately when this is
/// true, skipping the verifier, critic, todo and open-issues gates. So a false
/// positive silently disables the whole finalization stack — and skipping a gate
/// produces no output, so nothing in the transcript shows it happened.
///
/// [`awaiting_user_response`] answers it by testing whether the last meaningful
/// line ends in `?`. That cannot separate "which database should I use?" (the
/// run genuinely cannot proceed) from "I've added the parser — want me to wire
/// it in too?" (the work is done and is exactly what the gates exist to check).
/// Measured, it reads five out of five completed-work offers as blocked; see
/// `awaiting_user_corpus_known_misclassifications`.
///
/// Three tiers, cheapest first:
///   1. No `?` anywhere in the final text → not blocked. Costs nothing and is
///      the overwhelmingly common case, so the judge is never called for it.
///   2. No classifier configured → the heuristic, exactly as before.
///   3. Otherwise ask the judge to choose between two non-overlapping words.
///      A classifier error falls back to the heuristic rather than failing the
///      turn: the gate has to answer something, and the pre-fix behaviour is
///      the right thing to answer when the better signal is unavailable.
pub(crate) async fn is_awaiting_user(config: &LoopConfig, new_messages: &[LoopMessage]) -> bool {
    let Some(LoopMessage::Assistant(last)) = new_messages.last() else {
        return false;
    };
    let joined: String = last
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    // 1. Cheap structural pre-filter. A turn with no question mark at all is
    //    not waiting on anyone, and this is most turns.
    if !joined.contains('?') && !joined.contains('\u{ff1f}') {
        return false;
    }
    // 2. No judge armed — heuristic, unchanged.
    let Some(classify) = config.classify_fn.as_ref() else {
        return awaiting_user_response(new_messages);
    };
    // 3. Ask. Options are deliberately non-overlapping words: neither is a
    //    substring or whole-word match of the other (dirge-5mtx.3).
    const OPTIONS: &[&str] = &["BLOCKED", "OFFERING"];
    let question = format!(
        "Here is the final message a coding agent sent at the end of its turn:\n\n         ---\n{joined}\n---\n\n         Did it STOP because it needs a decision from the user before it can          continue (BLOCKED), or had it finished the work and was offering to do          more / asking a rhetorical or courtesy question (OFFERING)?"
    );
    match classify(question, OPTIONS).await {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => {
            tracing::debug!(
                target: "dirge::loop",
                error = %e,
                "awaiting-user classifier failed; falling back to the heuristic"
            );
            awaiting_user_response(new_messages)
        }
    }
}

/// Poll the finalization gates in strict priority order and return the first
/// non-empty source's messages (plus which source fired, for tracing/tests).
///
/// At most ONE source contributes per finalization. The lower-priority gates
/// (verifier, critic, code-review, goal, todo) are each one-shot or bounded, so deferring one by
/// a turn is intentional: e.g. a red build surfaces the verifier nudge now and
/// the critic runs at the *next* finalization once the build is fixed (the
/// verifier won't fire twice). This is the single authority for finalization
/// precedence — previously four separate `if follow_up.is_empty()` blocks
/// inline in the outer loop [dirge-vcsn].
///
/// Every gate's re-fire state lives in [`GateStates`], and every field there is
/// labelled cost-ceiling or re-fire-guard — see that type for why the
/// distinction decides what is safe to change [dirge-5mtx.5].
async fn poll_finalization_follow_up(
    config: &LoopConfig,
    system_prompt: &str,
    new_messages: &[LoopMessage],
    gates: &mut GateStates,
    inputs: GateInputs<'_>,
    // dirge-1elu.4: all follow-up delivery now flows through the returned
    // messages — the loop mirrors them to SystemNotice with
    // emit_harness_notices — so no gate emits on this channel anymore.
    _emit: &mpsc::Sender<LoopEvent>,
) -> (Vec<LoopMessage>, FollowUpSource) {
    // Destructured rather than accessed through `gates.` / `inputs.` so the
    // gate bodies below read exactly as they did when these were seventeen
    // positional parameters. Default binding modes give each name the same
    // `&mut T` / `T` it had before, so this collapse is a signature change
    // only — no gate logic moved.
    let GateStates {
        critic_done,
        critic_attempts,
        code_review_reacts,
        last_reviewed_fingerprint,
        last_review_findings,
        goal_reacts,
        todo_nudges,
        resume_nudges,
        open_issues_nudges,
        track_nudges,
        claim_nudges,
        source_nudges,
        completeness_nudges,
        run_epoch,
    } = gates;
    let GateInputs {
        code_review_baseline,
        open_issues_gate_mode,
        issue_db_path,
        session_id,
    } = inputs;

    // 0. Awaiting-user gate (dirge-g2ex). The model ended its final assistant
    //    turn by asking the user a question — it is blocked on the USER, not
    //    "done". Finalize and hand control back instead of letting the lower
    //    "are we done?" gates (critic, todo, open-issues) re-enter until the
    //    model gives up waiting and guesses.
    //
    //    This sits ABOVE the caller hook deliberately: a pending question
    //    outranks even the hook. Skipping the hook loses nothing —
    //    `followup_from_background_store` (src/agent/tools/background.rs:898)
    //    and `get_followup_messages_from_plugin_manager`
    //    (src/agent/agent_loop/plugin_hooks.rs:409) leave their queues intact
    //    when not polled, and background completions still surface on the next
    //    user prompt via `prepend_pending_notifications` (integration.rs:474-481).
    //
    //    The ONE exception is the goal gate: `--goal` is an explicit autonomous
    //    stop condition, so it must keep pushing the run forward even with a
    //    question pending. A still-running coordinator generation keeps its
    //    existing meaning too — `should_defer_finalization` defers as before.
    if is_awaiting_user(config, new_messages).await {
        // (a) A coordinator generation still running defers the whole decision.
        if config
            .should_defer_finalization
            .as_ref()
            .is_some_and(|should_defer| should_defer())
        {
            return (Vec::new(), FollowUpSource::None);
        }
        // (b) The goal gate stays authoritative for autonomous runs.
        if let Some(hit) = poll_goal_gate(config, system_prompt, new_messages, goal_reacts).await {
            return hit;
        }
        // (c) Finalize and hand control back to the user.
        tracing::debug!(
            target: "dirge::loop",
            "final assistant turn ended with a question to the user; \
             finalizing without running the critic/todo/open-issues gates"
        );
        return (Vec::new(), FollowUpSource::AwaitingUser);
    }

    // 1. Caller hook (pi lines 256-262) — highest priority.
    if let Some(get) = &config.get_followup_messages {
        let msgs = get().await;
        if !msgs.is_empty() {
            return (msgs, FollowUpSource::Hook);
        }
    }

    // Coordinator work may intentionally outlive this parent turn. The
    // completion hook above gets first chance to deliver a batch that became
    // terminal at the boundary; only a still-running generation defers the
    // critic and every lower "are we done?" gate. The UI wakes the parent when
    // the batch becomes deliverable, so returning no follow-up here suspends
    // cleanly without polling or consuming one-shot gate budgets.
    if config
        .should_defer_finalization
        .as_ref()
        .is_some_and(|should_defer| should_defer())
    {
        return (Vec::new(), FollowUpSource::None);
    }

    // 1.5 Deterministic resume-after-failure: fires when the model's last
    //     action was a failed tool call and it stopped without retrying.
    //     Cheap, no LLM call, always-on. Bounded by MAX_RESUME_NUDGE.
    if *resume_nudges < MAX_RESUME_NUDGE && last_action_failed_and_stopped(new_messages) {
        *resume_nudges += 1;
        return (
            vec![LoopMessage::User(super::message::UserMessage {
                content: vec![super::message::UserPart::text(format!(
                    "{RESUME_NUDGE_TAG} Your last tool call failed or was rejected and you stopped without \
                     completing that step. Do not end here. Re-issue the call with corrected arguments and \
                     finish the work; if it genuinely cannot be done, say so plainly and report what you \
                     found. Don't just describe what you would do — do it."
                ))],
            })],
            FollowUpSource::ResumeAfterFailure,
        );
    }
    // 2. F6 verifier gate — one-time "verify before done" when code was edited
    //    but nothing was run to check it.
    if let Some(verifier) = &config.verifier {
        let msgs = verifier.check_before_finalize(config.verification_tiers_mode);
        if !msgs.is_empty() {
            return (msgs, FollowUpSource::Verifier);
        }
    }

    // 2.5 Claim/evidence gate (dirge-d0e5.2) — deterministic, no LLM. Fires
    //     once when the final answer asserts a verification result or a change
    //     the run's evidence does not support: a test count / named-gate claim
    //     ("4954 passed", "compiles", "clippy clean") with no observed command
    //     of the matching kind — a build cannot support "N passed" and a test
    //     run cannot support a named lint gate (dirge-lavc) — or a first-person
    //     "I fixed …" claim with zero files mutated. Sits AFTER the verifier
    //     gate so the more actionable "actually run the check" nudge wins when
    //     both would fire; the claim gate is the backstop for a model that
    //     finalizes while still claiming an unrun result. Byte-identical when
    //     off.
    if config.claim_gate_mode != GateMode::Off
        && *claim_nudges < super::claim_gate::claim_nudge_cap(config.claim_gate_mode)
        && let Some(LoopMessage::Assistant(last)) = new_messages.last()
    {
        {
            let answer: String = last
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let claims = super::claim_gate::scan_final_answer(&answer);
            let observed = config
                .verifier
                .as_ref()
                .map(|v| v.observed_commands())
                .unwrap_or_default();
            let files_mutated = crate::agent::tools::modified::since(*run_epoch).len();
            if let Some(kind) =
                super::claim_gate::unsupported_claims(&claims, &observed, files_mutated)
            {
                *claim_nudges += 1;
                return (
                    vec![LoopMessage::User(super::message::UserMessage {
                        content: vec![super::message::UserPart::text(format!(
                            "{} {}",
                            super::claim_gate::CLAIM_GATE_TAG,
                            kind.nudge_text()
                        ))],
                    })],
                    FollowUpSource::ClaimGate,
                );
            }
        }
    }
    // 2.6 Artifact-scope sourcing gate (dirge-lavc GAP 1) — deterministic,
    //     no LLM, and OFF by default (`source_gate` config; opt-in). The
    //     final-answer scanners above cannot see a claim written INTO an
    //     artifact; this gate parses the run's diff for ADDED comment lines
    //     that assert consulting an external source ("checked Aug 2026",
    //     "per the", "pricing page") and fires one nudge when no
    //     fetch/search tool ran. Hard bias toward under-detecting: RFC/bug/
    //     spec citations, repo-file references, URLs, and pre-existing WIP
    //     (subtracted against the run-start baseline) are excluded before
    //     the vocabulary applies. The diff capture is paid only when armed
    //     AND the run mutated files.
    if config.source_gate_mode != GateMode::Off
        && *source_nudges < super::source_gate::source_nudge_cap(config.source_gate_mode)
    {
        let files = crate::agent::tools::modified::since(*run_epoch);
        if !files.is_empty() {
            let repo = config.code_review_repo.clone().unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
            let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
            let baseline = code_review_baseline.as_ref().map(|b| b.capped.clone());
            let repo_for_diff = repo.clone();
            let current = tokio::task::spawn_blocking(move || {
                super::code_review::capture_run_diff(&repo_for_diff)
            })
            .await
            .ok()
            .flatten();
            if let Some(current) = current {
                let allowed: Vec<String> = files
                    .iter()
                    .filter_map(|p| {
                        p.strip_prefix(&repo)
                            .ok()
                            .map(|r| r.to_string_lossy().replace('\\', "/"))
                    })
                    .collect();
                let hits = super::source_gate::added_sourcing_comments(
                    &current.capped,
                    baseline.as_deref(),
                    &allowed,
                );
                let tool_names: Vec<String> = new_messages
                    .iter()
                    .filter_map(|m| match m {
                        LoopMessage::ToolResult(r) => Some(r.tool_name.clone()),
                        _ => None,
                    })
                    .collect();
                if let Some(comment) = super::source_gate::unsupported_sourcing(&hits, &tool_names)
                {
                    *source_nudges += 1;
                    return (
                        vec![LoopMessage::User(super::message::UserMessage {
                            content: vec![super::message::UserPart::text(format!(
                                "{} A comment you added this run ({comment:?}) asserts having consulted an external source, but no fetch/search tool ran this run. Either verify the claim by actually fetching the source, or remove/rewrite the unsupported sourcing assertion.",
                                super::source_gate::SOURCE_GATE_TAG
                            ))],
                        })],
                        FollowUpSource::SourceGate,
                    );
                }
            }
        }
    }

    // 3. Unified finalization judge (dirge-8v98) — ONE judge call that both
    //    judges completeness (the old F6 critic) AND reviews the run's diff for
    //    defects (the old diff-aware reviewer), returning a single consolidated
    //    follow-up. Fires only if a judge is armed (`critic_fn`, i.e. a
    //    `critic_provider` is configured) and the run did real work.
    //
    //    `code_review_mode` tunes it: `Off` reviews completeness only (no diff
    //    capture, zero extra cost — behaves as the old transcript-only critic);
    //    `Advisory` (default) and `Blocking` additionally capture and review the
    //    run's diff. The diff is compared against the run-start baseline
    //    (dirge-1g3v), so a read-only turn has an unchanged diff and reviews
    //    completeness only — never paying to review nothing.
    //
    //    Lifecycle: `Off`/`Advisory` fire ONCE per run (one-shot, gated by
    //    `critic_done`); `Blocking` PERSISTS across finalizations (bounded by
    //    MAX_REVIEW_REACT via `code_review_reacts`) so the agent can fix findings
    //    and be re-reviewed until the diff is clean. Any finding — even
    //    medium/low — re-enters the loop so the model actually sees and acts on
    //    it, rather than a display-only notice it never reads.
    if config.critic_fn.is_some() && run_made_tool_calls(new_messages) {
        let mode = config.code_review_mode;
        let one_shot = mode != CodeReviewMode::Blocking;
        let may_fire = if one_shot {
            // dirge-2m68: `critic_done` marks a verdict actually received;
            // `critic_attempts` bounds the retries a broken judge can cost.
            !*critic_done && *critic_attempts < MAX_CRITIC_JUDGE_ATTEMPTS
        } else {
            *code_review_reacts < super::code_review::MAX_REVIEW_REACT
        };
        if may_fire && let Some(judge) = &config.critic_fn {
            // Capture the run's diff only when diff-review is on AND the working
            // tree changed since run start; otherwise review completeness alone.
            // dirge-9b2k: keep the UNcapped fingerprint too (dirge-8gdv) so the
            // Blocking path can tell when the model changed nothing between two
            // reviews of the same diff and skip a redundant stateless judge call.
            let (diff_owned, current_fingerprint): (Option<String>, Option<u64>) =
                if mode != CodeReviewMode::Off {
                    // dirge-9b2k: honor an explicit repo override (tests inject a
                    // temp tree); production leaves it None → process CWD.
                    let repo = config.code_review_repo.clone().unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    });
                    // Diff capture shells out to git — do it off the async runtime.
                    let captured = tokio::task::spawn_blocking(move || {
                        super::code_review::capture_run_diff(&repo)
                    })
                    .await
                    .ok()
                    .flatten();
                    let fp = captured.as_ref().map(|d| d.fingerprint);
                    let diff = run_delta_to_review(captured.as_ref(), code_review_baseline)
                        .map(str::to_string);
                    (diff, fp)
                } else {
                    (None, None)
                };
            // dirge-9b2k: Blocking dedupe. The judge is stateless, so when the
            // model declined a finding and changed nothing on disk, the next
            // reaction re-reviews the identical diff, re-raises the same finding,
            // and elicits the same rebuttal — a duplicate message. When this exact
            // diff (by uncapped fingerprint) was reviewed last reaction, skip the
            // judge call; the model's rebuttal stands. Off/Advisory are one-shot
            // via `critic_done`, so the guard is Blocking-only.
            //
            // Falling through (NOT early-returning) is load-bearing: a skip is
            // semantically "the critic found nothing this reaction", and the
            // existing empty-findings path falls through to the goal / todo /
            // open-issues gates. An early `return` here would silently drop those
            // nudges — so `skip` only gates the judge block below, leaving the
            // downstream gates in play on a skipped reaction.
            // dirge-9b2k: Blocking dedupe. The judge is stateless, so when the
            // model declined a finding and changed nothing on disk, the next
            // reaction re-reviews the identical diff, re-raises the same
            // finding, and elicits the same rebuttal — a duplicate message.
            //
            // dirge-mu46: but `run_unified_review` judges TWO things — the diff
            // for defects and the transcript for completeness — and skipping
            // wholesale skipped completeness too, letting an objectively
            // incomplete task finalize on the model's say-so because the
            // reaction that would have re-judged it never ran.
            //
            // The extra condition is `last_review_findings.is_some()`: only skip
            // when the previous reaction actually raised DIFF FINDINGS, which is
            // the duplicate-message scenario the dedupe exists for. When the
            // previous message was completeness-only there is no finding to
            // duplicate, and the transcript has grown since, so re-judging is
            // exactly the right thing to do.
            //
            // Falling through (NOT early-returning) is load-bearing: a skip is
            // semantically "the critic found nothing this reaction", and the
            // existing empty-findings path falls through to the goal / todo /
            // open-issues gates. An early `return` would silently drop those.
            let skip = mode == CodeReviewMode::Blocking
                && diff_owned.is_some()
                && current_fingerprint == *last_reviewed_fingerprint
                && last_review_findings.is_some();
            if !skip {
                let transcript = build_critic_transcript(new_messages);
                // dirge-6q3w: thread the run's compile/lint/test signal so the
                // judge can be pickier about unverified changes. dirge-bedj: judge
                // within the agent's own system prompt so it never demands a
                // forbidden action.
                let verification = config
                    .verifier
                    .as_ref()
                    .map(|v| v.status(config.verification_tiers_mode));
                let evidence = super::critic::Evidence {
                    files_mutated: crate::agent::tools::modified::since(*run_epoch)
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect(),
                    observed_commands: config
                        .verifier
                        .as_ref()
                        .map(|v| v.observed_commands())
                        .unwrap_or_default(),
                    tool_calls: new_messages
                        .iter()
                        .filter(|m| matches!(m, super::message::LoopMessage::ToolResult(_)))
                        .count(),
                    // dirge-lavc GAP 4: distinct tool names from the SAME
                    // observation as the count above, so a claim to have
                    // consulted a source is checkable against the set.
                    tool_names: new_messages
                        .iter()
                        .filter_map(|m| match m {
                            super::message::LoopMessage::ToolResult(r) => Some(r.tool_name.clone()),
                            _ => None,
                        })
                        .collect(),
                };
                let outcome = super::critic::run_unified_review(
                    judge,
                    system_prompt,
                    &transcript,
                    diff_owned.as_deref(),
                    verification,
                    last_review_findings.as_deref(),
                    Some(&evidence),
                )
                .await;
                let msgs = outcome.messages;
                // dirge-9b2k: carry per-reaction state forward for the next
                // Blocking finalization. The fingerprint (only when a diff was
                // actually reviewed) lets the next reaction skip an unchanged
                // diff; the findings feed its judge prompt so it re-raises one
                // only if still-present-and-unaddressed.
                //
                // dirge-q7vw: record the fingerprint ONLY when the judge
                // actually ran. A failed call fails open with no messages and
                // no findings — indistinguishable from a clean review until
                // `judged` existed — and recording the fingerprint for it made
                // the next reaction skip the same unchanged diff, so the diff
                // never got reviewed at all.
                if mode == CodeReviewMode::Blocking && outcome.judged {
                    if diff_owned.is_some() {
                        *last_reviewed_fingerprint = current_fingerprint;
                    }
                    *last_review_findings = outcome.raised_findings;
                }
                // One-shot modes fire at most once; blocking spends its budget
                // only when it actually re-enters.
                //
                // dirge-2m68: the shot is spent by a VERDICT, not by an attempt.
                // This used to flip `critic_done` unconditionally, so a judge
                // that timed out or errored — which fails open with no messages
                // and no findings — consumed the single completeness check for
                // the whole run. The backstop disappeared precisely when the
                // provider was unhealthy, and nothing said so. `outcome.judged`
                // already drew this distinction for the Blocking fingerprint
                // (dirge-q7vw); it just was not read here.
                //
                // The attempt is always counted, so a judge that keeps failing
                // is retried at most `MAX_CRITIC_JUDGE_ATTEMPTS` times per run
                // rather than at every finalization.
                if one_shot {
                    *critic_attempts += 1;
                    if outcome.judged {
                        *critic_done = true;
                    }
                } else if !msgs.is_empty() {
                    *code_review_reacts += 1;
                }
                if !msgs.is_empty() {
                    return (msgs, FollowUpSource::Critic);
                }
            }
        }
    }
    // 3.5 Goal gate — user-defined stop condition. Unlike the one-shot
    //     critic, this PERSISTS across finalizations: each time the model
    //     tries to stop, an independent judge (the critic provider, reused)
    //     rules whether the stated condition holds; if not, its reason
    //     re-enters the loop. Bounded by MAX_GOAL_REACT so a mis-stated or
    //     unsatisfiable goal can't loop forever. Active only when a goal is
    //     set AND a judge is configured — off for default/interactive runs.
    //     dirge-g2ex: extracted into `poll_goal_gate` so the step-0 awaiting-
    //     user gate reuses it; conditions and `*goal_reacts` accounting are
    //     byte-for-byte unchanged.
    if let Some(hit) = poll_goal_gate(config, system_prompt, new_messages, goal_reacts).await {
        return hit;
    }
    // 4. vix-port — final gate: nudge the model to finish or clear unfinished
    //    todos before stopping. Bounded by MAX_TODO_NUDGES.
    //    dirge-g2ex: gated on `turn_made_file_edits` (NOT `run_made_tool_calls`).
    //    This nudge means "you were working and stopped with it unfinished"; a
    //    read-only Q&A or investigation turn has nothing of its own to finish,
    //    and `todo::unfinished_count()` is a cross-turn process-global — so
    //    without this gate the model gets dragged back into stale coding todos
    //    every time the user interrupts to ask something. (A Q&A turn that
    //    greps and reads still trips `run_made_tool_calls`, so that's the wrong
    //    predicate here.)
    //    dirge-u1ay (GH #734): the edit precondition also excluded the one
    //    case with no other backstop — a turn that wrote a todo list and did
    //    nothing else. `write_todo_list` isn't an Edit operation, so "model
    //    plans, model stops, nothing on disk" finalized silently. Planning
    //    THIS turn is as good a signal of unfinished own-work as editing is,
    //    and it can't drag an interrupting Q&A into stale todos (a Q&A turn
    //    writes no list), so it gets its own branch with wording that points
    //    at the edit rather than at the list.
    //    The plan-only branch is ONE-SHOT (`*todo_nudges == 0`), unlike the
    //    edit branch's budget of MAX_TODO_NUDGES: `new_messages` accumulates
    //    across re-entries, so the triggering `write_todo_list` call stays in
    //    the list and the condition would hold on every later pass. A
    //    behavioral nudge that didn't land the first time doesn't land on the
    //    third identical repeat — it just spends round-trips.
    if *todo_nudges < MAX_TODO_NUDGES {
        let edited = turn_made_file_edits(new_messages);
        if edited || (*todo_nudges == 0 && turn_wrote_todos(new_messages)) {
            let (high, normal, low) = crate::agent::tools::todo::unfinished_by_priority();
            let unfinished = high + normal + low;
            if unfinished > 0 {
                *todo_nudges += 1;
                let msg = if edited {
                    todo_nudge_message(unfinished, low)
                } else {
                    plan_only_nudge_message(unfinished)
                };
                return (vec![msg], FollowUpSource::Todo);
            }
        }
    }
    // 5. dirge-ksjl — open-issues gate: nudge when this session left issues
    //    open. Session-scoped (not the global board), lowest priority.
    //    Advisory emits a one-shot SystemNotice; blocking re-enters the loop
    //    bounded by MAX_OPEN_ISSUES_NUDGES.
    //    dirge-g2ex: same `turn_made_file_edits` precondition as the todo gate
    //    (above) — a read-only turn has nothing of its own to finish.
    if open_issues_gate_mode != GateMode::Off
        && turn_made_file_edits(new_messages)
        && let Some(db_path) = issue_db_path
    {
        // Clone to PathBuf for 'static spawn_blocking captures.
        let db_path_buf = db_path.to_path_buf();
        let session_owned = session_id.map(|s| s.to_string());
        let count = tokio::task::spawn_blocking(move || {
            crate::extras::issue_db::IssueStore::open_at(&db_path_buf)
                .ok()
                .and_then(|store| {
                    store
                        .board_for_session(session_owned.as_deref(), None)
                        .ok()
                        .map(|issues| issues.len())
                })
                .unwrap_or(0)
        })
        .await
        .unwrap_or(0);
        if count > 0 {
            match open_issues_gate_mode {
                GateMode::Advisory => {
                    // One-shot (fires at most once per run), then re-enter
                    // ONCE with a model-visible, tagged message. The text is
                    // an imperative aimed at the model — a display-only
                    // SystemNotice it never sees would change nothing
                    // (dirge-1elu.4, arXiv:2604.25850v4 §C.1.4/§C.2.4). The
                    // loop's emit_harness_notices mirror renders the same
                    // SystemNotice the old path emitted directly, so what the
                    // human sees is unchanged.
                    if *open_issues_nudges == 0 {
                        *open_issues_nudges += 1;
                        return (
                            vec![LoopMessage::User(super::message::UserMessage::text(
                                format!(
                                    "{OPEN_ISSUES_NUDGE_TAG} {count} issue(s) from this session \
                                     are still open — close or defer them when done."
                                ),
                            ))],
                            FollowUpSource::OpenIssues,
                        );
                    }
                }
                GateMode::Blocking => {
                    if *open_issues_nudges < MAX_OPEN_ISSUES_NUDGES {
                        *open_issues_nudges += 1;
                        // Build the nudge listing up to ~5 open session issue titles.
                        let db_path_buf2 = db_path.to_path_buf();
                        let sid = session_id.map(|s| s.to_string());
                        let titles = tokio::task::spawn_blocking(move || {
                            crate::extras::issue_db::IssueStore::open_at(&db_path_buf2)
                                .ok()
                                .and_then(|store| {
                                    store.board_for_session(sid.as_deref(), Some(5)).ok()
                                })
                                .unwrap_or_default()
                                .into_iter()
                                .map(|i| i.title)
                                .collect::<Vec<_>>()
                        })
                        .await
                        .unwrap_or_default();
                        let title_list = if titles.is_empty() {
                            String::new()
                        } else {
                            let mut s = String::from("\n\nStill open:\n");
                            for t in &titles {
                                s.push_str(&format!("- {t}\n"));
                            }
                            s
                        };
                        return (
                            vec![LoopMessage::User(super::message::UserMessage::text(
                                format!(
                                    "{OPEN_ISSUES_NUDGE_TAG} {count} issue(s) you worked on \
                                     this session are still open. Close the ones you finished \
                                     (or explicitly defer them), then continue:{title_list}"
                                ),
                            ))],
                            FollowUpSource::OpenIssues,
                        );
                    }
                }
                GateMode::Off => unreachable!("gated above"),
            }
        }
    }
    // dirge-track: file-edits-without-todos advisory — fires at most once per
    // run when the model edited files this turn but has no active todo tracked.
    // The boundary nudge (poll_boundary_nudge → build_early_track_work_reminder)
    // shares the same `track_nudges` budget, so only one of the two can ever
    // fire; this finalization-time copy catches runs that finalize without
    // passing a boundary. The text is an imperative aimed at the model, so it
    // must be a model-visible tagged message (dirge-1elu.4) — the loop's
    // emit_harness_notices mirror emits the SystemNotice the old path emitted
    // directly, keeping what the human sees unchanged.
    if should_advise_untracked_work(
        session_id,
        *track_nudges,
        crate::agent::tools::todo::unfinished_count(),
        turn_made_file_edits(new_messages),
    ) {
        *track_nudges += 1;
        return (
            vec![LoopMessage::User(super::message::UserMessage::text(
                format!(
                    "{TRACK_WORK_TAG} You modified files this turn but have no active todo. If this \
                 task isn't finished, add it with write_todo_list and mark it in_progress so it \
                 stays your tracked priority (and gets closed when done)."
                ),
            ))],
            FollowUpSource::Todo,
        );
    }
    // 7. dirge-2m68 — deterministic completeness gate, LAST on purpose.
    //
    //    Every gate above is a narrow mechanical detector: unrun edits, a
    //    failed last call, a claim the evidence contradicts, todos the model
    //    tracked. The one gate that asks "is this task actually done?" is the
    //    LLM critic, which is inert without `critic_provider`. So a run that
    //    edited real files, ran a real check, claimed nothing false and
    //    tracked no todos could stop halfway and hit nothing at all.
    //
    //    This fires only when the model's OWN final answer states first-person
    //    work it still intended to do. It sits below everything else because
    //    it is the least specific: any gate above has a more actionable thing
    //    to say, and this is the backstop for when none of them did.
    //
    //    The `unfinished_count() == 0` condition is not redundant with the
    //    todo gate above — it is what keeps the two from both firing on the
    //    same run. With tracked todos outstanding, "finish your todos" is the
    //    better message and the todo gate owns it.
    if let Some(LoopMessage::Assistant(last)) = new_messages.last() {
        let answer: String = last
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if super::completeness_gate::should_nudge_incomplete(
            config.completeness_gate_mode,
            *completeness_nudges,
            crate::agent::tools::todo::unfinished_count(),
            turn_made_file_edits(new_messages),
            &answer,
        ) {
            *completeness_nudges += 1;
            return (
                vec![LoopMessage::User(super::message::UserMessage::text(
                    format!(
                        "{} {}",
                        super::completeness_gate::COMPLETENESS_GATE_TAG,
                        super::completeness_gate::nudge_text()
                    ),
                ))],
                FollowUpSource::CompletenessGate,
            );
        }
    }
    (Vec::new(), FollowUpSource::None)
}

/// LOOP-9 — context-compaction worker. Runs the cheap pruning pass
/// first; when a summarizer callback is wired AND pruning alone
/// didn't free enough headroom (compressed token count is still
/// above the pruner's protection floor), invokes the auxiliary
/// summarizer + replaces the middle section of `current_context.messages`
/// with a structured-summary system message.
///
/// Emits `LoopEvent::ContextCompacted` with a rotated session id
/// once the pass finishes (whether pruning-only or pruning+summary).
/// Session.id rotation + DB persistence is delegated to the event
/// consumer side via this event channel.
/// dirge-h5tv: fire `on_pre_compress` on a memory provider (if
/// attached) over the to-be-discarded message slice, and combine
/// its returned insights with the user-supplied focus topic so the
/// summary prompt preserves both. Returns the final string (or
/// `None` when neither contributes).
///
/// Lives here rather than in compression.rs because the
/// MemoryProvider trait lives in `extras` and shouldn't leak into
/// the pure compression module. The slice → transcript conversion
/// uses `build_transcript_from_value_slice` to share format with
/// the slash-path's `build_transcript_from_slice`.
fn build_augmented_focus(
    focus_topic: Option<&str>,
    provider: Option<&std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
    middle: &[serde_json::Value],
) -> Option<String> {
    // Lazy transcript build: only walk the middle slice when a
    // provider is attached. The common no-provider case
    // short-circuits without paying the format cost.
    let insights = provider.map(|p| {
        let transcript = transcript_from_value_slice(middle);
        crate::agent::review::fire_pre_compress(p.as_ref(), &transcript)
    });
    match (
        focus_topic.map(str::trim),
        insights.as_deref().map(str::trim),
    ) {
        (Some(focus), Some(ins)) if !focus.is_empty() && !ins.is_empty() => {
            Some(format!("{focus}\n\nProvider insights:\n{ins}"))
        }
        (Some(focus), _) if !focus.is_empty() => Some(focus.to_string()),
        (_, Some(ins)) if !ins.is_empty() => Some(format!("Provider insights:\n{ins}")),
        _ => None,
    }
}

/// Build a transcript string from a Vec<Value> slice (raw loop
/// messages). Mirrors `build_transcript_from_slice` over
/// `SessionMessage`. Used by `build_augmented_focus` for the
/// on_pre_compress hook.
fn transcript_from_value_slice(messages: &[serde_json::Value]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        let content = crate::agent::compression::content_text(m.get("content"));
        if !content.is_empty() {
            let _ = writeln!(out, "{}: {}", role, content);
            out.push('\n');
        }
    }
    out
}

/// Consecutive summarizer failures (per run) before the compaction
/// circuit breaker opens and the LLM summarizer is skipped for the rest
/// of the run — the cheap `prune_tool_outputs` pass still runs, so
/// context can't grow unbounded. 3 tolerates two transient failures; a
/// third means the summarizer is systematically broken and retrying it
/// every fold just wastes API calls (IMPROVEMENTS_PLAN #1).
const MAX_CONSECUTIVE_COMPACTION_FAILURES: u32 = 3;

/// How many few-shot tool-use exemplars to inject per task. Research
/// puts the sweet spot at 2–5; the retriever returns fewer (or none)
/// when the task matches fewer exemplars.
const EXEMPLAR_TOP_K: usize = 3;

/// dirge-4afz: append a tail context note, unless a byte-identical copy is
/// already in the model-facing context. Returns whether it was pushed.
///
/// These blocks (few-shot exemplars, memory pre-recall) are selected from the
/// user's prompt, so two related turns in a row routinely select the SAME
/// block. Unlike a system-prompt section — rebuilt from scratch each turn — a
/// tail message persists, so re-pushing an identical block adds a second copy
/// that says nothing new and is paid for until the session ends.
///
/// Comparing against the live context rather than remembering the last block
/// pushed is deliberate: if compaction has since folded the earlier copy away,
/// the block is genuinely absent and re-injecting it is the right call.
/// Push a tail context note that REPLACES any earlier copy of itself.
///
/// [`push_context_note_if_absent`] appends when the block is not already
/// present, which is right for the additive notes it was built for — another
/// exemplar or another recalled memory is more knowledge, and an older one is
/// still true. It is wrong for a block that states CURRENT state. After a `cd`
/// or a `git switch` the previous turn envelope does not become merely
/// redundant, it becomes FALSE, and leaving it in front of the new one hands
/// the model two contradictory answers with the stale one first — strictly
/// worse than the single stale answer the envelope exists to remove.
///
/// `marker` identifies prior copies (the block's opening tag). Messages that
/// merely CONTAIN the marker as part of ordinary content are not at risk: only
/// text-only user messages whose content STARTS with it are removed, and the
/// marker is an XML open tag the harness itself emits.
///
/// Returns `false` (and leaves the context untouched) when an identical block
/// is already the note in place — re-appending it would move it to the tail
/// every turn and invalidate everything cached after it.
pub(crate) fn replace_context_note(context: &mut Context, marker: &str, block: String) -> bool {
    let msg = LoopMessage::User(super::message::UserMessage::text(block));
    let value = loop_message_to_value(&msg);
    if context.messages.iter().any(|m| m == &value) {
        return false;
    }
    context.messages.retain(|m| {
        !m.get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|c| c.starts_with(marker))
    });
    context.messages.push(value);
    true
}

fn push_context_note_if_absent(context: &mut Context, block: String) -> bool {
    let msg = LoopMessage::User(super::message::UserMessage::text(block));
    let value = loop_message_to_value(&msg);
    if context.messages.iter().any(|m| m == &value) {
        return false;
    }
    context.messages.push(value);
    true
}

/// Max live ACTIVE issues surfaced in the turn-start "Active work queue" section.
/// The rest get a "+N more" hint so a large active board can't flood context.
const ACTIVE_TOP_N: usize = 7;

/// Max live BACKLOG issues surfaced in the turn-start "Backlog" section.
const BACKLOG_TOP_N: usize = 5;

/// dirge-x6yi: open the issue DB and build the turn-start board reminder with
/// separate active / backlog sections. This is synchronous rusqlite I/O (open +
/// query), so `run_agent_loop` hands it to `spawn_blocking` — a contended/locked
/// `state.db` must not stall the whole loop task (mirrors the pre-recall search
/// path). Returns `None` on any failure (missing/locked db, empty board); the
/// reminder is best-effort context, never fatal.
fn issue_board_reminder_block(
    db_path: &std::path::Path,
    session_id: Option<&str>,
) -> Option<String> {
    crate::extras::issue_db::IssueStore::open_at(db_path)
        .ok()?
        .board_reminder_split(session_id, ACTIVE_TOP_N, BACKLOG_TOP_N)
        .ok()
        .flatten()
}

/// What the LLM-summary stage of a compaction pass did, so `run_loop`
/// can drive the circuit-breaker counter. (The cheap prune always runs
/// regardless of this outcome.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryOutcome {
    /// A valid summary was produced (LLM or plugin) and applied. Carries
    /// the index of the inserted summary message so the caller can
    /// re-inject working-set file snapshots right after it
    /// (IMPROVEMENTS_PLAN #2).
    Succeeded(usize),
    /// The summarizer ran but returned an error or an invalid summary.
    Failed,
    /// The summarizer was not run: none wired, breaker open, or no
    /// foldable middle. Not a failure — doesn't trip the breaker.
    Skipped,
}

/// Fold a compaction pass outcome into the per-run failure counter:
/// reset on success, increment on failure, leave untouched on skip.
fn record_compaction_outcome(failures: &mut u32, outcome: SummaryOutcome) {
    match outcome {
        SummaryOutcome::Succeeded(_) => *failures = 0,
        SummaryOutcome::Failed => *failures = failures.saturating_add(1),
        SummaryOutcome::Skipped => {}
    }
}

/// A background-generated running summary that the destructive fold can
/// reuse instead of summarizing inline. The summary covers
/// `messages[0..boundary]` of the live context; `generation` is the fold
/// epoch it was built under. A destructive fold rebuilds the context (the
/// message indices change), so it bumps the epoch — a checkpoint whose
/// `generation` no longer matches the loop's is stale and won't be reused.
#[derive(Clone)]
struct CachedCheckpoint {
    summary: String,
    boundary: usize,
    generation: u64,
}

/// Loop-owned slot holding the freshest reusable checkpoint, shared with
/// the detached checkpoint tasks (which write it) and the fold (which reads
/// it). `None` means no reusable summary is available — the fold falls back
/// to an inline summarizer call.
type CheckpointSlot = std::sync::Arc<std::sync::Mutex<Option<CachedCheckpoint>>>;

/// Wall-clock ceiling on the inline compaction summarizer. A fold blocks
/// the loop until it returns; without a bound, a provider that stalls
/// without erroring (no chunks, stream never closes) freezes the session
/// indefinitely. On timeout the fold keeps the pruned context (a Failed
/// outcome) rather than hanging — the next turn retries or the breaker
/// eventually latches.
const COMPACTION_SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Wall-clock ceiling on a background checkpoint summarizer call. The
/// checkpoint is detached so it never blocks the loop, but a hung provider
/// would otherwise leak the task forever; bound it so it gives up.
const CHECKPOINT_SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Spawn a background incremental checkpoint: summarize a snapshot of the
/// current context off the loop, store it in `slot` for the next fold to
/// reuse, and emit [`LoopEvent::CheckpointRefresh`] so the consumer
/// persists it to the durable checkpoint WITHOUT folding. Best-effort — a
/// summarizer error, timeout, or invalid summary is silently dropped (the
/// next threshold, or the eventual destructive fold, will write one).
/// Mirrors MiMo's background checkpoint writer.
fn spawn_incremental_checkpoint(
    sfn: crate::agent::compression::SummarizeFn,
    messages: Vec<serde_json::Value>,
    // dirge-ioym: a WEAK sender. A detached checkpoint outlives the turn (its
    // summarizer call is bounded but slow), and a strong clone would keep the
    // per-turn event channel — and the runner task the pump joins on — open
    // until it finished, so a drain-to-close consumer blocked past AgentEnd.
    // Upgrading fails once the run's sender drops, so the refresh is delivered
    // only while the run is still live and skipped otherwise.
    emit: mpsc::WeakSender<LoopEvent>,
    slot: CheckpointSlot,
    generation: u64,
) {
    tokio::spawn(async move {
        use crate::agent::compression;
        if messages.is_empty() {
            return;
        }
        // Boundary = the snapshot length: this summary covers messages
        // [0..boundary]. Captured before the await so it reflects exactly
        // what was summarized, regardless of what the loop appends meanwhile.
        let boundary = messages.len();
        let budget = compression::summary_budget(compression::estimate_messages_tokens(&messages));
        // dirge-tgb9: refuses to build when the turns smuggle the fence
        // delimiter. Nothing to fall back to here — a checkpoint is an
        // optimisation, so skipping one just means the next fold summarizes
        // inline.
        let Ok(prompt) = compression::build_summary_prompt(
            &crate::agent::compaction_material::from_loop_messages(&messages),
            budget,
            None,
            None,
        ) else {
            tracing::warn!(
                target: "dirge::agent_loop",
                "checkpoint summary skipped: turns contain the reserved fence delimiter",
            );
            return;
        };
        let result = tokio::time::timeout(CHECKPOINT_SUMMARY_TIMEOUT, sfn(prompt)).await;
        // dirge-tgb9: strip echoed delimiters before this summary can be
        // spliced into context — same reason as the inline path below.
        let result =
            result.map(|r| r.map(|s| crate::agent::prompt::strip_compaction_delimiters(&s)));
        if let Ok(Ok(summary)) = result
            && compression::validate_summary(&summary)
        {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(CachedCheckpoint {
                    summary: summary.clone(),
                    boundary,
                    generation,
                });
            }
            if let Some(emit) = emit.upgrade() {
                let _ = emit.send(LoopEvent::CheckpointRefresh { summary }).await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_compaction_pass(
    current_context: &mut Context,
    summarize_fn: &Option<crate::agent::compression::SummarizeFn>,
    protect_tail: usize,
    compaction_failures: u32,
    memory_provider: &Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
    compaction_hooks: Option<&crate::agent::agent_loop::types::CompactionHooks>,
    emit: &mpsc::Sender<LoopEvent>,
    checkpoint_slot: &CheckpointSlot,
    generation: &mut u64,
    fold_target: u64,
) -> SummaryOutcome {
    run_compaction_pass_with_focus(
        current_context,
        summarize_fn,
        protect_tail,
        compaction_failures,
        None,
        memory_provider,
        compaction_hooks,
        emit,
        checkpoint_slot,
        generation,
        fold_target,
    )
    .await
}

/// Same as `run_compaction_pass` but accepts an optional focus
/// topic to splice into the Hermes-style summary prompt. Wired by
/// the `/compress <focus>` slash command path. The auto-triggered
/// compaction (`PostUsageDecisionKind::Fold` / `ExitWithSummary`)
/// continues to use the no-focus wrapper above.
///
/// dirge-h5tv: `memory_provider` carries the optional plugin
/// provider so `on_pre_compress` can fire here, mirroring what
/// `handle_compress` does for the /compress slash command. Auto-
/// fold is the high-frequency path; without the fire, plugin
/// providers' extracted insights are silently dropped.
#[allow(clippy::too_many_arguments)]
async fn run_compaction_pass_with_focus(
    current_context: &mut Context,
    summarize_fn: &Option<crate::agent::compression::SummarizeFn>,
    protect_tail: usize,
    compaction_failures: u32,
    focus_topic: Option<String>,
    memory_provider: &Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
    compaction_hooks: Option<&crate::agent::agent_loop::types::CompactionHooks>,
    emit: &mpsc::Sender<LoopEvent>,
    // Round 1 (fast compaction): reusable background-checkpoint slot, the
    // current fold epoch (bumped on a successful destructive fold so
    // pre-fold checkpoints go stale), and the token level reuse must clear
    // to count as "fast enough" (else fall back to inline summarization).
    checkpoint_slot: &CheckpointSlot,
    generation: &mut u64,
    fold_target: u64,
) -> SummaryOutcome {
    use crate::agent::compression;

    let before = compression::estimate_messages_tokens(&current_context.messages);

    // dirge-jia8: observe-only `on-before-compact` plugin hook. It
    // CANNOT cancel — the fold proceeds regardless (cancelling an
    // emergency fold would overflow the next request).
    if let Some(hooks) = compaction_hooks {
        (hooks.on_before)(current_context.messages.len(), before).await;
    }

    // First pass: cheap tool-output pruning. No LLM call.
    let pruned = compression::prune_tool_outputs(&current_context.messages, protect_tail);
    current_context.messages = pruned;
    let after_prune = compression::estimate_messages_tokens(&current_context.messages);

    // Second pass: if a summarizer is wired AND we still have
    // meaningful material to summarize, build the Hermes-style
    // structured prompt, call the auxiliary model, validate the
    // returned summary, and replace the middle section.
    let mut after_summary = after_prune;
    let mut applied_summary = String::new();
    // first_kept_index defaults to "no message was folded out" —
    // pruner-only path doesn't drop messages by index, just trims
    // their content in place. compress_reporting handles that
    // gracefully (zero-width fold).
    let mut applied_first_kept = current_context.messages.len();
    // Drives the per-run circuit breaker: Skipped unless the summarizer
    // actually runs and resolves to a valid summary (Succeeded) or an
    // error / invalid summary (Failed).
    let mut outcome = SummaryOutcome::Skipped;
    // Tracks the breaker-open case so the emitted CompactionKind stays a
    // distinct failure signal (not a healthy-looking PruneOnly).
    let mut breaker_open = false;
    if compaction_failures >= MAX_CONSECUTIVE_COMPACTION_FAILURES {
        // Circuit breaker open: the summarizer has failed too many times
        // this run. Skip the LLM call entirely and keep the pruned
        // context (IMPROVEMENTS_PLAN #1).
        breaker_open = true;
        tracing::warn!(
            target: "dirge::agent_loop",
            failures = compaction_failures,
            "compaction summarizer failed {compaction_failures} consecutive times — circuit breaker open, skipping LLM summarization",
        );
    } else if let Some(sfn) = summarize_fn {
        // Fast path (Round 1): reuse a fresh background-checkpoint summary
        // instead of summarizing inline. The expensive summarization already
        // ran off the loop; here the fold is just prune + splice. Only when
        // the checkpoint is from the current fold epoch AND reusing it
        // actually clears `fold_target` — otherwise fall through to inline.
        let reusable = checkpoint_slot
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|cp| cp.generation == *generation);
        let mut reused = false;
        if let Some(cp) = reusable
            && let Some((new_msgs, cut)) = compression::apply_checkpoint_summary(
                &current_context.messages,
                &cp.summary,
                cp.boundary,
            )
        {
            let projected = compression::estimate_messages_tokens(&new_msgs);
            if projected <= fold_target {
                // dirge-vpma.9: fire the compaction hooks over the slice this
                // fold discards (`messages[..cut]`). The background checkpointer
                // that produced `cp` never consulted them, so without this a
                // memory provider never sees the discarded messages on the
                // high-frequency fast path (the silent-insight-drop dirge-h5tv
                // fixed for the inline path) and the on-compact plugin's
                // first-refusal is bypassed. Fired ONLY here inside the
                // committed reuse branch — the inline path below is
                // `!reused`-guarded, so the hooks fire exactly once per pass.
                let mut effective_summary = cp.summary;
                let mut effective_msgs = new_msgs;
                let mut effective_tokens = projected;
                if memory_provider.is_some() || compaction_hooks.is_some() {
                    let discarded: Vec<serde_json::Value> =
                        current_context.messages[..cut].to_vec();
                    // on_pre_compress (dirge-h5tv): let the memory provider
                    // observe the discarded slice for insight capture. The
                    // returned focus has no summary prompt to feed on the fast
                    // path, so the call is purely for its side effect.
                    let _ = build_augmented_focus(
                        focus_topic.as_deref(),
                        memory_provider.as_ref(),
                        &discarded,
                    );
                    // on_compact (dirge-jia8): give the plugin first refusal. If
                    // it returns a valid summary, fold with THAT instead of the
                    // checkpoint's — the checkpoint summary was generated without
                    // consulting the plugin.
                    if let Some(hooks) = compaction_hooks
                        && let Some(s) = (hooks.on_compact)(discarded).await
                        && compression::validate_summary(&s)
                        && let Some((m, _)) = compression::apply_checkpoint_summary(
                            &current_context.messages,
                            &s,
                            cp.boundary,
                        )
                    {
                        effective_tokens = compression::estimate_messages_tokens(&m);
                        effective_msgs = m;
                        effective_summary = s;
                    }
                }
                current_context.messages = effective_msgs;
                after_summary = effective_tokens;
                applied_summary = effective_summary;
                // dirge-vpma.3: apply_checkpoint_summary yields `[summary] +
                // messages[cut..]`, so the summary marker sits at NEW-list index 0.
                // `Succeeded(idx)` / `first_kept_index` are the NEW-list summary
                // index (restore_working_files splices file snapshots at idx+1), so
                // report 0 — the returned `cut` is the OLD-list cut and
                // feeding it splices snapshots at the wrong position (mid-tail →
                // orphaned tool_use/result → provider 400, or past the end).
                applied_first_kept = 0;
                outcome = SummaryOutcome::Succeeded(0);
                reused = true;
                tracing::info!(
                    target: "dirge::agent_loop",
                    boundary = cp.boundary,
                    tokens_after = after_summary,
                    "fast compaction: reused background checkpoint summary (no inline LLM call)",
                );
            }
        }

        let (start, end) = compression::compute_compress_window(
            &current_context.messages,
            compression::PROTECT_HEAD_DEFAULT,
            protect_tail.max(compression::PROTECT_TAIL_DEFAULT),
        );
        if !reused && start < end {
            // Signal the UI BEFORE the multi-second summarizer call so it
            // can show a "compacting…" indicator during the wait instead of
            // appearing frozen. `ContextCompacted` follows on completion.
            let _ = emit
                .send(LoopEvent::CompactionStarted {
                    tokens_before: before,
                })
                .await;
            let middle: Vec<serde_json::Value> = current_context.messages[start..end].to_vec();
            // Carry forward any previous summary body for iterative
            // re-compression (Hermes _find_latest_context_summary).
            let prev =
                compression::find_previous_summary(&current_context.messages).map(|(_, body)| body);
            let budget =
                compression::summary_budget(compression::estimate_messages_tokens(&middle));
            // dirge-h5tv: fire on_pre_compress on the to-be-discarded
            // middle slice and fold the provider's insights into the
            // focus_topic block. Empty returns / no provider → no
            // change (focus_topic stays as supplied). This mirrors
            // the /compress slash path's instructions augmentation.
            let augmented_focus =
                build_augmented_focus(focus_topic.as_deref(), memory_provider.as_ref(), &middle);
            // dirge-jia8: give the `on-compact` plugin hook first
            // refusal — if it supplies a valid summary, use it
            // instead of calling the LLM summarizer. An absent hook,
            // no summary, or an invalid one falls through to the LLM.
            let plugin_summary: Option<String> = match compaction_hooks {
                Some(hooks) => match (hooks.on_compact)(middle.clone()).await {
                    Some(s) if compression::validate_summary(&s) => Some(s),
                    _ => None,
                },
                None => None,
            };
            let summary_result: Result<String, _> = match plugin_summary {
                Some(s) => Ok(s),
                None => match compression::build_summary_prompt(
                    &crate::agent::compaction_material::from_loop_messages(&middle),
                    budget,
                    prev.as_deref(),
                    augmented_focus.as_deref(),
                ) {
                    // dirge-tgb9: the turns smuggle the reserved fence
                    // delimiter, so the material cannot be safely fenced.
                    // Reported as a failed summarization, which keeps the
                    // pruned context — the same degradation as the circuit
                    // breaker above, and far better than summarizing
                    // attacker-shaped text into the next turn's context.
                    Err(e) => Err(e),
                    Ok(prompt) => {
                        // Bound the inline call: a provider that stalls without
                        // erroring would otherwise freeze the loop indefinitely.
                        // On timeout, keep the pruned context (Failed outcome).
                        match tokio::time::timeout(COMPACTION_SUMMARY_TIMEOUT, sfn(prompt)).await {
                            Ok(r) => r,
                            Err(_) => Err(anyhow::anyhow!(
                                "compaction summarizer timed out after {}s",
                                COMPACTION_SUMMARY_TIMEOUT.as_secs()
                            )),
                        }
                    }
                },
            };
            // dirge-tgb9: strip any delimiter the model echoed, exactly as the
            // `/compact` path does (`provider::run_compaction`). This summary is
            // spliced into the context the next turn reads, so a stray fence
            // marker there would both confuse that turn and break the collision
            // check on the NEXT compaction — which is the guard that keeps an
            // attacker from closing the fence early. Covers the plugin-supplied
            // summary above too, since both arrive here.
            let summary_result =
                summary_result.map(|s| crate::agent::prompt::strip_compaction_delimiters(&s));
            match summary_result {
                Ok(summary) if compression::validate_summary(&summary) => {
                    let new_msgs =
                        compression::apply_summary(&current_context.messages, &summary, start, end);
                    current_context.messages = new_msgs;
                    after_summary =
                        compression::estimate_messages_tokens(&current_context.messages);
                    applied_summary = summary;
                    // After apply_summary, the head (0..start) is
                    // preserved, then a single summary message
                    // takes the place of the middle, then the tail
                    // resumes. The first KEPT original-index slot
                    // is therefore `start` — anything below was
                    // protected, anything above was folded.
                    applied_first_kept = start;
                    outcome = SummaryOutcome::Succeeded(start);
                }
                Ok(_) => {
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        "compaction summarizer returned an unvalidated summary — keeping pruned context",
                    );
                    outcome = SummaryOutcome::Failed;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        error = %e,
                        "compaction summarizer failed — keeping pruned context",
                    );
                    outcome = SummaryOutcome::Failed;
                }
            }
        }
    }

    // A successful destructive fold rebuilt the context — message indices
    // changed. Bump the fold epoch so any in-flight or already-stored
    // checkpoint built against the OLD indices is stale (generation mismatch
    // → never reused), and drop the slot: its summary was just consumed, or
    // belongs to a context that no longer exists.
    if matches!(outcome, SummaryOutcome::Succeeded(_)) {
        *generation = generation.wrapping_add(1);
        if let Ok(mut guard) = checkpoint_slot.lock() {
            *guard = None;
        }
    }

    // IMPROVEMENTS_PLAN #5: report what the pass did so consumers can
    // tell pruning-only from a summary, and spot a failing summarizer.
    // Breaker-open is its OWN kind so the failure signal survives after
    // the breaker latches (it'd otherwise look like a healthy PruneOnly).
    let compaction_kind = if breaker_open {
        crate::event::CompactionKind::PruneSummarizerDisabled
    } else {
        match outcome {
            SummaryOutcome::Succeeded(_) => crate::event::CompactionKind::PruneAndSummary,
            SummaryOutcome::Failed => crate::event::CompactionKind::PruneAndFailedSummary,
            SummaryOutcome::Skipped => crate::event::CompactionKind::PruneOnly,
        }
    };

    // dirge-kq3a: a pass that changed nothing must not present itself as a
    // compaction. `Skipped` means the summarizer never replaced a slice, and
    // no token reduction means the pruner freed nothing either — so the
    // context is byte-identical and there is nothing to report.
    //
    // Rotating anyway is not merely noisy, it is unbounded. The fold trigger
    // reads the API's `prompt_tokens`, which counts the system prompt and
    // every tool schema, while the fold only rewrites
    // `current_context.messages`. Once the unfoldable fixed overhead alone
    // sits above the threshold (a large MCP tool surface will do it), the
    // ratio stays high however often we fold, so the loop re-fires every
    // turn: rotate the session id, rebuild the agent, save, fire
    // `on_session_switch`, print "context compacted: N → N tokens", repeat.
    // Observed in the wild at ~6 second intervals with identical counts.
    //
    // Failed/breaker-open passes still report, even when the pruner freed
    // nothing: that event carries the summarizer-failure signal, and its own
    // runaway is already bounded by MAX_CONSECUTIVE_COMPACTION_FAILURES.
    if matches!(outcome, SummaryOutcome::Skipped) && after_summary >= before {
        tracing::debug!(
            target: "dirge::agent_loop",
            tokens = before,
            messages = current_context.messages.len(),
            "compaction pass freed nothing — not rotating the session",
        );
        return outcome;
    }

    let new_id = compression::rotate_session_id();
    let _ = emit
        .send(LoopEvent::ContextCompacted {
            new_session_id: new_id,
            tokens_before: before,
            tokens_after: after_summary,
            summary: applied_summary,
            first_kept_index: applied_first_kept,
            compaction_kind,
            // The summarizer model name isn't threaded through the opaque
            // SummarizeFn closure yet (follow-up).
            summary_model: None,
        })
        .await;

    outcome
}

/// Per-file read ceiling for restoration. A file larger than this is
/// skipped entirely rather than read into memory just to truncate it to
/// the snapshot budget — avoids an OOM if the agent touched a multi-GB
/// artifact (review fix). Generous vs the snapshot budget so normal
/// source files always restore.
const POST_COMPACT_MAX_READ_BYTES: u64 = 2 * 1024 * 1024;

/// Don't re-inject file snapshots if the just-folded context is already
/// above this fraction of the window: adding up to ~25k tokens of files
/// could re-cross the fold threshold and chatter fold↔restore (review
/// fix). Restoration is a convenience, not load-bearing — skip it when
/// there's no headroom.
const POST_COMPACT_RESTORE_CEILING: f64 = 0.50;

/// IMPROVEMENTS_PLAN #2: after a successful summary fold, re-read the
/// working-set files the agent was editing and splice fresh
/// `[Post-compaction file snapshot]` system messages in right after the
/// summary (index `summary_idx`) — so the fold doesn't strand the model
/// without the concrete file state it had been working from.
///
/// No-op without a file-touch tracker or tracked files, when the
/// post-fold context already lacks headroom, or when all candidate files
/// are unreadable / oversized. Reads are bounded by file count
/// (`POST_COMPACT_MAX_FILES`) AND per-file size (`POST_COMPACT_MAX_READ_BYTES`),
/// and each snapshot is token-capped by `build_post_compact_snapshots`.
async fn restore_working_files(
    config: &LoopConfig,
    ctx: &mut Context,
    summary_idx: usize,
    ctx_max: u64,
) {
    let Some(tracker) = &config.file_touch_tracker else {
        return;
    };
    let files = tracker.working_files();
    if files.is_empty() {
        return;
    }
    // Headroom guard: if the freshly-folded context is already high,
    // re-injecting files risks immediately re-crossing the fold
    // threshold. Restoration is optional — skip rather than oscillate.
    let post_fold = crate::agent::compression::estimate_messages_tokens(&ctx.messages);
    if post_fold as f64 > POST_COMPACT_RESTORE_CEILING * ctx_max.max(1) as f64 {
        tracing::debug!(
            target: "dirge::agent_loop",
            post_fold,
            ctx_max,
            "skipping post-compaction file restore — insufficient headroom",
        );
        return;
    }
    let mut contents: Vec<(std::path::PathBuf, String)> = Vec::new();
    for path in files
        .into_iter()
        .take(crate::agent::compression::POST_COMPACT_MAX_FILES)
    {
        // Skip files too large to read cheaply — don't materialize a
        // huge artifact in memory just to truncate it.
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.len() > POST_COMPACT_MAX_READ_BYTES => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        if let Ok(body) = tokio::fs::read_to_string(&path).await {
            contents.push((path, body));
        }
    }
    if contents.is_empty() {
        return;
    }
    let snapshots = crate::agent::compression::build_post_compact_snapshots(&contents);
    // Insert right after the summary message, before the protected tail.
    let at = (summary_idx + 1).min(ctx.messages.len());
    for (offset, snap) in snapshots.into_iter().enumerate() {
        ctx.messages.insert(at + offset, snap);
    }
}

/// Public entry point: start a new run from one or more prompt
/// messages. Faithful port of pi `runAgentLoop` (agent-loop.ts:95).
///
/// Emits `agent_start` + `turn_start`, then `message_start` /
/// `message_end` for each prompt, THEN enters `run_loop`. Returns
/// the full list of messages produced by this run (prompts + every
/// assistant turn + every tool result).
///
/// `summarize_fn` is an optional LOOP-9 context-compaction callback.
/// When `Some`, the compaction path runs a structured summarization
/// pass after the cheap `prune_tool_outputs` pre-pass — see
/// `crate::agent::compression::SummarizeFn` for the contract. Pass
/// `None` to disable LLM-summary compaction.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    prompts: Vec<LoopMessage>,
    mut context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
    summarize_fn: Option<crate::agent::compression::SummarizeFn>,
    // dirge-h5tv: optional memory provider for the on_pre_compress
    // hook during auto-compaction.
    memory_provider: Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
) -> Vec<LoopMessage> {
    // dirge-vlfb: the run's starting conditions. Not derivable from the event
    // stream, and the first thing a harness review needs: the tool NAMES the
    // model was actually offered (a feature the model cannot see is a feature
    // that is not shipped), and the window the context manager will judge
    // every turn against.
    if super::trace::enabled() {
        super::trace::note(
            "run_start",
            serde_json::json!({
                "tools": context.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
                "ctx_max": context_manager::effective_ctx_max(
                    context_manager::context_window_override().unwrap_or_else(|| {
                        config
                            .model_name
                            .as_deref()
                            .and_then(crate::config::context_window_for_model)
                            .unwrap_or(128_000)
                    }),
                ),
                "max_turns": config.max_turns,
                "model": config.model_name.clone(),
                "messages": context.messages.len(),
            }),
        );
    }

    // Pi line 103: `newMessages = [...prompts]`.
    let new_messages = prompts.clone();

    // The verbatim user message for this turn — drives both few-shot exemplar
    // retrieval and verbatim pre-recall.
    let task_query: String = prompts
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.text_joined()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Few-shot tool-use exemplars: retrieve up to K demonstrations
    // relevant to this task and inject them just before the prompt, so
    // the model has on-topic examples at the action boundary (in-context
    // tool demonstrations are a large reliability lever for open models).
    // Injected into the model-facing context ONLY — not `new_messages` —
    // so it steers this run without being persisted into session history.
    if let Some(block) = crate::agent::exemplars::block_for_task(&task_query, EXEMPLAR_TOP_K) {
        push_context_note_if_absent(&mut context, block);
    }

    // dirge-e31n.2: the volatile session facts, re-read at the start of every
    // user turn instead of frozen into the preamble at agent construction.
    // Pushed through the same tail channel as exemplars and pre-recall, so it
    // cannot churn the cached prefix, and deduped by `push_context_note_if_absent`
    // — an unchanged environment is pushed once and then costs nothing, while a
    // `cd` or a branch switch produces a genuinely different block that lands on
    // the next turn. `builder::agent_inner` omits these four lines from the
    // preamble under the same flag, so they are stated exactly once.
    if config.turn_envelope
        && let Some(rendered) = super::envelope::SessionFacts::read().to_envelope()
    {
        if !rendered.dropped.is_empty() {
            tracing::debug!(
                target: "dirge::context",
                dropped = ?rendered.dropped,
                "turn envelope over budget; sections dropped",
            );
        }
        // REPLACES rather than appends: a `cd` or `git switch` makes the
        // previous envelope false, not merely redundant.
        replace_context_note(&mut context, super::envelope::MARKER, rendered.text);
    }

    // Pi line 105: `currentContext.messages = [...context.messages, ...prompts]`.
    for prompt in &prompts {
        context.messages.push(loop_message_to_value(prompt));
        // Phase 4 part 2: notify the file-touch tracker about user
        // prompts so it can decide whether the streak persists or
        // resets to a new topic.
        if let (Some(tracker), LoopMessage::User(u)) = (&config.file_touch_tracker, prompt) {
            tracker.record_user_message(&u.text_joined());
        }
    }

    // dirge-0gxb: verbatim pre-recall. Auto-search long-term memory on this
    // turn's verbatim user message and inject the hits as a SUPPLEMENTAL
    // context note — pushed to the model-facing context ONLY, never to
    // `new_messages` (persisted history) or the frozen `<project_memory>`
    // snapshot (`system_prompt`). Appending at the tail can't churn the cached
    // prefix. Surfaces relevant stored memory the agent wouldn't think to look
    // up. Off-loaded to the blocking pool because the hybrid provider's search
    // may do a network embedding round-trip.
    //
    // The `memory_provider` gate is also the real safety net for the global
    // flag: the forked review/curator runners build with `memory_provider:
    // None`, so they never pre-recall regardless of the process-global toggle.
    // Injected as a USER message (like the exemplar block) rather than a
    // `system` one — the Codex/Responses path strips `system` transcript items
    // into the cached `instructions`, which would both drop the block and churn
    // the prefix; a user message stays a plain transcript item on every path.
    if super::context_manager::verbatim_pre_recall_enabled()
        && let Some(provider) = &memory_provider
        && super::context_manager::query_worth_pre_recalling(&task_query)
    {
        let snapshot = provider.format_for_system_prompt();
        let q = task_query.clone();
        let p = provider.clone();
        match tokio::task::spawn_blocking(move || p.search(&q)).await {
            Ok(Ok(resp)) => {
                if let Some(block) = super::context_manager::pre_recall_block(&resp, &snapshot) {
                    push_context_note_if_absent(&mut context, block);
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(target: "dirge::memory", error = %e, "pre-recall search failed")
            }
            Err(e) => {
                tracing::debug!(target: "dirge::memory", error = %e, "pre-recall task join failed")
            }
        }
    }

    // Native issue board: surface the agent's persistent kanban at the top of
    // each user-initiated run, so it doesn't have to remember to list it. Like
    // pre-recall, this is model-facing context only (never persisted) and is
    // gated on `memory_provider` — the same safety net that excludes the forked
    // review/curator runners (they build with `memory_provider: None`). Bounded
    // to the top N live issues with a "see the rest" hint, so a large backlog
    // can't flood the context.
    if memory_provider.is_some() {
        let db_path = std::env::current_dir()
            .map(|c| crate::extras::dirge_paths::ProjectPaths::new(&c).session_db_path())
            .unwrap_or_else(|_| std::path::PathBuf::from(".dirge/sessions/state.db"));
        let sid = config.session_id.clone();
        // dirge-x6yi: run the blocking open+query off the loop task.
        if let Ok(Some(block)) = tokio::task::spawn_blocking(move || {
            issue_board_reminder_block(&db_path, sid.as_deref())
        })
        .await
        {
            let msg = LoopMessage::User(super::message::UserMessage::text(block));
            context.messages.push(loop_message_to_value(&msg));
        }
    }

    // Pi lines 109-114: emit agent_start + turn_start + per-prompt
    // start/end pair.
    let _ = emit.send(LoopEvent::AgentStart).await;
    let _ = emit.send(LoopEvent::TurnStart).await;
    for prompt in &prompts {
        let _ = emit
            .send(LoopEvent::MessageStart {
                message: prompt.clone(),
            })
            .await;
        let _ = emit
            .send(LoopEvent::MessageEnd {
                message: prompt.clone(),
            })
            .await;
    }

    run_loop(
        context,
        new_messages,
        config,
        signal,
        emit,
        stream_fn,
        summarize_fn,
        memory_provider,
    )
    .await
}

/// The mid-turn boundary arbiter: at most ONE harness nudge per boundary
/// [dirge-5mtx.2].
///
/// The finalization boundary has had this discipline since dirge-vcsn —
/// [`poll_finalization_follow_up`] polls its gates in strict priority and
/// returns the first non-empty source. The mid-turn boundary never got it:
/// track-work, fast-verify, the progress signal, the file-touch reminder and
/// the safe-state/reflection rungs each pushed independently, so up to five
/// harness messages could land before one assistant turn. The only mutual
/// exclusions were two hand-written special cases, and the priority reasoning
/// was written in a comment ("runs LAST ... shouldn't pre-empt them") while
/// the code emitted everything anyway.
///
/// Priority runs most-specific first. A safe-state abort supersedes the
/// recovery checkpoint it replaces (telling the model to both retry and abort
/// is contradictory — the exclusion that used to be special-cased is now just
/// ordering). Progress is last because a stall diagnosis is the broadest thing
/// we can say, and saying it over a concrete instruction is noise.
///
/// ONE WART, deliberate. `ProgressTracker::record_turn` both advances the
/// barren-boundary counters AND returns the message, so it must be called on
/// every boundary or the stall and prologue signals freeze — it cannot be
/// short-circuited by a higher-priority winner. When something outranks it,
/// its message is dropped and that nudge's budget is spent on nothing. The
/// cost is bounded (MAX_STALL_NUDGES is 2, MAX_PROLOGUE_NUDGES is 1) and only
/// applies on a boundary where the model already received something more
/// specific. Splitting `record_turn` into advance-then-peek would remove it;
/// that is a change to progress.rs's contract and is left for dirge-5mtx.7,
/// which reworks these budgets anyway.
///
/// `poll_budget` is NOT called when something else wins: it consumes a budget
/// mark, and unlike `record_turn` it has no state to advance, so skipping it
/// preserves the mark for a later boundary.
/// Record one tool result's capability signals on the run tally [dirge-5mtx.7].
///
/// Every call counts toward `tool_calls` (the denominator for every rate) and,
/// when it failed, toward the errored count FOR ITS RECOVERY CLASS
/// (dirge-s9ry). A failed call whose name is in no tool the run was given is
/// ALSO recorded as a hallucinated name — the model invented the name rather
/// than misusing a real tool, which is a materially different capability
/// signal.
///
/// The class comes from the same `tool_error_class::classify` the failure
/// tracker uses on the same excerpt, so the checkpoint's account of a streak
/// and the estimator's account of the run cannot disagree about what a failure
/// was.
///
/// `prepare_tool_call` (tools.rs) is where the miss is actually detected: it
/// short-circuits with "Tool X not found" plus a nearest-name suggestion. But
/// it runs inside `execute_tool_calls_{sequential,parallel}`, neither of which
/// has the tally, and the parallel path shares its preflight across futures.
/// Re-deriving the classification here from the same tool list keeps the
/// counter next to every other one rather than threading `&mut GateTally`
/// through two batch executors.
///
/// The two counts STACK deliberately: a hallucinated call is both errored and
/// hallucinated, exactly as `repair_invalid` already stacks with errored. With
/// the current weights that orders the failure kinds — a plain error is 1, an
/// invented tool name 3, arguments too malformed to repair 5 — which is the
/// intended ranking.
///
/// KEEPING that ranking is why an invented name is classed [`ErrorClass::Misuse`]
/// rather than run through the classifier (dirge-s9ry). `prepare_tool_call`
/// phrases the miss as "Tool X not found", which the classifier reads — quite
/// reasonably — as [`ErrorClass::MissingInfo`], the one class that scores
/// double. That would have made an invented name 2 + 2 = 4, silently
/// re-ranking it against `repair_invalid` and counting ONE failure twice on
/// the same axis: `hallucinated_tool_names` already exists to say precisely
/// this. `Misuse` is also the honest label — an unknown tool name is the call
/// shape being wrong, not the tree being shaped differently than the model
/// thought.
///
/// Adding this signal can only ever move a run DOWN a tier, never up, so the
/// worst case is a run that read `Strong` reading `Nominal`, which is today's
/// default behaviour. That is why restoring it is safe despite its effect on
/// tiering being unmeasured: the counter was always zero, so nobody has data
/// on how often models invent tool names.
fn record_tool_result_signals(
    tally: &mut GateTally,
    name: &str,
    is_error: bool,
    excerpt: &str,
    known_tools: &[&str],
) {
    use super::tool_error_class::ErrorClass;
    let hallucinated = is_error && !known_tools.contains(&name);
    tally.record_tool_call(is_error.then(|| {
        if hallucinated {
            ErrorClass::Misuse
        } else {
            super::tool_error_class::classify(name, excerpt)
        }
    }));
    if hallucinated {
        tally.record_hallucinated_tool_name();
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn poll_boundary_nudge(
    config: &LoopConfig,
    guards: &super::activity::LoopGuards,
    safe_state_msg: Option<String>,
    new_messages: &[LoopMessage],
    turns_taken: usize,
    track_nudges: &mut u8,
    verify_nudges: &mut u8,
    tally: &mut GateTally,
    tier: super::capability::CapabilityTier,
) -> Option<(LoopMessage, BoundaryNudge)> {
    // Unconditional: the progress counters must advance every boundary.
    let progress_signal = config.progress.as_ref().and_then(|progress| {
        let snapshot = super::progress::ProgressSnapshot {
            todos_unfinished: crate::agent::tools::todo::unfinished_count(),
            files_touched: crate::agent::tools::modified::count(),
            // Deliberately the LATCHED green (`GateMode::Off`), not the
            // tier-aware one. Staleness (dirge-uw2l.3) flips a tiered status
            // back to Unverified on every post-green edit, so reading it here
            // would manufacture a fresh false→true edge on each edit→test
            // cycle and reset the stall counter forever — silently disabling
            // the monitor for green-but-not-converging runs, the exact case it
            // exists to catch. Staleness answers "verify again?"; progress
            // answers "did this run reach a new state?".
            verified_green: matches!(
                config.verifier.as_ref().map(|v| v.status(GateMode::Off)),
                Some(super::verifier::VerificationStatus::VerifiedGreen)
            ),
            // dirge-t5dh: the prologue bound also watches tool calls, since a
            // turn batching forty reads is one boundary but forty calls.
            tool_calls: tally.tool_calls() as usize,
        };
        progress.record_turn(snapshot)
    });

    // 1. Safe-state abort (EXEC rung 3) — supersedes the rung-2 checkpoint.
    if let Some(msg) = safe_state_msg {
        tally.record_nudge(BoundaryNudge::SafeState);
        return Some((
            LoopMessage::User(super::message::UserMessage::text(msg)),
            BoundaryNudge::SafeState,
        ));
    }
    // 2. Cross-turn recovery checkpoint (rung 2) — distinct tool errors piling
    //    up, which storm's identical-repeat rule never sees.
    if let Some((msg, kind)) = guards.poll_reflection(tier).into_iter().next() {
        tally.record_nudge(kind);
        return Some((msg, kind));
    }
    // 3. Work tracking — edits with nothing on the board.
    if let Some(reminder) = build_early_track_work_reminder(
        config.session_id.as_deref(),
        *track_nudges,
        crate::agent::tools::todo::unfinished_count(),
        turn_made_file_edits(new_messages),
    ) {
        *track_nudges += 1;
        tally.record_nudge(BoundaryNudge::TrackWork);
        return Some((reminder, BoundaryNudge::TrackWork));
    }
    // 4. Fast verify — edits piling up with nothing run since.
    if let Some(reminder) = build_fast_verify_reminder(
        config.verification_tiers_mode,
        *verify_nudges,
        tier,
        config
            .verifier
            .as_ref()
            .map_or(0, |v| v.edits_since_verify()),
    ) {
        *verify_nudges += 1;
        tally.record_nudge(BoundaryNudge::FastVerify);
        return Some((reminder, BoundaryNudge::FastVerify));
    }
    // 5. File-touch — the same file re-edited over and over.
    if let Some(tracker) = &config.file_touch_tracker
        && let Some(reminder) = tracker.poll_reminder().into_iter().next()
    {
        tally.record_nudge(BoundaryNudge::FileTouch);
        return Some((reminder, BoundaryNudge::FileTouch));
    }
    // 6. Progress — broadest, so last. Prologue and stall are mutually
    //    exclusive inside the tracker (unarmed vs armed).
    if let Some(msg) = progress_signal {
        let which = if super::progress::is_prologue_checkpoint(&msg) {
            BoundaryNudge::ProgressPrologue
        } else {
            BoundaryNudge::ProgressStall
        };
        tally.record_nudge(which);
        return Some((msg, which));
    }
    // 7. Budget countdown. Only polled when nothing else fired, so an unused
    //    mark survives to a later boundary.
    if let Some(progress) = config.progress.as_ref()
        && let Some(msg) = progress.poll_budget(turns_taken, config.max_turns.unwrap_or(0))
    {
        tally.record_nudge(BoundaryNudge::ProgressBudget);
        return Some((msg, BoundaryNudge::ProgressBudget));
    }
    tally.end_boundary();
    None
}

/// One-shot run-finish instrumentation: latch the verifier status and
/// emit the aggregated gate/nudge/capability tally as one `dirge::gates`
/// event. Observation only — no control-flow effect.
fn finish_tally(
    tally: &mut GateTally,
    config: &LoopConfig,
    capability: &super::capability::CapabilityEstimator,
) {
    tally.set_capability_tier(Some(capability.tier()));
    let verification = config
        .verifier
        .as_ref()
        .map(|v| v.status(config.verification_tiers_mode));
    tally.set_verification(verification);
    // dirge-1elu.7: hand this run's status to the post-session pass, which
    // runs on the UI side and cannot see the loop's verifier. Recorded
    // unconditionally — a `None` here CLEARS any prior entry, so a run that
    // verified nothing never inherits an earlier run's green.
    super::verifier::record_run_verification(config.session_id.as_deref(), verification);
    tally.set_repairs(Some(config.repair_stats.snapshot()));
    tally.set_retries(Some(config.retry_stats.snapshot()));
    tally.emit();
}

/// The actual loop. Faithful port of pi `runLoop` (agent-loop.ts:155-269)
/// plus the LOOP-9 `summarize_fn` callback for context-compaction's
/// structured-summary pass. Pass `None` to disable LLM compaction.
///
/// Owns `current_context`, `new_messages`, `config` — pi mutates
/// these as the run proceeds; in Rust we own them by value and
/// return `new_messages` at the end.
#[allow(clippy::too_many_arguments)]
pub async fn run_loop(
    mut current_context: Context,
    mut new_messages: Vec<LoopMessage>,
    // `config` is `mut`: the `prepareNextTurn` hook assigns
    // `config.reasoning` (the thinking-level swap) through this
    // binding, matching pi's `config = { ...config, reasoning }`
    // at agent-loop.ts:229. (Model swap is not yet wired — see
    // the `prepare_next_turn` handler below.)
    mut config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
    summarize_fn: Option<crate::agent::compression::SummarizeFn>,
    // dirge-h5tv: optional memory provider so on_pre_compress fires
    // when the loop auto-folds. `None` is a no-op (test paths,
    // no plugin provider attached).
    memory_provider: Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
) -> Vec<LoopMessage> {
    let mut first_turn = true;

    // Loop-protection guards behind one facade (dirge-hn60). Two engines:
    //   - storm breaker: pre-dispatch, SUPPRESSES a call repeated with
    //     identical args (reset each user turn). Port of Reasonix
    //     `repair/index.ts:38-46` + `loop.ts:621`.
    //   - failure tracker: post-result, NUDGES when errors pile up across
    //     turns (reset by success), catching the thrash storm misses —
    //     a model failing differently every call (dirge-opdt).
    // The facade classifies each result once (Ok/Error/Timeout) and feeds
    // both, so a timeout escalates in each: the tracker counts it double,
    // the storm breaker drops its threshold for that exact call.
    let mut guards = super::activity::LoopGuards::new(
        storm_for_config(&config),
        super::failure_tracker::FailureTracker::new(FAILURE_REFLECTION_THRESHOLD),
    );

    // Inflight set: authoritative running-id tracker.
    // UI cards consult `inflight.has(call_id)` to derive spinner state.
    // Port of Reasonix `loop.ts:147` InflightSet.
    let inflight = InflightSet::new();

    // Multi-tier compaction tracking. Port of Reasonix
    // loop.ts:172 `this._foldedThisTurn`.
    // Reset each new user turn; set true when a fold happens.
    let mut folded_this_turn: bool;

    // Circuit breaker: consecutive summarizer failures this run. After
    // MAX_CONSECUTIVE_COMPACTION_FAILURES, compaction skips the LLM
    // summarizer (cheap pruning still runs). Per-run — a fresh run_loop
    // starts at 0 (IMPROVEMENTS_PLAN #1).
    let mut compaction_failures: u32 = 0;

    // Tokens the pre-send snip freed this iteration. If it freed enough
    // headroom, the post-response NORMAL fold is skipped
    // (IMPROVEMENTS_PLAN #4). Reset after each post-usage decision.
    let mut snip_tokens_freed: u64 = 0;

    // dirge-5mtx.1: per-run gate/capability tally. Declared before the
    // initial steering poll so both steering-poll sites can record into it.
    let mut tally = GateTally::new();
    // dirge-5mtx.7: capability estimation, OBSERVATION ONLY. It is fed at each
    // turn boundary and its tier is latched onto the tally at run end; nothing
    // reads it back to change behaviour. The point is to collect tier
    // distributions across models and scenarios BEFORE deriving any threshold
    // from them — the alternative is picking another constant and calling it
    // adaptive.
    let mut capability = super::capability::CapabilityEstimator::new();
    // dirge-e31n.5: tool calls this run whose effect is Committed or Unknown,
    // and a monotonic ordinal so the model can read the order they happened in.
    // dirge-e31n.6: armed by a permission checkpoint, consumed by the next
    // stream call. See that call site for why it is one-shot.
    let mut pending_tool_choice: Option<super::types::ToolChoice> = None;

    // Pi line 167: initial steering poll.
    // Phase 4 part 2: composes with the file-touch tracker's
    // reminder poll when configured.
    let (mut pending_messages, _initial_user_steering): (Vec<LoopMessage>, bool) =
        // The initial poll runs before any results — user steering only.
        poll_steering(&config).await;

    // dirge-nqr: count assistant turns so a hard cap can stop a
    // runaway run. `max_turns = None` means unlimited (legacy).
    let mut turns_taken: usize = 0;

    // dirge-n00z: call ids for calls lifted out of the model's TEXT, which
    // arrive with none. Monotonic for the whole run and never reset, because
    // an id has to stay unique across the transcript, not just the turn —
    // results are matched back to calls by id, and two empty ones matched
    // each other.
    let mut scavenged_call_seq: usize = 0;

    // F4: in-session reflexion memory. Accumulates the approaches the
    // model looped on and abandoned this run, so the repeat-loop guard
    // can remind it of every dead end (not just the latest repeat).
    // Lives outside the outer loop so it persists across turns.
    let mut reflections = super::reflexion::ReflectionLog::new();

    // dirge-uw2l.4: safe-state abort rung. Off (the default) is byte-identical
    // — decide() short-circuits on Off before any work, so the default loop is
    // untouched. Advisory adds a third failure-ladder rung that re-plans from
    // the last verified-green tree when the failure streak reaches 2× the
    // checkpoint threshold.
    let mut safe_state = super::safe_state::SafeStateEngine::new();

    // dirge-1elu.1: publish-state guard. Off (the default) is byte-identical
    // to the loop without the guard — inspect() short-circuits before any
    // work. Arms at the fresh-green instant from the SAME fingerprint the
    // safe-state rung stamps; persists across turns so verified work from an
    // earlier turn stays protected.
    let mut publish_guard = super::publish_guard::PublishGuard::new();

    // dirge-5mtx.5: every finalization gate's re-fire state, in one place. Each
    // field is labelled cost-ceiling or re-fire-guard on `GateStates` itself —
    // that distinction is what decides which are safe to relax.
    let mut gates = GateStates {
        run_epoch: crate::agent::tools::modified::epoch(),
        ..Default::default()
    };

    // dirge-1g3v: snapshot the working-tree diff at run start so the reviewer
    // can tell what THIS run changed. Without a baseline it diffed the whole
    // dirty tree, so a read-only turn over pre-existing WIP triggered the judge
    // and could block the loop. Only needed when the reviewer is armed.
    let code_review_baseline: Option<super::code_review::RunDiff> =
        if config.code_review_fn.is_some() {
            // dirge-9b2k: same repo-override seam as the finalization poll.
            let repo = config.code_review_repo.clone().unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
            tokio::task::spawn_blocking(move || super::code_review::capture_run_diff(&repo))
                .await
                .ok()
                .flatten()
        } else {
            None
        };

    // Incremental background checkpoint schedule (MiMo 20% cadence).
    // Lazily built on first post-usage check with the live ctx_max; reset
    // after a destructive fold rebuilds the context.
    let mut checkpoint_schedule: Option<context_manager::CheckpointSchedule> = None;

    // Round 1 (fast compaction): the reusable background-checkpoint slot and
    // the fold epoch. Detached checkpoint tasks write the slot; the
    // destructive fold reads it to skip the inline summarizer when a fresh
    // summary is available, and bumps the epoch on success so pre-fold
    // checkpoints go stale.
    let checkpoint_slot: CheckpointSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut checkpoint_generation: u64 = 0;

    // Run-level recovery from a transient mid-stream error (bounded by
    // MAX_TRANSIENT_RECOVERIES). Counts consecutive recoveries so a
    // truly dead network still terminates.
    let mut transient_recoveries: u8 = 0;

    // dirge-1ug5: one-shot runaway-reasoning breaker for this task.
    let mut thinking_breaker = super::thinking_budget::ThinkingBreaker::new();

    let mut verify_nudges: u8 = 0;

    // Compute the issue db path once for both the issue-board reminder and
    // the open-issues gate. Fail-open: absent db → None, gate is inert.
    let issue_db_path: Option<std::path::PathBuf> = {
        let p = std::env::current_dir()
            .map(|c| crate::extras::dirge_paths::ProjectPaths::new(&c).session_db_path())
            .unwrap_or_else(|_| std::path::PathBuf::from(".dirge/sessions/state.db"));
        std::fs::metadata(&p).ok().map(|_| p)
    };

    'outer: loop {
        // Storm: fresh intent on each new user turn.
        // Port of Reasonix loop.ts:621 `this.repair.resetStorm()`.
        guards.reset_turn();
        let mut turn_self_corrected = false;

        // Multi-tier: fresh turn intent — clear fold flag.
        // Port of Reasonix loop.ts:623 `this._foldedThisTurn = false`.
        folded_this_turn = false;

        let mut has_more_tool_calls = true;

        // Pi line 174: INNER LOOP. `recovery_pending` forces one more
        // iteration after a transient mid-stream error is recovered at
        // the run level (see the error/aborted short-circuit below) —
        // the nudge rides in context.messages, so the flag alone drives
        // the next turn when no tool calls or steering are pending.
        let mut recovery_pending = false;
        // dirge-vpma.22: set by the `ExitWithSummary` post-usage tier, which
        // is the last line of defence when context is critically over the
        // threshold. It has to survive to the bottom of the iteration rather
        // than breaking on the spot, so the checkpoint-schedule reset and the
        // per-iteration snip-credit cleanup below it still run.
        //
        // dirge-8s2v: `Some((made_room, prompt_tokens))` — the tier ends the
        // TURN, and whether the run may take another one depends on whether
        // the fold it just ran actually shrank the context. It did: another
        // turn is safe, and the model needs one to see the results of the
        // calls it just made. It did not: going round again meets the same
        // wall, which is what this tier exists to prevent. `prompt_tokens` is
        // carried so the message that reports the stop can name the numbers.
        let mut force_turn_end: Option<(bool, u64)> = None;
        while has_more_tool_calls || !pending_messages.is_empty() || recovery_pending {
            recovery_pending = false;
            // Circuit-breaker bookkeeping is at-most-once per iteration:
            // a single iteration can run BOTH the turn-start fold and the
            // (ungated) post-usage ExitWithSummary pass, and counting two
            // failures from one iteration would open the breaker before
            // the intended 3-round budget (review fix). First record wins.
            let mut compaction_recorded_this_iter = false;

            // The model's context window is constant within one inner-loop
            // iteration — the model can only change at a turn boundary
            // (prepareNextTurn), after the post-usage decision. Look it up
            // once and reuse at all three sites that need it: the turn-start
            // fold, the per-result snip cap, and the post-usage decision.
            // The model's advertised window — an explicit `context_window`
            // config override wins over the built-in lookup table — then
            // capped to the working budget when one is configured (see
            // `context_target` in config.json). By default there is no cap,
            // so the model's advertised window is used as-is. Every
            // downstream tier (fold / snip / turn-start / incremental
            // checkpoint) reads this value.
            let model_window = context_manager::context_window_override().unwrap_or_else(|| {
                config
                    .model_name
                    .as_deref()
                    .and_then(crate::config::context_window_for_model)
                    .unwrap_or(128_000)
            });
            let ctx_max = context_manager::effective_ctx_max(model_window);

            // Pi lines 175-179: turn_start (skipped on very first
            // iteration — the outer wrapper already emitted it).
            if !first_turn {
                let _ = emit.send(LoopEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // dirge-el3n: turn-start (proactive) fold. Reasonix
            // parity at `loop.ts:656-684`. Covers cases the
            // post-response fold can't see — terminal prior turn,
            // session restore, huge paste, long multi-iter turn
            // that crossed the threshold inside one assistant
            // response. Fires when the rough token estimate
            // exceeds `TURN_START_FOLD_THRESHOLD` AND we haven't
            // already folded this turn (the post-response site
            // owns the same flag and is idempotent w.r.t. it).
            //
            // Before-fix: this block only LOGGED — no actual
            // compaction. Long turns ran past the 75/80/90%
            // thresholds without the fold ever firing.
            //
            // Uses the widened `estimate_messages_tokens` so
            // production block-shaped tool results actually
            // count (otherwise array content was 0 and the
            // estimate stayed at 0% forever).
            if !folded_this_turn {
                let rough_estimate =
                    crate::agent::compression::estimate_messages_tokens(&current_context.messages);
                let estimate = context_manager::estimate_turn_start(rough_estimate, ctx_max);
                if estimate.ratio > context_manager::TURN_START_FOLD_THRESHOLD {
                    tracing::info!(
                        target: "dirge::agent_loop",
                        estimate_tokens = %estimate.estimate_tokens,
                        ctx_max = %estimate.ctx_max,
                        ratio = %estimate.ratio,
                        "context-manager: turn-start fold firing ({}% of context)",
                        (estimate.ratio * 100.0) as u32,
                    );
                    let outcome = run_compaction_pass(
                        &mut current_context,
                        &summarize_fn,
                        5, // protect last 5 messages
                        compaction_failures,
                        &memory_provider,
                        config.compaction_hooks.as_ref(),
                        emit,
                        &checkpoint_slot,
                        &mut checkpoint_generation,
                        (ctx_max as f64 * context_manager::HISTORY_FOLD_THRESHOLD) as u64,
                    )
                    .await;
                    if let SummaryOutcome::Succeeded(idx) = outcome {
                        restore_working_files(&config, &mut current_context, idx, ctx_max).await;
                    }
                    if !compaction_recorded_this_iter {
                        record_compaction_outcome(&mut compaction_failures, outcome);
                        compaction_recorded_this_iter = true;
                    }
                    folded_this_turn = true;
                }
            }

            // Round 2 (memory-awareness feedback): if background
            // consolidation wrote new memories since the last turn, re-inject
            // the refreshed memory block here so the running agent becomes
            // aware of them without a restart — the system-prompt memory block
            // is baked at agent-build time and wouldn't otherwise update.
            // Model-facing only: pushed into the live context, not into
            // `new_messages` or persisted session history. The dirty flag is
            // consumed (swap-to-false), so this fires at most once per
            // consolidation. Check provider presence BEFORE consuming the
            // flag: a loop with no memory provider (subagents, many tests)
            // must not swallow the refresh meant for a memory-bearing loop.
            if let Some(provider) = &memory_provider
                && context_manager::take_memories_dirty()
            {
                let block = provider.format_for_system_prompt();
                if !block.trim().is_empty() {
                    // User-role `<system-reminder>`, NOT a system-role message:
                    // the OAuth shaper hoists system-role entries into the
                    // top-level `system` array, which shifts every message byte
                    // after it and re-bills the whole conversation at cache-write
                    // price (dirge-ugah.2). See `memory_refresh_message`.
                    current_context
                        .messages
                        .push(memory_refresh_message(&block));
                }
            }

            // Pi lines 181-189: inject pending steering / follow-up
            // messages.
            if !pending_messages.is_empty() {
                for msg in &pending_messages {
                    let _ = emit
                        .send(LoopEvent::MessageStart {
                            message: msg.clone(),
                        })
                        .await;
                    let _ = emit
                        .send(LoopEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                    current_context.messages.push(loop_message_to_value(msg));
                    new_messages.push(msg.clone());
                    // Phase 4 part 2: record user-originated steering
                    // messages so the file-touch tracker can decide
                    // whether the streak survives the new prompt.
                    // The tracker's OWN reminder message contains
                    // "[Context-depth reminder]" — skip recording
                    // those so they don't reset the streak they just
                    // diagnosed.
                    if let (Some(tracker), LoopMessage::User(u)) = (&config.file_touch_tracker, msg)
                    {
                        let joined = u.text_joined();
                        if !joined.contains("[Context-depth reminder]") {
                            tracker.record_user_message(&joined);
                        }
                    }
                }
                pending_messages.clear();
            }

            // dirge-k6be: cap oversized tool results in the
            // transcript before every model send. Reasonix
            // parity at `loop.ts:486-503` (`healActiveLogBeforeSend`).
            // Idempotent; cheap walk when nothing's over the cap.
            // The fold pass (75% trigger) still does aggressive
            // 1-line summarization — this cap is the per-result
            // safety net so a single 50KB tool output doesn't
            // dominate the prompt until fold fires.
            //
            // Tiered (IMPROVEMENTS_PLAN #3): above 60% estimated context
            // the cap tightens (3000 → 1000 tokens) so a single oversized
            // result can't push the NEXT request over the limit before
            // the (reactive) post-response fold fires.
            let cap_estimate =
                crate::agent::compression::estimate_messages_tokens(&current_context.messages);
            let result_cap = crate::agent::compression::tiered_result_cap(cap_estimate, ctx_max);
            // Counted variant (IMPROVEMENTS_PLAN #4): track how much the
            // snip freed so the post-response fold can be skipped if it
            // bought enough headroom.
            let (capped, freed) = crate::agent::compression::cap_oversized_tool_results_counted(
                &current_context.messages,
                result_cap,
            );
            current_context.messages = capped;
            snip_tokens_freed = snip_tokens_freed.saturating_add(freed);

            // Pi lines 192-194: LLM call.
            // dirge-e31n.6: the permission checkpoint tells the model that
            // "retrying, rephrasing, or switching to another tool will not
            // clear it". `take()` makes that ENFORCEABLE for exactly the turn
            // that reads it, rather than leaving it as advice the model is
            // free to answer with another blocked call.
            //
            // One turn only, by construction: the value is consumed here, so
            // the model is never disarmed for longer than the message it is
            // responding to. If it genuinely needs to read something to write
            // its report, it can on the following turn.
            let turn_tool_choice = pending_tool_choice.take();
            let (mut assistant_msg, token_usage) = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                emit,
                stream_fn,
                turn_tool_choice,
            )
            .await;
            // Where this turn's assistant message sits in each transcript, so
            // a call scavenged out of its text can be recorded ON it further
            // down (dirge-n00z). Both are appended to below, so neither index
            // may be re-derived as "the last one" by then.
            let assistant_in_new = new_messages.len();
            let assistant_in_context = current_context.messages.len().saturating_sub(1);
            new_messages.push(LoopMessage::Assistant(assistant_msg.clone()));

            // dirge-6gpr: a turn has happened — the model was called and
            // answered. Counted HERE rather than at the bottom of the
            // iteration, where it counted only turns the loop went on to
            // iterate past: the last turn of every run was never counted, and
            // a run force-ended by the context manager counted none at all
            // while making tool calls. `turns` is the denominator every other
            // count on the tally line is read against, so a wrong one makes
            // the whole line unreadable rather than merely incomplete.
            tally.record_turn();

            // dirge-ugah.3: report a turn that wrote a cache entry but read
            // none. Evidence-gathering, not a fix — see `is_silent_cache_miss`
            // for the two mechanisms this is meant to distinguish.
            if is_silent_cache_miss(token_usage, current_context.messages.len()) {
                tracing::warn!(
                    target: "dirge::cache",
                    messages = current_context.messages.len(),
                    cache_creation_tokens =
                        token_usage.map_or(0, |u| u.cache_creation_input_tokens),
                    uncached_input_tokens = token_usage.map_or(0, |u| u.input_tokens),
                    "prompt-cache read miss: wrote a new entry, read none. Check the \
                     dirge::prompt_cache target first — a `cached request prefix changed` \
                     warning just before this names the component that moved. Absent that, \
                     suspect the 20-block lookback window (a turn appending >20 content \
                     blocks) or concurrent subagent fan-out",
                );
            }

            // Pi lines 196-200: error / aborted short-circuit.
            if matches!(
                assistant_msg.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                // Run-level recovery: a transient mid-stream failure
                // (network blip, "error decoding response body",
                // rate-limit) that arrived AFTER the model had already
                // streamed content can't be silently retried by the
                // stream layer (the partial is already on screen), but
                // it shouldn't kill the whole run either. The partial is
                // already preserved in the transcript (stream.rs Error
                // arm), so nudge the model to continue and take another
                // turn — bounded by MAX_TRANSIENT_RECOVERIES so a truly
                // dead network still terminates. Aborted (explicit
                // cancel) and non-transient errors (auth, context-length)
                // still terminate as before.
                let transient = assistant_msg.stop_reason == StopReason::Error
                    && transient_recoveries < MAX_TRANSIENT_RECOVERIES
                    && assistant_msg
                        .error_message
                        .as_deref()
                        .map(|e| {
                            use crate::agent::recovery::{ErrorKind, classify_error};
                            matches!(classify_error(e), ErrorKind::Network | ErrorKind::RateLimit)
                        })
                        .unwrap_or(false);
                if transient {
                    transient_recoveries += 1;
                    // LLM-facing only: not routed through pending_messages
                    // (which render as user turns) so it doesn't surface as
                    // a `<you>` line. Mirrors the stall-recovery nudge in
                    // retry.rs.
                    current_context.messages.push(serde_json::json!({
                        "role": "user",
                        "content": TRANSIENT_RECOVERY_NUDGE,
                    }));
                    let _ = emit
                        .send(LoopEvent::RetryNotice {
                            attempt: transient_recoveries as u32,
                            delay_ms: 0,
                            error: assistant_msg
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "transient stream error".to_string()),
                        })
                        .await;
                    // No tool calls ran (the stream errored before
                    // dispatch). Force one more inner iteration so the
                    // nudge drives the next assistant turn.
                    has_more_tool_calls = false;
                    recovery_pending = true;
                    continue;
                }

                let _ = emit
                    .send(LoopEvent::TurnEnd {
                        message: assistant_msg.clone(),
                        tool_results: Vec::new(),
                    })
                    .await;
                finish_tally(&mut tally, &config, &capability);
                let _ = emit
                    .send(LoopEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                return new_messages;
            }

            // dirge-kjyz: MAX_TRANSIENT_RECOVERIES bounds CONSECUTIVE
            // recoveries. Reaching here means the assistant turn completed
            // with a non-error stop reason — the transient blip (if any)
            // recovered — so reset the counter. Otherwise blips spread
            // hours apart across a long autonomous run accumulate and the
            // fourth one hard-fails a perfectly healthy network.
            transient_recoveries = 0;

            // dirge-1ug5: runaway-reasoning breaker. The turn ended out of room
            // with an over-budget reasoning trace and no action to show for it —
            // either the stream's `ReasoningMeter` cut it off, or the provider's
            // own max_tokens did. Either way more thinking is not what the run
            // needs: drop the level to off for the rest of this task (config is
            // owned per run, so the next user prompt starts fresh) and hand the
            // model one instruction to commit.
            //
            // The cap is recomputed here rather than cached at construction:
            // `config.reasoning` can be swapped mid-run by `prepare_next_turn`,
            // and this has to judge the turn against the level it actually ran
            // at — the same value the stream metered it with.
            let turn_reasoning_cap = super::thinking_budget::budget_for_turn(
                config.reasoning,
                config.thinking_budgets.as_ref(),
            );
            if let super::thinking_budget::BreakerAction::ForceOff { nudge } =
                thinking_breaker.inspect(&assistant_msg, turn_reasoning_cap)
            {
                config.reasoning = Some(super::thinking_budget::ThinkingBreaker::forced_level());
                let msg =
                    super::intervention::user_message(super::thinking_budget::THINKING_TAG, nudge);
                current_context.messages.push(loop_message_to_value(&msg));
                emit_harness_notices(emit, std::slice::from_ref(&msg)).await;
                // The turn produced no tool calls, so the loop would otherwise
                // fall out and end the run on a truncated think. Force one more
                // iteration so the nudge actually drives a turn.
                has_more_tool_calls = false;
                recovery_pending = true;
                continue;
            }

            // Pi lines 202-216: tool calls + results.
            let mut tool_calls = extract_tool_calls_from(&assistant_msg);
            // Ids of the calls lifted out of this turn's TEXT, so the
            // assistant message can be made to say it made them (dirge-n00z).
            let mut promoted_ids: Vec<String> = Vec::new();

            // Scavenge: scan reasoning AND regular text content for
            // tool calls the model forgot to emit in `tool_calls`.
            // Port of Reasonix repair/index.ts:71 (`[reasoningContent
            // ?? "", content ?? ""].filter(Boolean).join("\n")`).
            //
            // dirge-ngic: previously only Thinking blocks were
            // scanned. A model emitting <|DSML|invoke …/> in regular
            // content (the common R1-in-content case) was silently
            // missed. Joining Text + Thinking matches Reasonix's
            // dual-channel scan exactly; the scavenger's internal
            // `strip_dsml_blocks` keeps inner-JSON in DSML params
            // from being double-counted.
            //
            // Only tools in the current context's tool set are
            // accepted. Deduplication by (name, args) signature
            // prevents double-counting if the same call appears in
            // both reasoning and declared tool_calls.
            let allowed_names: std::collections::HashSet<String> = current_context
                .tools
                .iter()
                .map(|t| t.name().to_string())
                .collect();
            let scavenge_source = build_scavenge_source(&assistant_msg.content);
            if !scavenge_source.is_empty() {
                let scavenge_result =
                    super::scavenge::scavenge_tool_calls(Some(&scavenge_source), &allowed_names, 4);
                // dirge-e31n.8: a call the model wrote as TEXT naming a tool
                // that does not exist. The scavenger drops it with no result
                // and no error — deliberately, per dirge-knt8 — so this is
                // the only place the loss is visible to anyone.
                for missed in &scavenge_result.unknown_names {
                    tally.record_dropped_unknown_name();
                    super::suggest::log_tool_name_miss(missed, &allowed_names, "scavenged");
                }
                for note in &scavenge_result.notes {
                    tracing::debug!(target: "dirge::agent_loop::scavenge", "{note}");
                }
                if !scavenge_result.calls.is_empty() {
                    // LOOP-12: canonicalize the JSON so different key orders or
                    // numeric reprs (1 vs 1.0) for the same logical call don't
                    // slip past dedupe. `canonical_json` (shared with storm's
                    // repeat-loop detector) sorts keys and normalizes numbers.
                    use super::message::canonical_json;
                    let seen_signatures: std::collections::HashSet<String> = tool_calls
                        .iter()
                        .map(|tc| format!("{}::{}", tc.name, canonical_json(&tc.arguments)))
                        .collect();
                    for sc in &scavenge_result.calls {
                        let sig = format!("{}::{}", sc.name, canonical_json(&sc.arguments));
                        if !seen_signatures.contains(&sig) {
                            // dirge-n00z: a text-channel call arrives with no
                            // id. Mint one now, before anything downstream
                            // keys on it — results, storm signatures and the
                            // publish guard all match calls by id, and two
                            // empty ids matched each other.
                            let sc = {
                                let mut c = sc.clone();
                                scavenged_call_seq += 1;
                                c.id = format!("scav-{scavenged_call_seq}");
                                c
                            };
                            // Every branch below that pushes the call also
                            // records its id, and the one that drops it does
                            // neither — a dropped call must not show up on the
                            // assistant message as one that ran.
                            let promote =
                                |call: super::tools::ToolCall,
                                 calls: &mut Vec<super::tools::ToolCall>,
                                 ids: &mut Vec<String>| {
                                    ids.push(call.id.clone());
                                    calls.push(call);
                                };
                            // dirge-knt8: validate scavenged calls against the
                            // tool's schema BEFORE promotion. Scavenged calls
                            // come from hallucinated text in the model's answer,
                            // not from the provider's native tool_calls.
                            // If validation fails, drop the call silently —
                            // do NOT turn it into an error tool result that
                            // forces a continuation turn (duplicate-response
                            // bug). Native calls are never touched here.
                            let tool = current_context.tools.iter().find(|t| t.name() == sc.name);
                            if let Some(tool) = tool {
                                match crate::agent::agent_loop::tool_input_repair::validate_and_repair(
                                    tool.parameters(),
                                    &sc.arguments,
                                ) {
                                    Ok(None) => {
                                        // Valid — push as-is.
                                        tally.record_scavenged_call();
                                        promote(sc, &mut tool_calls, &mut promoted_ids);
                                    }
                                    Ok(Some(rr)) => {
                                        // Repaired — push with repaired args.
                                        // Kinds are already counted in
                                        // `config.repair_stats`; the tally
                                        // latches that snapshot at run end.
                                        tally.record_scavenged_call();
                                        let mut repaired_call = sc;
                                        repaired_call.arguments = rr.repaired;
                                        promote(
                                            repaired_call,
                                            &mut tool_calls,
                                            &mut promoted_ids,
                                        );
                                    }
                                    Err(_) => {
                                        // Invalid scavenged call — drop silently.
                                        // This was hallucinated text, not a real
                                        // tool call the model intended to dispatch.
                                    }
                                }
                            } else {
                                // Defensive: tool not found — unreachable, since
                                // allowed_names is built from this same tool set.
                                // Preserve prior behavior and push the call as-is.
                                tally.record_scavenged_call();
                                promote(sc, &mut tool_calls, &mut promoted_ids);
                            }
                        }
                    }
                }
            }

            // dirge-e31n.8: place names the model wrote for a tool dirge
            // calls something else — `shell` for `bash`, `ask_user` for
            // `question`, `Bash` for `bash`. ONE site, covering native and
            // scavenged calls alike, and it rewrites the name in place so
            // everything downstream — storm signatures, the tally, the event
            // stream, the tool result — sees the tool that actually ran.
            //
            // Before storm and before truncation repair, so a guessed name
            // and its real spelling cannot survive as two distinct calls.
            for (guessed, real) in
                super::tool_aliases::resolve_call_names(&mut tool_calls, &allowed_names)
            {
                tally.record_aliased_tool_name();
                tracing::info!(
                    target: "dirge::tool_miss",
                    tool = %guessed,
                    resolved = %real,
                    path = "aliased",
                    "resolved a tool name the model wrote differently",
                );
            }

            // dirge-n00z: record the lifted calls ON the assistant message.
            // After alias resolution, so the block carries the name that
            // actually ran; before storm and dispatch, so every id here is
            // one `backfill_missing_tool_results` will guarantee a result for.
            if !promoted_ids.is_empty() {
                let lifted: Vec<super::tools::ToolCall> = tool_calls
                    .iter()
                    .filter(|c| promoted_ids.iter().any(|id| id == &c.id))
                    .cloned()
                    .collect();
                assistant_msg =
                    super::call_syntax::absorb_text_calls(&assistant_msg, &lifted, &allowed_names);
                let rewritten = LoopMessage::Assistant(assistant_msg.clone());
                if let Some(slot) = current_context.messages.get_mut(assistant_in_context) {
                    *slot = loop_message_to_value(&rewritten);
                }
                if let Some(slot) = new_messages.get_mut(assistant_in_new) {
                    *slot = rewritten;
                }
            }

            // dirge-7bwx: truncation repair runs BEFORE storm
            // filter. Port of Reasonix's pipeline order at
            // `repair/index.ts:88-109` (truncation) then
            // `:113-121` (storm). Previously dirge ran the
            // closer inside `validate_and_repair` at dispatch
            // time — after storm. That meant two calls whose
            // args strings both truncate to the same repaired
            // form survived storm (different pre-repair
            // signatures), then dispatched identically. Doing
            // the repair here lets storm see the canonical
            // post-repair signature and dedupe correctly.
            //
            // Hard-fallback (closer can't rebalance the stack)
            // leaves `arguments` as the original Value::String;
            // validate_and_repair downstream will surface that
            // as a real validation error rather than silently
            // dispatching a fabricated `{}` — same invariant
            // Reasonix maintains at `repair/index.ts:93-102`.
            apply_truncation_repair(
                &mut tool_calls,
                &config.repair_stats,
                &config.truncation_notes,
            );

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            // Storm-breaker: when the run gives up because it's stuck
            // looping, the tool names it looped on — used to synthesize
            // a graceful assistant explanation after the turn's results
            // are backfilled (below). None unless the terminal-stuck
            // branch fires.
            let mut storm_give_up_tools: Option<Vec<String>> = None;
            if !tool_calls.is_empty() {
                let original_count = tool_calls.len();
                let (mut surviving_calls, storm_report) = guards.inspect_calls(&tool_calls);
                for _ in 0..storm_report.storms_broken {
                    tally.record_storm_suppression();
                }
                let all_suppressed = storm_report.all_suppressed(original_count);

                // Port of Reasonix loop.ts:935-956 — first-time
                // all-suppressed: self-correction. Stub tool
                // results with a guard message and give the model
                // one shot to self-correct before the loud-warning
                // path.
                if all_suppressed && !turn_self_corrected {
                    turn_self_corrected = true;
                    // Reflect-then-pivot intervention. Just telling a
                    // model "try again" tends to reinforce the same
                    // failing chain (degeneration-of-thought / mental-set);
                    // an effective unstick prompt forces it to first
                    // diagnose, then DIVERGE — a different tool, entry
                    // point, or assumption — and gives explicit permission
                    // to stop. See docs/agent-loop.md.
                    const REPEAT_LOOP_GUARD: &str = "[repeat-loop guard] You've made this exact call more than once and gotten the same result — you're stuck in a loop. Do NOT repeat it. Before doing anything else, work through these steps:\n\
                        1. State what you were trying to achieve with this call and why it isn't getting you there.\n\
                        2. Look at the earlier results for it above. What assumption of yours might be wrong, and what do those results actually tell you?\n\
                        3. Propose 2-3 FUNDAMENTALLY different approaches — a different tool, a different entry point, or a different interpretation of the problem — and pick the most promising one.\n\
                        4. Proceed with that approach.\n\
                        If none of them can work with the tools available, say so plainly and report what you found instead of trying again.";
                    // F4: record each looped call as an abandoned approach,
                    // then append the running list so the model sees every
                    // dead end it has hit this run, not just this repeat.
                    for call in &tool_calls {
                        // dirge-r78m: key on canonical (key-sorted) JSON, the
                        // same normalization storm + scavenge dedup use, so two
                        // logically-identical calls with different key order
                        // don't show up twice in the abandoned-approaches list.
                        let args = super::message::canonical_json(&call.arguments);
                        let sig = super::reflexion::approach_signature(&call.name, &args);
                        reflections.record(sig);
                    }
                    let guard_text = format!(
                        "{REPEAT_LOOP_GUARD}{}",
                        reflections.block().unwrap_or_default()
                    );
                    let guard_blocks = vec![ContentBlock::Text {
                        text: guard_text.clone(),
                    }];
                    for call in &tool_calls {
                        let tr = ToolResultMessage {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            content: guard_blocks.clone(),
                            details: Value::Null,
                            is_error: false,
                        };
                        current_context.messages.push(tool_result_to_value(&tr));
                        new_messages.push(LoopMessage::ToolResult(tr.clone()));
                        tool_results.push(tr);
                    }
                    // Surface the self-correction as a tool result
                    // with a guard text — the model sees it as
                    // output for its suppressed tool calls.
                    has_more_tool_calls = true;
                } else if storm_report.storms_broken > 0 && surviving_calls.is_empty() {
                    // Port of Reasonix loop.ts:975-982:
                    // no calls left, all suppressed and already
                    // self-corrected. Model is stuck — no more
                    // tool calls to dispatch, exit the inner
                    // loop.
                    has_more_tool_calls = false;
                    // Storm-breaker: rather than end on an abrupt/empty
                    // stop, synthesize a coherent assistant explanation
                    // (built after backfill, below).
                    storm_give_up_tools = Some(tool_calls.iter().map(|c| c.name.clone()).collect());
                }

                // dirge-1elu.1: publish-state guard — pre-dispatch, after the
                // storm breaker. `blocking` drops the call (an error result
                // naming the protected paths is backfilled below); `advisory`
                // lets the call run but injects a tagged, model-visible
                // warning, bounded at 2 per run then silent. Off passes
                // everything through untouched.
                let mut blocked: Vec<(
                    super::tools::ToolCall,
                    super::publish_guard::PublishVerdict,
                )> = Vec::new();
                let mut warned: Vec<(
                    super::tools::ToolCall,
                    super::publish_guard::PublishVerdict,
                )> = Vec::new();
                if config.publish_guard_mode != super::types::GateMode::Off {
                    for call in &surviving_calls {
                        match publish_guard.inspect(config.publish_guard_mode, call) {
                            super::publish_guard::PublishVerdict::Pass => {}
                            v @ super::publish_guard::PublishVerdict::Hit {
                                block: true, ..
                            } => {
                                blocked.push((call.clone(), v));
                            }
                            v @ super::publish_guard::PublishVerdict::Hit {
                                block: false, ..
                            } => {
                                warned.push((call.clone(), v));
                            }
                        }
                    }
                    if !blocked.is_empty() {
                        let blocked_ids: std::collections::HashSet<&str> =
                            blocked.iter().map(|(c, _)| c.id.as_str()).collect();
                        surviving_calls.retain(|c| !blocked_ids.contains(c.id.as_str()));
                    }
                    // Advisory warnings are model-visible User messages tagged
                    // like the other harness injections, so emit_harness_notices
                    // mirrors them to a SystemNotice for headless consumers.
                    if !warned.is_empty() {
                        let mut body = format!(
                            "{} Advisory: a command in this batch would discard verified-green work. \
                             It is being allowed to run (advisory mode) — make a scratch copy under \
                             /tmp if you meant to clean up:\n",
                            super::publish_guard::PUBLISH_GUARD_TAG
                        );
                        for (call, verdict) in &warned {
                            if let super::publish_guard::PublishVerdict::Hit {
                                protected,
                                reason,
                                ..
                            } = verdict
                            {
                                let command = call
                                    .arguments
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let paths = protected
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                body.push_str(&format!(
                                    "- `{command}` ({reason}) discards: {paths}\n"
                                ));
                            }
                        }
                        let msg = LoopMessage::User(super::message::UserMessage::text(body));
                        emit_harness_notices(emit, std::slice::from_ref(&msg)).await;
                        current_context.messages.push(loop_message_to_value(&msg));
                        new_messages.push(msg);
                    }
                }

                // Dispatch surviving calls through the unified dispatch.
                // `execute_tool_calls` takes pre-extracted tool calls.
                if !surviving_calls.is_empty() {
                    let batch = super::tools::execute_tool_calls(
                        &current_context,
                        &assistant_msg,
                        &surviving_calls,
                        &config,
                        &signal,
                        emit,
                        &inflight,
                    )
                    .await;
                    tool_results.extend(batch.messages.clone());
                    has_more_tool_calls = !batch.terminate;
                    // dirge-5mtx.7: the tool set this batch was dispatched
                    // against, for the hallucinated-name classification below.
                    // Built once per batch rather than per result.
                    let known_tool_names: Vec<&str> =
                        current_context.tools.iter().map(|t| t.name()).collect();
                    for result in &batch.messages {
                        // Classify + feed both guards. Match the result back
                        // to its originating call so a timeout can be tied to
                        // the exact signature the storm breaker will see on a
                        // retry. `surviving_calls` are the dispatched ones, so
                        // the id lookup hits; fall back to a name-only call if
                        // it somehow doesn't (defensive — still feeds the
                        // failure tracker, just no storm signature).
                        let excerpt = tool_result_excerpt(&result.content);
                        let originating = surviving_calls
                            .iter()
                            .find(|c| c.id == result.tool_call_id)
                            .cloned()
                            .unwrap_or_else(|| super::tools::ToolCall {
                                id: result.tool_call_id.clone(),
                                name: result.tool_name.clone(),
                                arguments: serde_json::Value::Null,
                            });
                        guards.record_result(&originating, result.is_error, &excerpt);
                        record_tool_result_signals(
                            &mut tally,
                            &originating.name,
                            result.is_error,
                            &excerpt,
                            &known_tool_names,
                        );
                        // dirge-e31n.5: what this call may have LANDED. Only
                        // effects worth carrying are kept — a `NoEffect` fact
                        // in a handoff about what might have committed is
                        // noise that dilutes the ones that matter.
                        let effect = super::side_effect::classify_result(
                            &originating.name,
                            result.is_error,
                            &excerpt,
                        );
                        // OBSERVATION ONLY (dirge-e31n.5). The envelope block
                        // this used to feed was cut on the evidence: four model
                        // configurations across the supported range, two
                        // scenarios, 53 runs, and the control arm never failed.
                        // The counter stays because it costs nothing and it is
                        // the mechanism gate that made those three rounds
                        // legible at all -- without it a scenario that failed
                        // to produce the condition reads as a null result.
                        if effect == super::side_effect::SideEffect::Unknown {
                            tally.record_unresolved_effect();
                        }
                        tally.record_failure_streak(guards.failure_streak() as u32);
                        current_context.messages.push(tool_result_to_value(result));
                        new_messages.push(LoopMessage::ToolResult(result.clone()));
                    }
                }

                // dirge-1elu.1: publish-blocked calls return an error result,
                // so history stays well-formed and the model sees WHY the call
                // was suppressed and what to do instead. Because these ids are
                // already covered, the dirge-tc4r backfill below synthesizes
                // nothing for them.
                for (call, verdict) in &blocked {
                    if let super::publish_guard::PublishVerdict::Hit {
                        protected, reason, ..
                    } = verdict
                    {
                        let command = call
                            .arguments
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let paths = protected
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let text = format!(
                            "{} The command `{command}` ({reason}) would discard verified-green work \
                             ({paths}) and was blocked. Make a scratch copy under /tmp if you meant to \
                             clean up — the verified work is not recoverable once discarded.",
                            super::publish_guard::PUBLISH_GUARD_TAG
                        );
                        let tr = ToolResultMessage {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            content: vec![ContentBlock::Text { text }],
                            details: Value::Null,
                            is_error: true,
                        };
                        current_context.messages.push(tool_result_to_value(&tr));
                        new_messages.push(LoopMessage::ToolResult(tr.clone()));
                        tool_results.push(tr);
                    }
                }

                // dirge-tc4r: guarantee a result for EVERY tool_call_id in
                // the assistant message. Partial storm suppression and a
                // cancelled/interrupted batch both append fewer results
                // than there were calls, orphaning an id — which makes the
                // NEXT provider request 400. Backfill a synthetic error
                // result so history stays well-formed and the model sees
                // the gap instead of the user seeing a raw 400.
                for tr in super::tools::backfill_missing_tool_results(&tool_calls, &tool_results) {
                    current_context.messages.push(tool_result_to_value(&tr));
                    new_messages.push(LoopMessage::ToolResult(tr.clone()));
                    tool_results.push(tr);
                }

                // Storm-breaker graceful failure: the run is giving up
                // because it looped. Now that every suppressed call has
                // a backfilled result (history is well-formed), append a
                // first-person assistant message explaining the stop, so
                // the user sees a coherent reply instead of an empty turn
                // and the model carries its own failure account forward.
                if let Some(tools) = storm_give_up_tools.take() {
                    let text = super::storm::failure_narrative(&tools);
                    let msg =
                        AssistantMessage::new(vec![ContentBlock::Text { text }], StopReason::Stop);
                    // Render it to the user (text flows via MessageUpdate).
                    let _ = emit
                        .send(LoopEvent::MessageStart {
                            message: LoopMessage::Assistant(msg.clone()),
                        })
                        .await;
                    let _ = emit
                        .send(LoopEvent::MessageUpdate {
                            message: msg.clone(),
                            phase: super::message::DeltaPhase::TextStart,
                        })
                        .await;
                    let _ = emit
                        .send(LoopEvent::MessageEnd {
                            message: LoopMessage::Assistant(msg.clone()),
                        })
                        .await;
                    // Record in history so it persists for the next turn.
                    current_context
                        .messages
                        .push(loop_message_to_value(&LoopMessage::Assistant(msg.clone())));
                    new_messages.push(LoopMessage::Assistant(msg));
                }
            }

            // dirge-5mtx.7: feed the capability estimator. Repair counts come
            // from the per-run RepairStats on the config (the tally latches
            // that snapshot only at run end), everything else off the tally.
            {
                let repairs = config.repair_stats.snapshot();
                capability.observe(&super::capability::CapabilityCounters {
                    tool_calls: tally.tool_calls(),
                    errored_by_class: tally.errored_by_class(),
                    repair_invalid: repairs.invalid as u32,
                    repair_successful: repairs.total_successful() as u32,
                    hallucinated_tool_names: tally.hallucinated_tool_names(),
                    storm_suppressions: tally.storm_suppressions(),
                    scavenged_calls: tally.scavenged_calls(),
                    max_failure_streak: tally.max_failure_streak(),
                });
            }

            // dirge-5mtx.2: ONE harness nudge per boundary, chosen by
            // `poll_boundary_nudge` in strict priority. These used to be three
            // independent pushes here plus three more inside the steering poll,
            // so up to five could stack before a single assistant turn.
            //
            // The safe-state decision is computed here (it was at the steering
            // poll) because the arbiter ranks it against the checkpoint it
            // supersedes. Every input it reads — the guards, the verifier, the
            // reflexion log — is already current at this point: tool results
            // were recorded above.
            // dirge-1elu.1: the publish-state guard and the safe-state rung
            // arm from ONE fingerprint, taken at the fresh-green instant — a
            // single git sample is the source of truth for "what this run
            // changed at green". A later fresh-green replaces the protected
            // set; going stale (an edit after green) never clears it.
            let fresh_green = config.verifier.as_ref().is_some_and(|v| v.is_fresh_green());
            if fresh_green
                && (config.publish_guard_mode != super::types::GateMode::Off
                    || config.safe_state_abort_mode == super::types::SafeStateMode::Auto)
            {
                let repo = safe_state_repo(&config);
                let fp = repo.as_deref().and_then(super::worktree_probe::fingerprint);
                publish_guard.arm(fp.clone(), repo);
                safe_state.set_green_fingerprint(fp);
            }

            let safe_state_msg = if config.safe_state_abort_mode != super::types::SafeStateMode::Off
            {
                let excerpts = guards.recent_excerpts();
                let green_fp = safe_state.green_fingerprint().cloned();
                let repo = safe_state_repo(&config);
                safe_state.decide(
                    config.safe_state_abort_mode,
                    fresh_green,
                    guards.safe_state_due(),
                    config
                        .verifier
                        .as_ref()
                        .map_or(0, |v| v.edits_since_verify()),
                    crate::agent::tools::snapshots::current_turn_id().as_deref(),
                    &reflections,
                    &excerpts,
                    |green| coverage_verified_restore(repo.as_deref(), green_fp.as_ref(), green),
                )
            } else {
                None
            };
            if let Some((msg, which)) = poll_boundary_nudge(
                &config,
                &guards,
                safe_state_msg,
                &new_messages,
                turns_taken,
                &mut gates.track_nudges,
                &mut verify_nudges,
                &mut tally,
                capability.tier(),
            ) {
                // dirge-e31n.6: a permission checkpoint says no tool can clear
                // the block. Forbid tools on the turn that reads it so that is
                // a fact rather than a request. Every other nudge leaves the
                // model free to act — several of them are ASKING it to.
                if which == BoundaryNudge::PermissionCheckpoint {
                    pending_tool_choice = Some(super::types::ToolChoice::None);
                }
                emit_harness_notices(emit, std::slice::from_ref(&msg)).await;
                current_context.messages.push(loop_message_to_value(&msg));
                new_messages.push(msg);
            }

            // Pi line 218: turn_end.
            let _ = emit
                .send(LoopEvent::TurnEnd {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                })
                .await;

            // Reasonix loop.ts:987-1032 — context-manager decision
            // after each turn's response. Thresholds:
            //   >80% → exit-with-summary (defense in depth)
            //   >78% → aggressive fold (half tail budget)
            //   >75% → normal fold
            //   ≤75% → carry on
            //
            // `prompt_tokens` comes from the provider's usage report
            // (`token_usage`); it is None only when the provider
            // doesn't report usage, in which case the decision
            // defaults to None (carry on).
            {
                let decision = context_manager::decide_after_usage(
                    token_usage.map(|u| u.input_tokens),
                    ctx_max,
                    folded_this_turn,
                );
                // dirge-vlfb: the context verdict, every turn, whatever it is.
                // The `None` arm matters as much as the others — "the context
                // manager looked and did nothing" is the answer to most of the
                // questions a fold-related bug raises, and it is the one
                // outcome that logs nothing at all.
                if super::trace::enabled() {
                    super::trace::note(
                        "context",
                        serde_json::json!({
                            "verdict": format!("{:?}", decision.kind),
                            "prompt_tokens": decision.prompt_tokens,
                            "ctx_max": decision.ctx_max,
                            "ratio": decision.ratio,
                            "aggressive": decision.aggressive,
                            "already_folded": folded_this_turn,
                        }),
                    );
                }
                match decision.kind {
                    PostUsageDecisionKind::Fold if !folded_this_turn => {
                        folded_this_turn = true;
                        // IMPROVEMENTS_PLAN #4: if the pre-send snip
                        // already freed enough headroom, skip a NORMAL
                        // fold this turn (aggressive folds still fire).
                        // This is the "snip override" composed here rather
                        // than inside the decision engine — see the budget
                        // ladder doc in agent_loop::context_manager.
                        if crate::agent::compression::snip_bought_enough(
                            snip_tokens_freed,
                            ctx_max,
                            decision.aggressive,
                        ) {
                            tracing::info!(
                                target: "dirge::agent_loop",
                                freed = snip_tokens_freed,
                                ratio = %decision.ratio,
                                "snip freed {snip_tokens_freed} tokens — sufficient, skipping fold",
                            );
                        } else {
                            tracing::info!(
                                target: "dirge::agent_loop",
                                ratio = %decision.ratio,
                                // dirge-cprj: the ratio alone cannot say
                                // whether the prompt is too big or the window
                                // is too small, and those want opposite fixes.
                                prompt_tokens = decision.prompt_tokens,
                                ctx_max = decision.ctx_max,
                                aggressive = decision.aggressive,
                                tail_budget = ?decision.tail_budget,
                                "context-manager: fold recommended ({})",
                                if decision.aggressive { "aggressive" } else { "normal" },
                            );

                            // Context compaction: prune old tool results and
                            // compress the middle section of the conversation.
                            // Port of Hermes's compression pass.
                            if let Some(prompt_tokens) = token_usage.map(|u| u.input_tokens)
                                && crate::agent::compression::should_compress(
                                    prompt_tokens,
                                    ctx_max,
                                )
                            {
                                let outcome = run_compaction_pass(
                                    &mut current_context,
                                    &summarize_fn,
                                    5, // protect last 5 messages
                                    compaction_failures,
                                    &memory_provider,
                                    config.compaction_hooks.as_ref(),
                                    emit,
                                    &checkpoint_slot,
                                    &mut checkpoint_generation,
                                    (ctx_max as f64 * context_manager::HISTORY_FOLD_THRESHOLD)
                                        as u64,
                                )
                                .await;
                                if let SummaryOutcome::Succeeded(idx) = outcome {
                                    restore_working_files(
                                        &config,
                                        &mut current_context,
                                        idx,
                                        ctx_max,
                                    )
                                    .await;
                                }
                                // Guard against double-counting if a
                                // turn-start fold already recorded this
                                // iteration. No write-back needed — only one
                                // post-usage arm runs and the iteration ends
                                // right after.
                                if !compaction_recorded_this_iter {
                                    record_compaction_outcome(&mut compaction_failures, outcome);
                                }
                            }
                        }
                    }
                    PostUsageDecisionKind::ExitWithSummary => {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            ratio = %decision.ratio,
                            // dirge-cprj. This line is where a run that is
                            // silently force-ending every turn shows up, and
                            // without these two it cannot be told from a run
                            // legitimately out of room.
                            prompt_tokens = decision.prompt_tokens,
                            ctx_max = decision.ctx_max,
                            "context-manager: forcing summary and ending turn",
                        );
                        // dirge-vpma.22: and actually end it. This arm logged
                        // "ending turn" and then fell through to
                        // prepareNextTurn/steering with `has_more_tool_calls`
                        // untouched, so the turn continued. That is the exact
                        // case this tier exists to prevent: when the summarizer
                        // fails or the circuit breaker is already open at >80%
                        // context, the loop went round again against a context
                        // still over the threshold and the next request could
                        // overflow or 400. Honoured below, after the
                        // checkpoint-schedule reset.
                        // When context is critically over the threshold,
                        // prune aggressively then run the structured-summary
                        // pass if a summarizer is wired.
                        let outcome = run_compaction_pass(
                            &mut current_context,
                            &summarize_fn,
                            3, // protect only last 3
                            compaction_failures,
                            &memory_provider,
                            config.compaction_hooks.as_ref(),
                            emit,
                            &checkpoint_slot,
                            &mut checkpoint_generation,
                            (ctx_max as f64 * context_manager::HISTORY_FOLD_THRESHOLD) as u64,
                        )
                        .await;
                        // dirge-8s2v: `Succeeded` is the only outcome that
                        // moved anything — `Failed` and `Skipped` both leave
                        // the context exactly as it was.
                        force_turn_end = Some((
                            matches!(outcome, SummaryOutcome::Succeeded(_)),
                            decision.prompt_tokens,
                        ));
                        if let SummaryOutcome::Succeeded(idx) = outcome {
                            restore_working_files(&config, &mut current_context, idx, ctx_max)
                                .await;
                        }
                        if !compaction_recorded_this_iter {
                            record_compaction_outcome(&mut compaction_failures, outcome);
                        }
                    }
                    _ => {}
                }
                // Incremental background checkpoint (MiMo 20% cadence):
                // when NOT folding, refresh the durable checkpoint at each
                // newly-crossed usage threshold so a later resume/overflow
                // recovers a fresh state. Non-destructive — the summary is
                // generated off the loop and written by the consumer
                // without touching the live context. A destructive fold
                // re-arms the schedule (the context was rebuilt).
                if context_manager::incremental_checkpoint_enabled()
                    && let Some(sfn) = &summarize_fn
                {
                    let sched = checkpoint_schedule
                        .get_or_insert_with(|| context_manager::CheckpointSchedule::new(ctx_max));
                    match decision.kind {
                        PostUsageDecisionKind::Fold | PostUsageDecisionKind::ExitWithSummary => {
                            sched.reset()
                        }
                        PostUsageDecisionKind::None => {
                            if sched.is_enabled() && sched.note_usage(decision.ratio) {
                                spawn_incremental_checkpoint(
                                    sfn.clone(),
                                    current_context.messages.clone(),
                                    emit.downgrade(),
                                    checkpoint_slot.clone(),
                                    checkpoint_generation,
                                );
                            }
                        }
                    }
                }
                // Snip credit is per-iteration: it informed THIS post-usage
                // decision; clear it so a later iteration's fold isn't
                // suppressed by a stale snip (IMPROVEMENTS_PLAN #4).
                snip_tokens_freed = 0;
            }

            // dirge-vpma.22: the critical-context tier asked for the turn to
            // end. Clearing `has_more_tool_calls` would not be enough on its
            // own — the loop condition also admits pending messages and a
            // pending recovery — so leave the inner loop, which leaves the
            // flag unread.
            //
            // dirge-8s2v: but leaving the inner loop is leaving the TURN loop,
            // and control then falls to the finalization poll, whose default
            // is to stop. As first written this ended the RUN: a model over
            // the threshold got one turn, its tool calls ran, their results
            // were appended, and the run finished before it ever saw them.
            // `continue 'outer` is the difference between ending the turn and
            // ending the run — it starts a fresh turn against the context the
            // fold just shrank.
            //
            // Only when the fold made room. When it did not, another turn
            // meets the same wall, and the honest move is to stop — but to say
            // so, because a run that stops mid-task in silence is
            // indistinguishable from one that decided it was finished.
            if let Some((made_room, prompt_tokens)) = force_turn_end.take() {
                turns_taken += 1;
                let under_cap = config.max_turns.is_none_or(|cap| turns_taken < cap);
                if made_room && under_cap {
                    continue 'outer;
                }
                if !made_room {
                    let notice = format!(
                        "Run stopped: the context is over the model's window \
                         ({} of {} tokens) and compaction could not reduce it. \
                         The task is unfinished. This usually means the system \
                         prompt and tool schemas alone exceed the window — \
                         check the model's context_window in config.",
                        prompt_tokens, ctx_max,
                    );
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        prompt_tokens = prompt_tokens,
                        ctx_max = ctx_max,
                        "run truncated: context over window and nothing left to fold",
                    );
                    let _ = emit
                        .send(LoopEvent::SystemNotice {
                            content: notice.clone(),
                        })
                        .await;
                    new_messages.push(LoopMessage::User(super::message::UserMessage::text(notice)));
                }
                break;
            }

            // Pi lines 220-239: prepareNextTurn.
            if let Some(hook) = &config.prepare_next_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if let Some(update) = hook(hook_ctx).await {
                    // Pi line 228: `context: snapshot.context ??
                    // currentContext`. Apply only `Some`.
                    if let Some(new_ctx) = update.context {
                        current_context = new_ctx;
                    }
                    // dirge-6js7 plugin review: apply the requested
                    // thinking level to subsequent turns.
                    // `config.reasoning` is read per-turn when
                    // building `StreamOptions` (stream.rs:173) and
                    // mapped into the provider request, so reassigning
                    // it here takes effect on the NEXT stream call —
                    // pi's `prepareNextTurn` thinking-swap semantics
                    // (agent-loop.ts:229). Previously this value was
                    // dropped with a "not yet wired" warning, making
                    // the plugin `harness/set-next-thinking-level`
                    // slot a silent no-op in the pi-style loop.
                    if let Some(level) = update.thinking_level {
                        config.reasoning = Some(level);
                        tracing::debug!(
                            target: "dirge::agent_loop",
                            thinking = ?level,
                            "prepareNextTurn applied a new thinking_level for the next turn",
                        );
                    }
                    // Mid-run MODEL swap still requires restructuring
                    // the loop to accept a `Fn(Context) -> StreamFn`
                    // factory (the StreamFn bakes the CompletionModel
                    // at construction and isn't part of LoopConfig).
                    // Tracked separately; warn so a plugin author
                    // knows the model swap was ignored.
                    if let Some(model) = &update.model {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            requested_model = %model,
                            "prepareNextTurn returned a new model but mid-run model swap is not yet wired — ignoring",
                        );
                    }
                }
            }

            // Pi lines 241-251: shouldStopAfterTurn.
            if let Some(hook) = &config.should_stop_after_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if hook(hook_ctx).await {
                    finish_tally(&mut tally, &config, &capability);
                    let _ = emit
                        .send(LoopEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await;
                    return new_messages;
                }
            }

            // Pi line 253: refresh steering for the next iteration. User
            // input only — the safe-state decision and every other harness
            // nudge are chosen by `poll_boundary_nudge` above (dirge-5mtx.2).
            let (next_pending, had_user_steering) = poll_steering(&config).await;
            pending_messages = next_pending;
            // dirge-uw2l.7: surface harness steers to headless consumers.
            emit_harness_notices(emit, &pending_messages).await;

            // dirge-st8r: a fresh USER steering message means the human is
            // actively driving the run — give them a fresh turn budget
            // rather than killing their work with the runaway-loop cap.
            // Genuine runaway loops are caught by the storm breaker, not
            // this counter; the cap is a cost ceiling for AUTONOMOUS runs,
            // and an explicit human interjection is a choice to continue.
            // Only the user's own queued steering resets it — file-touch
            // reminders and plugin/critic follow-ups do not.
            if had_user_steering {
                turns_taken = 0;
            }

            // dirge-nqr: cap reached → emit a system-visible note,
            // append a user-facing message into the transcript so the
            // model's history reflects the truncation, and bail.
            turns_taken += 1;
            if let Some(cap) = config.max_turns
                && turns_taken >= cap
            {
                tracing::warn!(
                    target: "dirge::agent_loop",
                    turns = turns_taken,
                    cap = cap,
                    "max_turns reached — terminating run"
                );
                let notice = max_turns_notice(cap, &crate::agent::tools::todo::snapshot());
                // Surface to the user as a `<system>` log line (warning
                // color) rather than a `MessageStart { User }` — the
                // latter rendered with the `<you>` prefix as if the user
                // had typed it.
                //
                // dirge-1elu.4 audit (site 4): deliberately notice-only and
                // never a steering message — the run is ending, there is no
                // next model turn to read one. The transcript entry below is
                // a record of truncation for callers, not a steer.
                let _ = emit
                    .send(LoopEvent::SystemNotice {
                        content: notice.clone(),
                    })
                    .await;
                // Also include it in `run_agent_loop`'s returned message
                // list so a caller that inspects the produced messages can
                // see the run was truncated. NOTE: the interactive and
                // headless paths drive display from the LoopEvent stream
                // (the SystemNotice above), not from this return value —
                // today's production callers discard it — so this is a
                // contract nicety, not the display mechanism.
                new_messages.push(LoopMessage::User(super::message::UserMessage::text(notice)));
                break 'outer;
            }

            // dirge-j4dz: honor a graceful interjection at the tool-result
            // boundary. The post-inner-loop check below only runs once the
            // model STOPS emitting tool calls; a run that keeps calling
            // tools (e.g. after a permission-denial cascade calls
            // `interject()`) would otherwise never observe it and keep
            // taking turns. History is well-formed here — this turn's tool
            // results are appended and any missing ones backfilled — so
            // breaking now is safe. Falls through to the outer break.
            if signal.is_interjected() {
                break;
            }
        }
        // INNER END

        // LOOP-4: check for graceful interjection at the turn
        // boundary. In-flight tools already completed normally
        // (they never check `is_interjected()`). Stop here rather
        // than starting a new turn or processing follow-ups.
        if signal.is_interjected() {
            break;
        }

        // Outer-loop finalization poll (pi lines 256-262): the single
        // priority-ordered authority for follow-up interjections —
        // hook → verifier → unified judge → goal → todo, at most one per
        // finalization.
        let (follow_up, source) = poll_finalization_follow_up(
            &config,
            &current_context.system_prompt,
            &new_messages,
            &mut gates,
            GateInputs {
                code_review_baseline: code_review_baseline.as_ref(),
                open_issues_gate_mode: config.open_issues_gate_mode,
                issue_db_path: issue_db_path.as_deref(),
                session_id: config.session_id.as_deref(),
            },
            emit,
        )
        .await;
        // dirge-1elu.6: one finalization boundary — the source gate (and
        // any future co-firing gate) becomes one co-occurrence event.
        tally.begin_boundary();
        tally.record_gate(source.into());
        tally.end_boundary();
        if !follow_up.is_empty() {
            tracing::trace!(target: "dirge::loop", ?source, "finalization follow-up interjected");
            emit_harness_notices(emit, &follow_up).await;
            pending_messages = follow_up;
            continue 'outer;
        }
        break;
    }

    // Phase-1 telemetry (docs/AGENTIC_LOOP_PLAN.md): emit the
    // per-run repair counter snapshot just before AgentEnd, but
    // only when at least one repair fired or one input was
    // invalid. Empty snapshots are skipped so the UI doesn't
    // print "repaired 0 inputs" on every clean session.
    {
        let snapshot = config.repair_stats.snapshot();
        if !snapshot.is_empty() {
            let _ = emit.send(LoopEvent::RepairStats { snapshot }).await;
        }
    }

    // Pi line 268: final agent_end.
    finish_tally(&mut tally, &config, &capability);
    let _ = emit
        .send(LoopEvent::AgentEnd {
            messages: new_messages.clone(),
        })
        .await;
    new_messages
}

/// Local extract — same as `tools::extract_tool_calls`. Kept
/// inline so `run.rs` doesn't reach into `tools` for tiny helpers.
fn extract_tool_calls_from(msg: &AssistantMessage) -> Vec<super::tools::ToolCall> {
    super::tools::extract_tool_calls(msg)
}

/// Pure decision for the untracked-work advisory (dirge-track): fire when a
/// real session made file edits this turn but is tracking no active todo, and
/// the one-shot budget isn't spent. Split out from `poll_finalization_follow_up`
/// so the gate is unit-testable without the process-global TODO_LIST mirror
/// that `unfinished_count()` reads. When `unfinished > 0` the ordinary todo
/// nudge already covers it, so this only handles the empty-list gap.
fn should_advise_untracked_work(
    session_id: Option<&str>,
    track_nudges: u8,
    unfinished: usize,
    made_file_edits: bool,
) -> bool {
    session_id.is_some() && track_nudges < MAX_TRACK_NUDGES && unfinished == 0 && made_file_edits
}

/// Model-visible reminder injected into the conversation when the model is
/// editing files without an active todo. Imperative tone matching the
/// unfinished-todo nudge — tells the model to create a todo before continuing.
/// How deep a conversation must be before a zero cache-read is suspicious
/// rather than an ordinary cold start (dirge-ugah.3). A first turn writes an
/// entry and reads nothing by definition; by this many messages the session
/// has had at least one prior turn to write a readable prefix.
const CACHE_MISS_MIN_MESSAGES: usize = 4;

/// Whether a turn's usage is the signature of a *silent* prefix-cache miss
/// (dirge-ugah.3): caching is demonstrably active — an entry was written —
/// yet nothing was read back.
///
/// Two documented ways to lose an Anthropic cache read with no error to show
/// for it:
///
/// - **The 20-block lookback window.** A breakpoint walks back at most 20
///   content blocks to find a prior entry. A turn appending more than that —
///   ten parallel tool calls is `assistant(text + 10 tool_use)` plus
///   `user(10 tool_result)` = 21 blocks — pushes the previous breakpoint out
///   of range. dirge's system prompt actively encourages parallel tool calls,
///   and a single automatic breakpoint has no redundancy here.
/// - **Concurrent fan-out.** An entry only becomes readable once the first
///   response *begins streaming*, so K subagents launched together each pay
///   the write instead of one write and K−1 reads.
///
/// Neither is confirmed to fire in practice, which is exactly why this
/// reports rather than "fixes": the log is the evidence for deciding whether
/// either needs a code change.
fn is_silent_cache_miss(usage: Option<TokenUsage>, message_count: usize) -> bool {
    usage.is_some_and(|u| {
        u.cached_input_tokens == 0
            && u.cache_creation_input_tokens > 0
            && message_count > CACHE_MISS_MIN_MESSAGES
    })
}

/// The mid-session memory-refresh message (dirge-ugah.2).
///
/// A **user**-role message wrapping a `<system-reminder>` block, not a
/// `system`-role one. The role is load-bearing for cost: on the OAuth path
/// `hoist_system_messages` relocates every system-role entry out of
/// `messages[]` into the top-level `system` array, and since Anthropic
/// renders `tools → system → messages` and caches on a strict prefix match,
/// growing `system` shifts every message byte after it — re-billing the
/// entire conversation at cache-write price. Appending to the tail of
/// `messages[]` is the cheapest possible placement; hoisting moved it to the
/// most expensive one.
///
/// `<system-reminder>` is the same operator-framing convention
/// `agent::tools::background` uses, and `ui::text_output` already strips a
/// leading one from anything user-visible.
pub(crate) fn memory_refresh_message(block: &str) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": format!(
            "<system-reminder>\n## Updated memory (consolidated mid-session)\n\
             {block}\n</system-reminder>"
        ),
    })
}

fn track_work_reminder_message() -> LoopMessage {
    LoopMessage::User(super::message::UserMessage::text(format!(
        "{TRACK_WORK_TAG} You're editing files without an active todo. Before continuing, \
         add this task to your active work list with write_todo_list and mark the item \
         you're working on in_progress, so your progress is tracked."
    )))
}

/// Whether to remind the model to run a FAST check mid-run (dirge-uw2l.2).
/// True only when tiers are engaged, the one-shot budget is unspent, and
/// enough code edits have piled up since the last verification of any tier.
fn should_nudge_fast_verify(
    mode: GateMode,
    verify_nudges: u8,
    edits_since_verify: u32,
    tier: super::capability::CapabilityTier,
) -> bool {
    mode != GateMode::Off
        && verify_nudges < MAX_VERIFY_NUDGES
        && edits_since_verify >= tier.scale_threshold(FAST_VERIFY_EDIT_THRESHOLD)
}

/// Pure decision + message builder for the mid-run fast-check reminder
/// (dirge-uw2l.2). The RAX finding this implements: most bugs were caught by
/// developers front-line testing DURING integration, not by the expensive
/// end-stage campaign — so ask for the cheap tier now and keep the slow tier
/// for the boundary. Split out from the inner loop so it's unit-testable
/// without the run loop.
fn build_fast_verify_reminder(
    mode: GateMode,
    verify_nudges: u8,
    tier: super::capability::CapabilityTier,
    edits_since_verify: u32,
) -> Option<LoopMessage> {
    if !should_nudge_fast_verify(mode, verify_nudges, edits_since_verify, tier) {
        return None;
    }
    Some(LoopMessage::User(super::message::UserMessage::text(
        format!(
            "{VERIFY_TAG} You've made {edits_since_verify} code edits without running any check. \
             Run a FAST one now — a typecheck, a linter, or just the test covering what you're \
             touching — so a mistake surfaces here instead of several edits from now. Save the \
             full suite for when the work is done."
        ),
    )))
}

/// Pure decision + message builder for the early track-work nudge (dirge-track
/// v2). Returns `Some(message)` when all conditions hold — session, budget
/// unspent, no active todos, file edits this turn — and `None` otherwise.
/// Split out from the inner loop so it's unit-testable without the run loop.
fn build_early_track_work_reminder(
    session_id: Option<&str>,
    track_nudges: u8,
    unfinished: usize,
    made_file_edits: bool,
) -> Option<LoopMessage> {
    if should_advise_untracked_work(session_id, track_nudges, unfinished, made_file_edits) {
        Some(track_work_reminder_message())
    } else {
        None
    }
}

/// Did any assistant turn this finalization cycle contain a file-edit tool
/// call (write, edit, apply_patch, etc.)? Read-only / execute-only turns
/// (read, grep, bash, etc.) return false.
fn turn_made_file_edits(new_messages: &[LoopMessage]) -> bool {
    for msg in new_messages {
        if let LoopMessage::Assistant(a) = msg {
            for tc in extract_tool_calls_from(a) {
                if crate::permission::engine::tool_operation(&tc.name)
                    == crate::permission::engine::types::Operation::Edit
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Did any assistant turn this finalization cycle call `write_todo_list`?
/// Distinguishes "planned this turn" from the cross-turn
/// `todo::unfinished_count()` global, so the plan-only nudge fires on a turn
/// that produced a list and nothing else, and never on a turn that merely
/// inherited someone else's open items.
fn turn_wrote_todos(new_messages: &[LoopMessage]) -> bool {
    new_messages.iter().any(|msg| {
        matches!(msg, LoopMessage::Assistant(a)
            if extract_tool_calls_from(a)
                .iter()
                .any(|tc| tc.name == "write_todo_list"))
    })
}

/// Did this run actually use tools? Gates the F6 critic so pure Q&A turns
/// (no tool calls) never trigger an LLM critique.
fn run_made_tool_calls(new_messages: &[LoopMessage]) -> bool {
    new_messages
        .iter()
        .any(|m| matches!(m, LoopMessage::ToolResult(_)))
}

/// True when the run's final assistant turn ended by asking the user a
/// question. [dirge-g2ex]
///
/// Every prompt instructs the model to clarify in prose ("ask one question at a
/// time, prefer multiple-choice"), and the `question` tool blocks and resolves
/// in-turn so it never reaches finalization — so a trailing prose question is
/// the only reliable signal that a turn ended waiting on the user. The
/// finalization gates (`poll_finalization_follow_up`) key off this so they
/// finalize and hand control back instead of re-entering until the model gives
/// up waiting and guesses.
///
/// Rules, in order:
/// 1. The last message must be `LoopMessage::Assistant`.
/// 2. It must contain NO `ContentBlock::ToolCall` (still working).
/// 3. Join its `Text` bodies with `\n`; `Thinking` blocks are ignored.
/// 4. An ODD number of ``` fences means an unterminated code block — don't
///    parse prose out of code.
/// 5. Drop trailing blank or option/list-item lines, so a question followed by
///    a multiple-choice block still counts.
/// 6. Nothing remaining → false.
/// 7. Trim the last remaining line, then strip trailing decoration chars and
///    whitespace.
/// 8. True iff it now ends with `?` or `？`.
fn awaiting_user_response(new_messages: &[LoopMessage]) -> bool {
    // 1. The last message must be an assistant turn.
    let Some(LoopMessage::Assistant(last)) = new_messages.last() else {
        return false;
    };
    // 2. A trailing tool call means it's still working, not waiting.
    if !extract_tool_calls_from(last).is_empty() {
        return false;
    }
    // 3. Join the Text bodies with '\n'; ignore Thinking blocks.
    let mut joined = String::new();
    for block in &last.content {
        if let ContentBlock::Text { text } = block {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(text);
        }
    }
    // 4. An odd number of ``` fences is an unterminated code block — don't
    //    parse prose (and a trailing '?') out of code.
    if joined.matches("```").count() % 2 == 1 {
        return false;
    }
    // 5-6. Walk back over trailing blank / option-list lines and keep the last
    //      meaningful line, so a question followed by a multiple-choice block
    //      still counts.
    let mut candidate: Option<&str> = None;
    for line in joined.lines().rev() {
        if line.trim().is_empty() || is_option_list_line(line) {
            continue;
        }
        candidate = Some(line);
        break;
    }
    let Some(candidate) = candidate else {
        return false;
    };
    // 7. Trim, then strip trailing decoration chars and whitespace so e.g.
    //    `**Which approach?**` resolves to a trailing '?'.
    let mut s: String = candidate.trim().to_string();
    while let Some(ch) = s.chars().last() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '*' | '_' | '`' | ')' | ']' | '"' | '\'' | '\u{201d}' | '\u{2019}' | '>'
            )
        {
            s.pop();
        } else {
            break;
        }
    }
    // 8. True iff it now ends with '?' or fullwidth '？'.
    s.ends_with('?') || s.ends_with('\u{ff1f}')
}

/// True for a trailing option/list-item line: leading whitespace, then a marker
/// (one of `-`, `*`, `+`, `•`, `N.`, `N)`, `(N)`, `a.`, `a)`), then whitespace
/// or end of line. Plain string matching only — no regex dependency.
/// [dirge-g2ex]
fn is_option_list_line(line: &str) -> bool {
    let s = line.trim_start();
    let Some(marker_len) = option_marker_len(s) else {
        return false;
    };
    let after = &s[marker_len..];
    after.is_empty() || after.starts_with(char::is_whitespace)
}

/// Byte length of an option-list marker at the start of `s`, if any.
fn option_marker_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    // Single-char markers: -, *, +, •
    if let Some(ch) = s.chars().next() {
        match ch {
            '-' | '*' | '+' | '\u{2022}' => return Some(ch.len_utf8()),
            _ => {}
        }
    }
    // <digits>.  or  <digits>)
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') {
        return Some(i + 1);
    }
    // ( <digits> )
    if bytes.first() == Some(&b'(') {
        let mut j = 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > 1 && j < bytes.len() && bytes[j] == b')' {
            return Some(j + 1);
        }
    }
    // <single letter>.  or  <single letter>)
    let mut chars = s.chars();
    if let Some(first) = chars.next()
        && first.is_ascii_alphabetic()
        && let Some(second) = chars.next()
        && (second == '.' || second == ')')
    {
        return Some(first.len_utf8() + second.len_utf8());
    }
    None
}

/// dirge-1g3v / dirge-8gdv: the diff-aware reviewer engages only on what THIS
/// run changed. Given the working-tree diff now (`current`) and the run-start
/// baseline (`baseline`), return the diff to review, or `None` to skip. An
/// identical diff means the turn touched nothing on disk (e.g. a read-only
/// 'explain this' turn over pre-existing WIP), so the judge is skipped — as
/// the gate's own comment intends ("no code changed on disk"). Without this,
/// any `ToolResult` (read-only included) drove the judge over the entire dirty
/// tree, spending judge calls and blocking the loop up to `MAX_REVIEW_REACT`.
///
/// dirge-8gdv: the changed-or-not decision keys on the UNcapped
/// [`RunDiff::fingerprint`], NOT the size-capped text. When pre-existing WIP
/// already exceeds [`MAX_DIFF_BYTES`], a length-preserving edit landing PAST
/// the cap leaves the two capped strings byte-identical, which made the old
/// capped-string comparison wrongly skip the reviewer. The bounded capped text
/// still goes to the reviewer unchanged — only this equality check changed.
fn run_delta_to_review<'a>(
    current: Option<&'a super::code_review::RunDiff>,
    baseline: Option<&super::code_review::RunDiff>,
) -> Option<&'a str> {
    let current = current?;
    let changed = match baseline {
        Some(b) => current.fingerprint != b.fingerprint,
        None => true,
    };
    if changed { Some(&current.capped) } else { None }
}

/// Build a compact transcript of one run for the F6 critic: the user
/// request, the assistant's text, the tool calls it made, and a short
/// slice of each tool result. Capped so a giant run can't blow up the
/// critic prompt.
///
/// dirge-p9qm: when over budget, keep the HEAD (the original request and
/// early framing) AND the TAIL (the most recent activity), eliding the
/// middle — NOT the first N chars. The critic judges "is the task complete
/// and correct", which is decided by the latest work and verification; a
/// blind head cut fed it the planning phase and dropped the implementation,
/// so it wrongly reported nothing was done.
fn build_critic_transcript(new_messages: &[LoopMessage]) -> String {
    const MAX_CHARS: usize = 12_000;
    // Reserve for the run's opening (the user request + first framing) so the
    // critic still knows what was asked; the rest of the budget goes to the
    // tail, where completion is decided.
    const HEAD_CHARS: usize = 2_000;
    const PER_RESULT_CHARS: usize = 400;
    const ELISION: &str =
        "\n…(earlier run steps elided; showing the start and the most recent activity)…\n";

    let mut blocks: Vec<String> = Vec::new();
    for m in new_messages {
        match m {
            LoopMessage::User(u) => {
                blocks.push(format!("USER: {}\n", u.text_joined().trim()));
            }
            LoopMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            blocks.push(format!("ASSISTANT: {}\n", text.trim()));
                        }
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            let args = serde_json::to_string(arguments).unwrap_or_default();
                            let args: String = args.chars().take(200).collect();
                            blocks.push(format!("ASSISTANT called {name}({args})\n"));
                        }
                        _ => {}
                    }
                }
            }
            LoopMessage::ToolResult(t) => {
                let text: String = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                // dirge-kk3x: mark permission/approval denials distinctly so
                // the critic reads them as a policy wall (out of scope), not a
                // failure to demand the assistant fix or route around. Gate on
                // `is_error` exactly like Outcome::classify does — a genuine
                // enforce-layer denial is always an error result, whereas a
                // SUCCESSFUL result whose text merely begins "Permission denied"
                // (e.g. bash returns Ok(text) for a failed `ssh` whose output is
                // "Permission denied (publickey).") must NOT be excused as
                // out-of-scope, or the critic would pass genuinely unfinished work.
                let denied = t.is_error && crate::agent::tools::is_permission_denial(&text);
                let text: String = text.chars().take(PER_RESULT_CHARS).collect();
                let tag = if denied {
                    "DENIED"
                } else if t.is_error {
                    "ERROR"
                } else {
                    "result"
                };
                blocks.push(format!("TOOL {} [{}]: {}\n", t.tool_name, tag, text.trim()));
            }
            _ => {}
        }
    }

    let total: usize = blocks.iter().map(|b| b.chars().count()).sum();
    if total <= MAX_CHARS {
        return blocks.concat();
    }

    // Over budget. Take leading blocks up to HEAD_CHARS (always at least the
    // first block, the request)…
    let mut head_end = 0;
    let mut head_len = 0;
    while head_end < blocks.len() {
        let n = blocks[head_end].chars().count();
        if head_len + n > HEAD_CHARS && head_end > 0 {
            break;
        }
        head_len += n;
        head_end += 1;
        if head_len >= HEAD_CHARS {
            break;
        }
    }

    // …then fill the remaining budget from the END backward, without
    // re-crossing into the head region.
    let tail_budget = MAX_CHARS.saturating_sub(head_len + ELISION.chars().count());
    let mut tail_start = blocks.len();
    let mut tail_len = 0;
    while tail_start > head_end {
        let n = blocks[tail_start - 1].chars().count();
        if tail_len + n > tail_budget && tail_start < blocks.len() {
            break;
        }
        tail_len += n;
        tail_start -= 1;
        if tail_len >= tail_budget {
            break;
        }
    }

    let mut out = String::new();
    out.push_str(&blocks[..head_end].concat());
    out.push_str(ELISION);
    out.push_str(&blocks[tail_start..].concat());
    // Final safety clamp — keep the TAIL (recent activity), never the head,
    // if a pathological single block still overran.
    let len = out.chars().count();
    if len > MAX_CHARS {
        return out.chars().skip(len - MAX_CHARS).collect();
    }
    out
}

/// dirge-ngic: build the merged source the scavenger inspects from
/// the assistant message's content blocks. Reasonix combines both
/// reasoning and visible content (`loop.ts:910-913` →
/// `repair/index.ts:71`); dirge previously merged only Thinking,
/// losing any DSML invoke that arrived as plain Text (Anthropic
/// often streams DSML in Text rather than Thinking on cache hit).
/// Returns the concatenated text with `\n` between blocks.
pub(crate) fn build_scavenge_source(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking { text } => Some(text.as_str()),
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// dirge-7bwx: walk the tool-call list and apply the truncation
/// closer to any call whose arguments arrived as a `Value::String`
/// that fails to parse as JSON. Successful repairs replace the
/// arguments in-place and record `RepairKind::TruncationFixed` in
/// stats; hard fallback leaves the original string untouched so
/// validation downstream surfaces the failure (Reasonix
/// invariant at `repair/index.ts:93-102`).
///
/// Called BEFORE `storm.filter_calls` so two streams whose raw
/// args differ but repair identically dedupe under storm.
pub(crate) fn apply_truncation_repair(
    tool_calls: &mut [crate::agent::agent_loop::ToolCall],
    repair_stats: &crate::agent::agent_loop::tool_input_repair::RepairStats,
    truncation_notes: &std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    >,
) {
    use crate::agent::agent_loop::tool_input_repair::{RepairKind, repair_truncated_json};
    for tc in tool_calls.iter_mut() {
        if let serde_json::Value::String(raw) = &tc.arguments {
            // Already-valid JSON-as-string: promote to its parsed
            // form so the storm filter's canonical signature matches
            // any peer that arrived as a real Object/Array. No
            // repair stat — nothing was healed. (Dirge-only
            // compensation; Reasonix args are always strings so it
            // has no equivalent.)
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
                tc.arguments = parsed;
                continue;
            }
            // Truncated / malformed: run the brace-closer.
            let r = repair_truncated_json(raw);
            if !r.changed {
                continue;
            }
            // dirge-7bwx review-fix #1: Reasonix bumps
            // `truncationsFixed` on BOTH success
            // (`repair/index.ts:105`) AND hard-fallback (`:99`).
            // Operators care most about the unrecoverable rate —
            // dropping it from telemetry would hide the cases that
            // most need attention.
            repair_stats.record(RepairKind::TruncationFixed);
            // dirge-7bwx review-fix #2: forward the closer's notes
            // (Reasonix `repair/index.ts:100-101, :106`). Stored
            // per call-id; `prepare_tool_call` plucks them and
            // prepends to the tool result so the model sees what
            // was repaired.
            let prefix = if r.fallback {
                format!("[{}] ⚠️ TRUNCATION UNRECOVERABLE", tc.name)
            } else {
                format!("[{}]", tc.name)
            };
            let mut sink = truncation_notes.lock_ignore_poison();
            let entry = sink.entry(tc.id.clone()).or_default();
            for n in &r.notes {
                entry.push(format!("{prefix} {n}"));
            }
            drop(sink);
            // On success only, replace args with the parsed form.
            // Hard-fallback leaves the raw string so
            // validate_and_repair surfaces a real validation
            // error (Reasonix invariant `repair/index.ts:93-102`).
            if !r.fallback
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.repaired)
            {
                tc.arguments = parsed;
            }
        }
    }
}

// =====================================================================
// Tests — ported from pi/test/agent-loop.test.ts
// Inlined tests were extracted to the sibling `run_tests.rs` file;
// `#[path = "..."]` pulls it in as the `tests` child module so the
// `use super::*` references inside continue to resolve.
// =====================================================================

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
