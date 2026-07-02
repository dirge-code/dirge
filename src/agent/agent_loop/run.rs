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
use super::inflight::InflightSet;
use super::message::{
    AssistantMessage, ContentBlock, LoopEvent, LoopMessage, StopReason, ToolResultMessage,
    loop_message_to_value, tool_result_to_value,
};
use super::storm::StormBreaker;
use super::stream::{StreamFn, stream_assistant_response};
use super::tool::AbortSignal;
use super::types::{Context, LoopConfig};

/// Phase 4 part 2: poll the configured `get_steering_messages`
/// hook AND the file-touch tracker (when present), concatenating
/// their outputs. The tracker reminder follows any queued steering
/// messages so the user's explicit guidance is observed first.
///
/// Kept as a free fn so the inner/outer steering-poll sites stay
/// terse. Returns an empty Vec when neither source has anything to
/// inject — preserves the legacy fast path byte-for-byte.
/// Returns the polled messages plus whether any came from genuine USER
/// steering (the interjection queue) — the file-touch reminder doesn't
/// count. The bool drives the turn-budget reset (dirge-st8r): active human
/// steering gets a fresh budget; ambient reminders do not.
async fn poll_steering_and_reminder(
    config: &LoopConfig,
    guards: &super::activity::LoopGuards,
) -> (Vec<LoopMessage>, bool) {
    let mut out = match &config.get_steering_messages {
        Some(get) => get().await,
        None => Vec::new(),
    };
    let had_user_steering = !out.is_empty();
    if let Some(tracker) = &config.file_touch_tracker {
        out.extend(tracker.poll_reminder());
    }
    // Cross-turn recovery checkpoint: fired when consecutive *distinct*
    // tool errors pile up (storm only catches identical repeats). Follows
    // any user steering so the human's guidance is read first.
    out.extend(guards.poll_reflection());
    (out, had_user_steering)
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
    /// Caller-supplied `get_followup_messages` hook (e.g. the `/plan`
    /// reviewer loop). Highest priority.
    Hook,
    /// Verifier gate: code was edited but nothing was run to check it.
    Verifier,
    /// Bounded LLM critic judgment (at most once per run).
    Critic,
    /// Diff-aware code reviewer: findings on the run's diff. Re-enters the
    /// loop, bounded by [`super::code_review::MAX_REVIEW_REACT`].
    CodeReview,
    /// Goal gate: user-defined stop condition not yet met. Re-enters the
    /// loop, bounded by [`super::goal::MAX_GOAL_REACT`].
    Goal,
    /// Unfinished-todo nudge (bounded by [`MAX_TODO_NUDGES`]).
    Todo,
    /// No gate fired — the run may finalize.
    None,
}

/// Display tag prefixing the unfinished-todo nudge. The UI keys on this to
/// attribute the message to the system/critic rather than the user — it's
/// injected as a user-role message so the model responds, but it isn't user
/// input [dirge-i75f].
pub(crate) const TODO_NUDGE_TAG: &str = "[todo]";

/// Stable prefix of the max-agent-turns truncation notice. The
/// headless result path (`provider::run`) matches on this to mark the
/// run truncated in its JSON envelope (dirge-18v2) — sharing the
/// constant keeps emitter and detector from drifting.
pub(crate) const MAX_TURNS_NOTICE_PREFIX: &str = "[dirge] Max agent turns";

/// The unfinished-todo nudge message. Pure (no globals) so the singular/plural
/// wording is unit-testable independent of the todo store.
fn todo_nudge_message(unfinished: usize) -> LoopMessage {
    LoopMessage::User(super::message::UserMessage {
        content: format!(
            "{TODO_NUDGE_TAG} You still have {unfinished} unfinished todo{} (pending or in progress). \
             Finish the remaining work, or if it's genuinely done or no longer needed, \
             update the todo list (mark items completed/cancelled) before stopping.",
            if unfinished == 1 { "" } else { "s" }
        ),
    })
}

/// Poll the finalization gates in strict priority order and return the first
/// non-empty source's messages (plus which source fired, for tracing/tests).
///
/// At most ONE source contributes per finalization. The lower-priority gates
/// (verifier, critic, todo) are each one-shot or bounded, so deferring one by
/// a turn is intentional: e.g. a red build surfaces the verifier nudge now and
/// the critic runs at the *next* finalization once the build is fixed (the
/// verifier won't fire twice). This is the single authority for finalization
/// precedence — previously four separate `if follow_up.is_empty()` blocks
/// inline in the outer loop [dirge-vcsn].
#[allow(clippy::too_many_arguments)]
async fn poll_finalization_follow_up(
    config: &LoopConfig,
    system_prompt: &str,
    new_messages: &[LoopMessage],
    critic_done: &mut bool,
    code_review_reacts: &mut u8,
    code_review_baseline: Option<&str>,
    emitted_advisories: &mut std::collections::HashSet<String>,
    goal_reacts: &mut u8,
    todo_nudges: &mut u8,
    emit: &mpsc::Sender<LoopEvent>,
) -> (Vec<LoopMessage>, FollowUpSource) {
    // 1. Caller hook (pi lines 256-262) — highest priority.
    if let Some(get) = &config.get_followup_messages {
        let msgs = get().await;
        if !msgs.is_empty() {
            return (msgs, FollowUpSource::Hook);
        }
    }
    // 2. F6 verifier gate — one-time "verify before done" when code was edited
    //    but nothing was run to check it.
    if let Some(verifier) = &config.verifier {
        let msgs = verifier.check_before_finalize();
        if !msgs.is_empty() {
            return (msgs, FollowUpSource::Verifier);
        }
    }
    // 3. F6 tier 3 — bounded LLM critic, once per run, only if the run did real
    //    work. `critic_done` flips unconditionally so it fires at most once
    //    regardless of verdict.
    if !*critic_done && config.critic_fn.is_some() && run_made_tool_calls(new_messages) {
        *critic_done = true;
        if let Some(critic) = &config.critic_fn {
            let transcript = build_critic_transcript(new_messages);
            // dirge-6q3w: hand the critic the run's compile/lint/test signal
            // (read-only, doesn't spend the cheap verifier's one-shot nudge)
            // so it can be pickier about unverified code changes. `None` when
            // no verifier gate is configured → critic behaves as before.
            let verification = config.verifier.as_ref().map(|v| v.status());
            // dirge-bedj: judge within the agent's own system prompt so the
            // critic never demands a forbidden action.
            let msgs =
                super::critic::run_critic(critic, system_prompt, &transcript, verification).await;
            if !msgs.is_empty() {
                return (msgs, FollowUpSource::Critic);
            }
        }
    }
    // 3.25 dirge-iyf5 — diff-aware code reviewer. Reviews the run's
    //      uncommitted diff and surfaces severity-ranked findings. Unlike
    //      the one-shot critic it PERSISTS across finalizations (bounded by
    //      MAX_REVIEW_REACT) so the agent can fix findings and be
    //      re-reviewed. The precondition is a change on disk SINCE RUN START:
    //      the current working-tree diff is compared against the run-start
    //      baseline (dirge-1g3v), so a read-only turn — even one over
    //      pre-existing uncommitted WIP — has an unchanged diff and skips the
    //      reviewer entirely, never paying for a judge call. Active only when
    //      a `critic_provider` is configured (the reviewer reuses its judge) —
    //      off with no cost for default sessions.
    if *code_review_reacts < super::code_review::MAX_REVIEW_REACT
        && config.code_review_fn.is_some()
        && run_made_tool_calls(new_messages)
        && let Some(review_fn) = &config.code_review_fn
    {
        let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        // Diff capture shells out to git — do it off the async runtime.
        let diff = tokio::task::spawn_blocking(move || super::code_review::capture_run_diff(&repo))
            .await
            .ok()
            .flatten();
        // Review only what THIS run changed: skip when the diff is identical to
        // the run-start baseline (nothing touched on disk this turn).
        if let Some(diff) = run_delta_to_review(diff.as_deref(), code_review_baseline) {
            let transcript = build_critic_transcript(new_messages);
            let findings =
                super::code_review::run_code_review(review_fn, system_prompt, diff, &transcript)
                    .await;
            if !findings.is_empty() {
                let reaction = decide_review_reaction(findings, emitted_advisories);
                // Medium/Low → surface a non-blocking `<system>` notice, but
                // only the first time this exact advisory comes up (dirge-jo0o).
                if let Some(text) = reaction.advisory_to_emit {
                    let _ = emit.send(LoopEvent::SystemNotice { content: text }).await;
                }
                // Only a blocking engagement spends the react budget — an
                // advisory-only review finalizes without burning it (dirge-jo0o).
                if reaction.counts_against_budget {
                    *code_review_reacts += 1;
                }
                // High/Critical → re-enter the loop; the agent must fix or
                // justify. Advisory-only reviews fall through and finalize.
                if let Some(msg) = reaction.blocking_followup {
                    return (vec![msg], FollowUpSource::CodeReview);
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
    if *goal_reacts < super::goal::MAX_GOAL_REACT
        && let Some(goal) = &config.goal
        && let Some(judge) = &config.goal_fn
    {
        let transcript = build_critic_transcript(new_messages);
        // dirge-6q3w: same read-only verification signal as the critic, but
        // the goal judge treats it as a SOFT advisory (see
        // `goal_verification_note`) so a non-testable task can't trap the
        // bounded goal loop.
        let verification = config.verifier.as_ref().map(|v| v.status());
        let msgs =
            super::goal::run_goal_gate(judge, goal, system_prompt, &transcript, verification).await;
        if !msgs.is_empty() {
            *goal_reacts += 1;
            return (msgs, FollowUpSource::Goal);
        }
    }
    // 4. vix-port — final gate: nudge the model to finish or clear unfinished
    //    todos before stopping. Bounded by MAX_TODO_NUDGES.
    if *todo_nudges < MAX_TODO_NUDGES {
        let unfinished = crate::agent::tools::todo::unfinished_count();
        if unfinished > 0 {
            *todo_nudges += 1;
            return (vec![todo_nudge_message(unfinished)], FollowUpSource::Todo);
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
        let content = m
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
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

/// Max live issues surfaced in the turn-start board reminder. The rest are
/// summarized as a "+N more" hint so a large backlog can't flood context.
const ISSUE_BOARD_TOP_N: usize = 7;

/// dirge-x6yi: open the issue DB and build the turn-start board reminder.
/// This is synchronous rusqlite I/O (open + query), so `run_agent_loop`
/// hands it to `spawn_blocking` — a contended/locked `state.db` must not
/// stall the whole loop task (mirrors the pre-recall search path). Returns
/// `None` on any failure (missing/locked db, empty board); the reminder is
/// best-effort context, never fatal.
fn issue_board_reminder_block(db_path: &std::path::Path) -> Option<String> {
    crate::extras::issue_db::IssueStore::open_at(db_path)
        .ok()?
        .board_reminder(ISSUE_BOARD_TOP_N)
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
    emit: mpsc::Sender<LoopEvent>,
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
        let prompt = compression::build_summary_prompt(&messages, budget, None, None);
        let result = tokio::time::timeout(CHECKPOINT_SUMMARY_TIMEOUT, sfn(prompt)).await;
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
            let _ = emit.send(LoopEvent::CheckpointRefresh { summary }).await;
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
            && let Some((new_msgs, first_kept)) = compression::apply_checkpoint_summary(
                &current_context.messages,
                &cp.summary,
                cp.boundary,
            )
        {
            let projected = compression::estimate_messages_tokens(&new_msgs);
            if projected <= fold_target {
                current_context.messages = new_msgs;
                after_summary = projected;
                applied_summary = cp.summary;
                applied_first_kept = first_kept;
                outcome = SummaryOutcome::Succeeded(first_kept);
                reused = true;
                tracing::info!(
                    target: "dirge::agent_loop",
                    boundary = cp.boundary,
                    tokens_after = projected,
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
                None => {
                    let prompt = compression::build_summary_prompt(
                        &middle,
                        budget,
                        prev.as_deref(),
                        augmented_focus.as_deref(),
                    );
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
            };
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
    // Pi line 103: `newMessages = [...prompts]`.
    let new_messages = prompts.clone();

    // The verbatim user message for this turn — drives both few-shot exemplar
    // retrieval and verbatim pre-recall.
    let task_query: String = prompts
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.content.as_str()),
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
        let ex_msg = LoopMessage::User(super::message::UserMessage { content: block });
        context.messages.push(loop_message_to_value(&ex_msg));
    }

    // Pi line 105: `currentContext.messages = [...context.messages, ...prompts]`.
    for prompt in &prompts {
        context.messages.push(loop_message_to_value(prompt));
        // Phase 4 part 2: notify the file-touch tracker about user
        // prompts so it can decide whether the streak persists or
        // resets to a new topic.
        if let (Some(tracker), LoopMessage::User(u)) = (&config.file_touch_tracker, prompt) {
            tracker.record_user_message(&u.content);
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
                    let msg = LoopMessage::User(super::message::UserMessage { content: block });
                    context.messages.push(loop_message_to_value(&msg));
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
        // dirge-x6yi: run the blocking open+query off the loop task.
        if let Ok(Some(block)) =
            tokio::task::spawn_blocking(move || issue_board_reminder_block(&db_path)).await
        {
            let msg = LoopMessage::User(super::message::UserMessage { content: block });
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

    // Pi line 167: initial steering poll.
    // Phase 4 part 2: composes with the file-touch tracker's
    // reminder poll when configured.
    let (mut pending_messages, _initial_user_steering): (Vec<LoopMessage>, bool) =
        poll_steering_and_reminder(&config, &guards).await;

    // dirge-nqr: count assistant turns so a hard cap can stop a
    // runaway run. `max_turns = None` means unlimited (legacy).
    let mut turns_taken: usize = 0;

    // F4: in-session reflexion memory. Accumulates the approaches the
    // model looped on and abandoned this run, so the repeat-loop guard
    // can remind it of every dead end (not just the latest repeat).
    // Lives outside the outer loop so it persists across turns.
    let mut reflections = super::reflexion::ReflectionLog::new();

    // F6 tier 3: the bounded LLM critic fires at most once per run.
    let mut critic_done = false;

    // dirge-iyf5: the diff-aware code reviewer persists across
    // finalizations (fix-then-re-review) bounded by MAX_REVIEW_REACT.
    let mut code_review_reacts: u8 = 0;

    // dirge-jo0o: advisory (medium/low) notices already emitted this run, so a
    // persistent low-severity finding is advised once rather than re-posted as
    // a duplicate SystemNotice at every finalization.
    let mut emitted_advisories: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // dirge-1g3v: snapshot the working-tree diff at run start so the reviewer
    // can tell what THIS run changed. Without a baseline it diffed the whole
    // dirty tree, so a read-only turn over pre-existing WIP triggered the judge
    // and could block the loop. Only needed when the reviewer is armed.
    let code_review_baseline: Option<String> = if config.code_review_fn.is_some() {
        let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        tokio::task::spawn_blocking(move || super::code_review::capture_run_diff(&repo))
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    // Goal gate: counts re-entries so a user-defined stop condition that
    // never resolves can't loop past MAX_GOAL_REACT.
    let mut goal_reacts: u8 = 0;

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

    // vix-port: don't let the model end a turn while it still has unfinished
    // todos (bounded by MAX_TODO_NUDGES; vix caps at 3, session.go:1551).
    let mut todo_nudges: u8 = 0;

    'outer: loop {
        // Storm: fresh intent on each new user turn.
        // Port of Reasonix loop.ts:621 `this.repair.resetStorm()`.
        guards.reset_turn();
        let mut turn_self_corrected = false;

        // Multi-tier: fresh turn intent — clear fold flag.
        // Port of Reasonix loop.ts:623 `this._foldedThisTurn = false`.
        folded_this_turn = false;

        let mut has_more_tool_calls = true;

        // Pi line 174: INNER LOOP.
        while has_more_tool_calls || !pending_messages.is_empty() {
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
            // capped to the working budget: effective context degrades well
            // before a large window is full, so dirge folds/forms memory to
            // stay inside the budget (default 100k) rather than trusting the
            // full window. Every downstream tier (fold / snip / turn-start /
            // incremental checkpoint) reads this capped value.
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
                    current_context.messages.push(serde_json::json!({
                        "role": "system",
                        "content": format!(
                            "## Updated memory (consolidated mid-session)\n{block}"
                        ),
                    }));
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
                        && !u.content.contains("[Context-depth reminder]")
                    {
                        tracker.record_user_message(&u.content);
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
            let (assistant_msg, token_usage) = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                emit,
                stream_fn,
            )
            .await;
            new_messages.push(LoopMessage::Assistant(assistant_msg.clone()));

            // Pi lines 196-200: error / aborted short-circuit.
            if matches!(
                assistant_msg.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                let _ = emit
                    .send(LoopEvent::TurnEnd {
                        message: assistant_msg.clone(),
                        tool_results: Vec::new(),
                    })
                    .await;
                let _ = emit
                    .send(LoopEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                return new_messages;
            }

            // Pi lines 202-216: tool calls + results.
            let mut tool_calls = extract_tool_calls_from(&assistant_msg);

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
                            tool_calls.push(sc.clone());
                        }
                    }
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
                let (surviving_calls, storm_report) = guards.inspect_calls(&tool_calls);
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
                        current_context.messages.push(tool_result_to_value(result));
                        new_messages.push(LoopMessage::ToolResult(result.clone()));
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
            // `prompt_tokens` is None until usage tracking is wired
            // into the stream pipeline (future phase). With None,
            // decision defaults to None (carry on).
            {
                let decision = context_manager::decide_after_usage(
                    token_usage.map(|u| u.input_tokens),
                    ctx_max,
                    folded_this_turn,
                );
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
                            "context-manager: forcing summary and ending turn",
                        );
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
                                    emit.clone(),
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
                    let _ = emit
                        .send(LoopEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await;
                    return new_messages;
                }
            }

            // Pi line 253: refresh steering for next iteration.
            // Phase 4 part 2: also polls the file-touch tracker.
            let (next_pending, had_user_steering) =
                poll_steering_and_reminder(&config, &guards).await;
            pending_messages = next_pending;

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
                let notice = format!(
                    "{MAX_TURNS_NOTICE_PREFIX} ({cap}) reached. Stopping the run. Increase --max-agent-turns or `max_agent_turns` in config.json to allow more."
                );
                // Surface to the user as a `<system>` log line (warning
                // color) rather than a `MessageStart { User }` — the
                // latter rendered with the `<you>` prefix as if the user
                // had typed it.
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
                new_messages.push(LoopMessage::User(super::message::UserMessage {
                    content: notice,
                }));
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
        // hook → verifier → critic → todo, at most one per finalization.
        let (follow_up, source) = poll_finalization_follow_up(
            &config,
            &current_context.system_prompt,
            &new_messages,
            &mut critic_done,
            &mut code_review_reacts,
            code_review_baseline.as_deref(),
            &mut emitted_advisories,
            &mut goal_reacts,
            &mut todo_nudges,
            emit,
        )
        .await;
        if !follow_up.is_empty() {
            tracing::trace!(target: "dirge::loop", ?source, "finalization follow-up interjected");
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

/// Did this run actually use tools? Gates the F6 critic so pure Q&A turns
/// (no tool calls) never trigger an LLM critique.
fn run_made_tool_calls(new_messages: &[LoopMessage]) -> bool {
    new_messages
        .iter()
        .any(|m| matches!(m, LoopMessage::ToolResult(_)))
}

/// dirge-1g3v: the diff-aware reviewer engages only on what THIS run changed.
/// Given the working-tree diff now (`current`) and the run-start baseline
/// (`baseline`), return the diff to review, or `None` to skip. An identical
/// diff means the turn touched nothing on disk (e.g. a read-only 'explain
/// this' turn over pre-existing WIP), so the judge is skipped — as the gate's
/// own comment intends ("no code changed on disk"). Without this, any
/// `ToolResult` (read-only included) drove the judge over the entire dirty
/// tree, spending judge calls and blocking the loop up to `MAX_REVIEW_REACT`.
fn run_delta_to_review<'a>(current: Option<&'a str>, baseline: Option<&str>) -> Option<&'a str> {
    match current {
        Some(diff) if Some(diff) != baseline => Some(diff),
        _ => None,
    }
}

/// One code-review engagement's outcome at finalization, split from the
/// async diff/judge plumbing so the counting + advisory-dedup rules are
/// unit-testable (dirge-jo0o).
struct ReviewReaction {
    /// Advisory (medium/low) notice to emit, or `None` when there is
    /// nothing new to advise — no advisory findings, or an identical notice
    /// already went out this run.
    advisory_to_emit: Option<String>,
    /// Blocking (high/critical) follow-up that re-enters the loop.
    blocking_followup: Option<LoopMessage>,
    /// Whether this engagement spends one unit of the bounded react budget.
    counts_against_budget: bool,
}

/// Decide what a single code-review engagement does with its findings.
///
/// dirge-jo0o: the budget (`MAX_REVIEW_REACT`) exists to stop a *stubbern*
/// blocking finding from looping forever, so only a blocking engagement
/// spends it — an advisory-only review finalizes and must not burn budget.
/// And a persistent medium/low finding must be advised once, not re-posted
/// as a duplicate `SystemNotice` at every finalization: `emitted_advisories`
/// remembers the exact notice text already sent this run.
fn decide_review_reaction(
    findings: Vec<super::code_review::Finding>,
    emitted_advisories: &mut std::collections::HashSet<String>,
) -> ReviewReaction {
    let (blocking, advisory) = super::code_review::partition_findings(findings);
    let advisory_to_emit = super::code_review::advisory_notice(&advisory)
        .filter(|text| emitted_advisories.insert(text.clone()));
    let blocking_followup = super::code_review::blocking_followup(&blocking);
    let counts_against_budget = blocking_followup.is_some();
    ReviewReaction {
        advisory_to_emit,
        blocking_followup,
        counts_against_budget,
    }
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
                blocks.push(format!("USER: {}\n", u.content.trim()));
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
            let mut sink = truncation_notes.lock().expect("truncation_notes poisoned");
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
