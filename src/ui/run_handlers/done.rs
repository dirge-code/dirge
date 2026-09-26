//! `AgentEvent::Done` handler extracted from `run_interactive`.
//!
//! This is the largest handler — it closes a successful turn,
//! finalizes the streamed response, runs the plugin `on-response` /
//! `on-complete` / `prepare-next-run` chain (with optional model
//! swap), decides via `decide_post_done_action` whether to launch a
//! follow-up / loop iteration / stop, hands off to `plan_review` when a
//! phased `/plan` implement turn just finished (the reviewer-runs-code
//! loop), spawns a background review + curator pass when idle, handles
//! git-worktree return, and finally drains any user interjections queued
//! during the run.
//!
//! Behavior is identical to the original inline body; only the
//! lexical home moved.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;
use crossterm::style::Color;
use tokio::sync::mpsc;

#[cfg(feature = "plugin")]
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
use crate::provider::AnyAgent;
use crate::session::MessageRole;
use crate::ui::agent_io::persist_turn_to_db;
use crate::ui::avatar;
use crate::ui::colors::{c_agent, c_error};
use crate::ui::run_handlers::{AgentBuildDeps, RunCtx};
use crate::ui::theme;

/// Optional loop-feature state passed through to `handle_done`.
/// Behind `cfg(feature = "loop")` we hand the real mutable state;
/// without the feature, the placeholder type is `()` so the call
/// site doesn't need to thread a sentinel.
#[cfg(feature = "loop")]
pub(crate) struct LoopBits<'a> {
    pub state: &'a mut Option<crate::extras::r#loop::LoopState>,
    pub label: &'a mut Option<String>,
}

/// Optional vigil-feature state passed through to `handle_done`.
#[cfg(feature = "vigil")]
pub(crate) struct VigilBits<'a> {
    pub state: &'a mut Option<crate::extras::vigil::VigilState>,
}

/// Outcome of [`prepare_next_model_client`]: tells the caller whether to go on
/// and rebuild the agent, and if so whether the provider changed.
#[cfg(feature = "plugin")]
pub(crate) enum NextModelAction {
    /// Don't apply the swap — empty/unchanged model, an unroutable family, or a
    /// client that failed to build. Any warning has already been rendered.
    Skip,
    /// Rebuild the agent for the new model. `swapped_provider` is `Some(alias)`
    /// when the live client was swapped to a different provider (so the caller
    /// updates `session.provider`); `None` for a same-client rename.
    Apply { swapped_provider: Option<String> },
}

/// dirge-m6ut: resolve the plugin `prepare-next-run` model against the
/// configured providers and, when it belongs to a *different* provider, swap
/// `client` IN PLACE so the follow-up turn actually runs there. Returns
/// [`NextModelAction`] telling the caller whether to rebuild the agent.
///
/// This MUST run before the caller builds `AgentBuildDeps` (which borrows
/// `&client`): doing the swap here — the one spot on the done path where
/// `&mut client` is available — is what lets [`apply_next_model`] keep taking
/// the shared, immutable deps bundle. Warns and returns `Skip` when the swap
/// can't happen (family with no configured provider, or a client that fails to
/// build), so an unroutable request is explained rather than mis-sent.
#[cfg(feature = "plugin")]
pub(crate) fn prepare_next_model_client(
    client: &mut crate::provider::AnyClient,
    renderer: &mut crate::ui::renderer::Renderer,
    session: &crate::session::Session,
    cfg: &crate::config::Config,
    next_model: &str,
) -> anyhow::Result<NextModelAction> {
    let trimmed = next_model.trim();
    // Empty string is a misconfiguration; a no-op swap to the current model is
    // nothing to do. Either way, don't rebuild.
    if trimmed.is_empty() || trimmed == session.model.as_str() {
        return Ok(NextModelAction::Skip);
    }
    // Shared routing decision (the one `/model` and `/agent` make), but only
    // the CLIENT half is applied here: the session's model/provider move later
    // in `apply_next_model`, after the hook chain resolves.
    let route = crate::provider::resolve_model_route(cfg, session.provider.as_str(), trimmed);
    match crate::provider::swap_client_for_route(cfg, client, session.provider.as_str(), &route) {
        Ok(swapped_provider) => Ok(NextModelAction::Apply { swapped_provider }),
        Err(refusal) => {
            renderer.write_line(
                &format!(
                    "[plugin] ignoring model swap: {refusal} Staying on '{}'.",
                    session.provider,
                ),
                c_error(),
            )?;
            Ok(NextModelAction::Skip)
        }
    }
}

/// Apply a `prepare-next-run` model swap on-loop (dirge-qhfk stage 3b).
/// Rebuilds the agent for the new model so the next user prompt runs against it,
/// updating the session's model/provider/context-window. Extracted from
/// handle_done so the off-loop `done_phase` arm can run it after the hook chain
/// resolves.
///
/// The routing decision and any client swap happen upstream in
/// [`prepare_next_model_client`]; by the time we're here `deps.client` is
/// already the right client, so this just rebuilds the agent from it.
/// `swapped_provider` carries the target alias when the client was swapped to a
/// different provider (`None` for a same-client rename).
#[cfg(feature = "plugin")]
pub(crate) async fn apply_next_model(
    ctx: &mut RunCtx<'_>,
    agent: &mut AnyAgent,
    context: &mut ContextFiles,
    deps: &AgentBuildDeps<'_>,
    next_model: &str,
    swapped_provider: Option<String>,
) -> anyhow::Result<()> {
    let trimmed = next_model.trim();
    // Defensive: `prepare_next_model_client` already guaranteed a non-empty,
    // changed model, but keep the guard so a direct caller can't rename to junk.
    if trimmed.is_empty() || trimmed == ctx.session.model.as_str() {
        return Ok(());
    }
    let new_model_compact = CompactString::new(trimmed);
    let model_obj = deps.client.completion_model(new_model_compact.to_string());
    *agent = crate::provider::build_agent(
        model_obj,
        ctx.cli,
        ctx.cfg,
        context,
        deps.permission.clone(),
        deps.ask_tx.clone(),
        deps.question_tx.clone(),
        deps.plan_tx.clone(),
        deps.bg_store.clone(),
        #[cfg(feature = "lsp")]
        deps.lsp_manager.cloned(),
        deps.sandbox.clone(),
        #[cfg(feature = "mcp")]
        deps.mcp_manager,
        #[cfg(feature = "semantic")]
        deps.semantic_manager,
        Some(ctx.session.id.to_string()),
        // GH #832: `ctx.session.provider` is only updated further down, after the
        // rebuild, so read the swap's target here — seeding from the provider being
        // LEFT is the very bug this fixes.
        swapped_provider
            .as_deref()
            .or(Some(ctx.session.provider.as_str())),
    )
    .await;
    let old_model = ctx.session.model.clone();
    ctx.session.model = new_model_compact.clone();
    // Only touch `session.provider` when the client actually moved (dirge-cfaw /
    // dirge-m6ut). A same-client rename leaves it unchanged; re-resolving from
    // CLI/config used to clobber a session that had switched providers.
    if let Some(alias) = &swapped_provider {
        ctx.session.provider = CompactString::new(alias);
    }
    // Re-resolve context window for the new model — mirrors `/model` so a
    // 128k→1M jump (or vice versa) updates the status indicator.
    // Against the provider we ended up on — updated just above (GH #772).
    let new_ctx = ctx
        .cfg
        .resolve_context_window_for(&ctx.session.provider, new_model_compact.as_str());
    if new_ctx != ctx.session.context_window {
        ctx.session.context_window = new_ctx;
    }
    let provider_note = swapped_provider
        .as_deref()
        .map(|a| format!("  ·  {a}"))
        .unwrap_or_default();
    ctx.renderer.write_line(
        &format!("[plugin] swapped model: {old_model} → {new_model_compact}{provider_note}"),
        c_agent(),
    )?;
    Ok(())
}

// dirge-qhfk stage 3b: handle_done no longer holds the plugin-manager lock
// across an await — the on-response/message-end/on-complete/prepare-next-run
// chain (plus its model-swap rebuild) moved OFF the loop into `done_phase`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_done(
    ctx: &mut RunCtx<'_>,
    response: CompactString,
    tokens: u64,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    agent: &mut AnyAgent,
    // dirge-4y4l: the ~10 build_agent inputs bundled (see AgentBuildDeps).
    deps: &AgentBuildDeps<'_>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    // dirge-4koy: the loop parks the spawned `/plan` reviewer here; its
    // `review_phase` arm applies the verdict after the reviewer (which runs
    // code, off-thread) finishes.
    review_phase: &mut Option<crate::agent::plan::runtime::ReviewPhaseHandle>,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        &std::sync::Arc<std::sync::Mutex<PluginManager>>,
    >,
    // dirge-qhfk: the off-loop Done hook chain is parked here; the `done_phase`
    // arm applies the model swap and runs finish_done once it resolves.
    #[cfg(feature = "plugin")] done_phase: &mut Option<crate::ui::done_phase::DonePhaseHandle>,
    #[cfg(feature = "loop")] loop_bits: LoopBits<'_>,
    #[cfg(feature = "vigil")] vigil_bits: VigilBits<'_>,
) -> anyhow::Result<()> {
    *was_reasoning = false;
    // Dispatch on-vigil-observance hook now that we have the agent's response.
    // The vigil select! arm stored pending observance metadata in VigilState;
    // we fire the hook here so it sees :response and :exit.
    #[cfg(feature = "vigil")]
    if let Some(vs) = vigil_bits.state
        && let Some(pending) = vs.pending_observance.take()
    {
        #[cfg(feature = "plugin")]
        if let Some(pm) = plugin_manager {
            let ctx = crate::extras::vigil::observance_context(
                &pending.vigil_name,
                pending.event_count,
                &response,
            );
            let pm = pm.clone();
            tokio::task::spawn_blocking(move || {
                pm.lock_ignore_poison()
                    .dispatch_tool_hook("on-vigil-observance", &ctx)
            })
            .await
            .ok();
        }
        // Release the in-flight flag so the vigil can fire again on its next
        // reap window. The reaper skips a vigil while this flag is set.
        pending
            .running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    // A successful turn must not leave a chamber
    // half-painted. If anything slipped through
    // — show_details=false skipping the body, an
    // in-flight Ask the user resolved with a path
    // that didn't reach the bottom paint, etc. —
    // close with a plain chamber bottom (not the
    // `⚠ tool denied · aborted` wording, which
    // would mislead the user about an otherwise-
    // successful run).
    if *ctx.tool_chamber_open {
        // Same drop-or-close logic as
        // close_tool_chamber_passive: if no
        // body content was added since the
        // TOP was painted (result never
        // arrived from the agent — MCP timeout,
        // network blip, agent loop bug), drop
        // the chamber entirely instead of
        // leaving an empty box on screen.
        // Otherwise close with a bottom border.
        let drop_chamber = match (*ctx.chamber_top_start, *ctx.chamber_top_end) {
            (Some(_), Some(end)) => ctx.renderer.buffer_len() == end,
            _ => false,
        };
        if drop_chamber {
            if let Some(start) = *ctx.chamber_top_start {
                ctx.renderer.replace_from(start, Vec::new());
            }
        } else {
            // dirge-ghpf: reflowing chamber bottom.
            ctx.renderer.write_chamber_bottom(theme::dim())?;
        }
        *ctx.tool_chamber_open = false;
        *ctx.chamber_top_start = None;
        *ctx.chamber_top_end = None;
    }
    *ctx.last_tool_name = None;
    // The idle `Done` face is deferred until the turn actually settles idle
    // (finish_done's tail / finalize_idle_turn). Painting it here — before the
    // plugin hook chain and `/plan` reviewer decide whether the runner stays
    // busy — showed `(^_^)` while `is_running` was still true, so typed
    // messages queued behind an idle-looking avatar (GH #621).
    #[cfg(feature = "experimental-ui-terminal-tab")]
    ctx.renderer.set_last_tool_name("");

    // dirge-qhfk stage 3b: run the on-response/message-end/on-complete/
    // prepare-next-run chain OFF the loop so a hook opening a dialog can't
    // freeze the single-threaded runtime that services dialog_rx. The turn's
    // runner is finished (Done was terminal), so tear it down now — otherwise
    // the loop would re-poll the closed channel while the phase runs. Keep
    // is_running=true so further submits queue as steering rather than starting
    // a new turn mid-chain. The done_phase arm applies the model swap and runs
    // finish_done once the chain resolves.
    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        if let Some(h) = agent_abort.take() {
            h.abort();
        }
        *agent_rx = None;
        *agent_interject = None;
        *agent_cancel = None;
        // is_running stays true while the hook chain runs off-loop, so keep a
        // working face rather than the idle `Done` (GH #621).
        ctx.renderer
            .set_avatar_state(avatar::AvatarState::settled(true));
        *done_phase = Some(crate::ui::done_phase::spawn(pm.clone(), response, tokens));
        return Ok(());
    }

    // No plugin (feature off or no manager): no hook chain, rewrite, or model
    // swap — run the tail directly with the response as-is.
    finish_done(
        ctx,
        response,
        tokens,
        agent,
        is_running,
        deps,
        None,
        agent_rx,
        agent_abort,
        agent_interject,
        agent_cancel,
        interjection_queue,
        review_phase,
        #[cfg(feature = "plugin")]
        plugin_manager,
        #[cfg(feature = "loop")]
        loop_bits,
        #[cfg(feature = "vigil")]
        vigil_bits,
    )
    .await
}

/// Finalize a completed turn once the plugin `on-response`/`message-end`/
/// `on-complete`/`prepare-next-run` chain has resolved: render + seal the
/// response, persist the turn, run the post-done action (follow-up / loop /
/// idle), drive the `/plan` reviewer, and finalize when idle. Split out of
/// `handle_done` (dirge-qhfk stage 3a) so the off-loop done-chain completion
/// arm can run the exact same tail after the plugin hooks finish on a task.
/// `response` is already the FINAL text (message-end rewrite applied);
/// `plugin_followup` is the on-response follow-up prompt, if any.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_done(
    ctx: &mut RunCtx<'_>,
    response: CompactString,
    tokens: u64,
    agent: &mut AnyAgent,
    is_running: &mut bool,
    deps: &AgentBuildDeps<'_>,
    #[cfg_attr(not(feature = "plugin"), allow(unused_variables))] plugin_followup: Option<String>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    review_phase: &mut Option<crate::agent::plan::runtime::ReviewPhaseHandle>,
    // Used only by the experimental-graph-search entity/relation drain below.
    #[cfg(feature = "plugin")]
    #[cfg_attr(not(feature = "experimental-graph-search"), allow(unused_variables))]
    plugin_manager: Option<&std::sync::Arc<std::sync::Mutex<PluginManager>>>,
    #[cfg(feature = "loop")] loop_bits: LoopBits<'_>,
    #[cfg(feature = "vigil")] vigil_bits: VigilBits<'_>,
) -> anyhow::Result<()> {
    let bg_store = deps.bg_store;

    if !ctx.response_buf.is_empty() {
        // dirge-qy3y: final render through the source-tracked stream API (the
        // open block created during streaming), so the committed response is a
        // reflowable markdown block. `commit_stream` seals it.
        ctx.renderer.stream(ctx.response_buf, c_agent(), true);
        ctx.renderer.render_viewport()?;
    } else if !*ctx.agent_line_started {
        ctx.renderer.write("<dirge> ", c_agent())?;
    }
    // Seal any open streamed block (response above, or reasoning-only turn).
    // dirge-fw0p: `end_stream`, not `commit_stream` — the turn is over, so the
    // next one must not look like a continuation of this text.
    ctx.renderer.end_stream();

    ctx.renderer.write_line("", Color::White)?;
    ctx.renderer.write_line("", Color::White)?;
    // Phase 3: persist structured tool calls
    // alongside the assistant text so the next
    // resume sees the full tool_use/tool_result
    // pairs in convert_history.
    // dirge-byun: don't persist a turn that produced nothing at all. A
    // reasoning-only turn (the model thinks, emits no prose, calls no tool)
    // would otherwise be stored with empty content and replayed on every
    // later prompt as an empty text content block — which Moonshot/Kimi and
    // GLM reject with `400 invalid_request_error: text content is empty`,
    // wedging the session permanently. The replay boundaries drop it anyway
    // (`runner::convert_history`), so keeping it here only adds a session
    // entry nothing can use. The abort path (`interjected.rs`) has always
    // skipped empty turns for the same reason.
    let tool_calls = std::mem::take(ctx.tool_calls_buf);
    if !response.is_empty() || !tool_calls.is_empty() {
        ctx.session
            .add_message_with_tool_calls(MessageRole::Assistant, &response, tool_calls);
    }
    // `tokens` is the run's real provider usage (input + output, from the
    // bridge's per-run accumulation). `cost` stays 0.0 by decision — cost
    // display was dropped (rates can't be sourced or kept fresh offline) —
    // so `total_cost` is never advanced here.
    ctx.session.total_tokens = ctx.session.total_tokens.saturating_add(tokens);
    // Run ended cleanly — reset the per-run tool-
    // call counter so the next user submission
    // starts at zero. Mirrored in the Interjected
    // branch + both abort paths below.
    *ctx.tool_calls_this_run = 0;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    // Stash the turn's thinking before clearing so Ctrl+O can still expand
    // it after the turn completes.
    ctx.end_reasoning();
    *ctx.reasoning_start_line = None;

    // No eager post-turn auto-compaction here (dirge-21sb). Running the
    // summarizer inline froze the UI for the 10-60s it took, and this is the
    // most common trigger. The two non-blocking paths now cover it: the next
    // user prompt compacts preemptively off-thread (the original motivation —
    // stop users typing into an over-full context), and any follow-up turn
    // (loop / reviewer / plugin / drained interjection) that still overflows
    // recovers reactively via handle_context_overflow, also off-thread. Both
    // keep the loop responsive and Ctrl+C-able.

    if !ctx.cli.no_session
        && let Err(e) = crate::session::storage::save_session(ctx.session)
    {
        ctx.renderer.write_line(
            &format!("warning: failed to save session: {}", e),
            c_error(),
        )?;
    }
    *is_running = false;
    if let Some(h) = agent_abort.take() {
        h.abort();
    }
    *agent_rx = None;
    *agent_interject = None;
    *agent_cancel = None;

    #[cfg(feature = "plugin")]
    let followup_for_decision = plugin_followup.clone();
    #[cfg(not(feature = "plugin"))]
    let followup_for_decision: Option<String> = None;

    #[cfg(feature = "loop")]
    let (loop_active, loop_should_stop) = loop_bits
        .state
        .as_ref()
        .map(|ls| (ls.active, ls.active && ls.should_stop()))
        .unwrap_or((false, false));
    #[cfg(not(feature = "loop"))]
    let (loop_active, loop_should_stop) = (false, false);

    #[cfg(feature = "vigil")]
    let vigil_active = vigil_bits
        .state
        .as_ref()
        .map(|vs| vs.active)
        .unwrap_or(false);

    #[cfg(feature = "vigil")]
    let action = crate::plugin::decide_post_done_action(
        followup_for_decision,
        loop_active,
        loop_should_stop,
        vigil_active,
    );
    #[cfg(not(feature = "vigil"))]
    let action = crate::plugin::decide_post_done_action(
        followup_for_decision,
        loop_active,
        loop_should_stop,
    );

    match action {
        crate::plugin::PostDoneAction::Followup(text) => {
            let followup_prompt = text + "\n\nContinue.";
            ctx.last_user_prompt.clone_from(&followup_prompt);
            let runner = agent.clone().spawn_runner(
                crate::provider::Prompt::text(
                    crate::agent::tools::background::prepend_pending_notifications(
                        &followup_prompt,
                        bg_store.as_ref(),
                    ),
                ),
                crate::agent::runner::convert_history(ctx.session),
                Some(interjection_queue.clone()),
                Some(ctx.session.assets_dir()),
            );
            runner.install_into(
                agent_rx,
                agent_abort,
                agent_interject,
                agent_cancel,
                is_running,
            );
        }
        crate::plugin::PostDoneAction::LoopStop =>
        {
            #[cfg(feature = "loop")]
            if let Some(ls) = loop_bits.state.as_mut() {
                ctx.renderer.write_line(
                    &format!("[loop] max iterations ({}) reached, stopping", ls.iteration),
                    c_agent(),
                )?;
                ls.active = false;
                *loop_bits.label = None;
            }
        }
        crate::plugin::PostDoneAction::LoopIter =>
        {
            #[cfg(feature = "loop")]
            if let Some(ls) = loop_bits.state.as_mut() {
                let summary: String = response.chars().take(200).collect();
                ls.last_summary = Some(summary);
                ls.iteration += 1;
                let prompt = ls.build_prompt();
                ctx.last_user_prompt.clone_from(&prompt);
                let runner = agent.clone().spawn_runner(
                    crate::provider::Prompt::text(
                        crate::agent::tools::background::prepend_pending_notifications(
                            &prompt,
                            bg_store.as_ref(),
                        ),
                    ),
                    Vec::new(),
                    Some(interjection_queue.clone()),
                    None,
                );
                runner.install_into(
                    agent_rx,
                    agent_abort,
                    agent_interject,
                    agent_cancel,
                    is_running,
                );
                *loop_bits.label = Some(ls.iteration_label());
                ctx.renderer.write_line(
                    &format!("[loop] launching {}", ls.iteration_label()),
                    c_agent(),
                )?;
            }
        }
        crate::plugin::PostDoneAction::Idle => {}
        #[cfg(feature = "vigil")]
        crate::plugin::PostDoneAction::VigilSleep => {
            // Vigil mode: don't auto-follow-up or loop; just sleep.
            // The select! loop's vigil-wake arm will observe and
            // launch the next agent turn.
        }
    }

    // Phased `/plan` reviewer loop (P3e-b). If this `Done` closed a plan-driven
    // implement run and nothing else (plugin follow-up / loop iteration) claimed
    // the next turn, a write-disabled reviewer runs the code and either approves
    // or relaunches the implement run with the punch-list. See `plan_review`.
    // Clone the turn's tool calls out before the `&mut ctx` call (they're
    // carried into the reviewer handle for the deferred finalize).
    let plan_review_tool_calls = ctx.tool_calls_buf.clone();
    super::plan_review::drive_plan_review(
        ctx,
        agent,
        &response,
        &plan_review_tool_calls,
        review_phase,
        is_running,
    )?;

    // Phase 4: when the session is truly idle (no plugin follow-up, loop
    // iteration, or worktree cleanup claimed the next turn) finalize the turn —
    // persist it, spawn the post-session learning pass, and drain any queued
    // interjections. Skipped when a follow-up is in flight, INCLUDING a spawned
    // `/plan` reviewer (dirge-4koy): drive_plan_review keeps `is_running` true,
    // so finalization runs later from the review_phase arm's terminal verdict
    // (via this same `finalize_idle_turn`) rather than now.
    if !*is_running {
        // Persist entity/relation records drained from the Janet harness
        // accumulators (#393), then build the graph context for the next turn's
        // system prompt. Best-effort: silently skip on DB errors. Gated to
        // experimental-graph-search (the SQL tables) + plugin (the harness
        // accumulators). Runs before finalize_idle_turn so the system-prompt
        // append lands before any interjection drain spawns the next turn.
        #[cfg(all(feature = "experimental-graph-search", feature = "plugin"))]
        {
            let paths = crate::extras::dirge_paths::ProjectPaths::new(
                &std::env::current_dir().unwrap_or_else(|_| ".".into()),
            );
            if let Some(pm) = plugin_manager {
                let mut mgr = pm.lock_ignore_poison();
                let entities = mgr.drain_entity_records();
                let relations = mgr.drain_relation_records();
                if (!entities.is_empty() || !relations.is_empty())
                    && let Ok(db) =
                        crate::extras::session_db::SessionDb::open(&paths.session_db_path())
                {
                    use crate::extras::entity_db;
                    let sid = crate::text::db_session_id(ctx.session.id.as_str());
                    for ent in &entities {
                        let _ = entity_db::upsert_entity(
                            &db.conn,
                            &sid,
                            None,
                            &ent.kind,
                            &ent.name,
                            ent.extra.as_deref(),
                        );
                    }
                    for rel in &relations {
                        let source_id =
                            entity_db::resolve_entity(&db.conn, &rel.source_kind, &rel.source_name);
                        let target_id =
                            entity_db::resolve_entity(&db.conn, &rel.target_kind, &rel.target_name);
                        if let (Ok(Some(src_eid)), Ok(Some(tgt_eid))) = (source_id, target_id) {
                            let _ = entity_db::insert_relation(
                                &db.conn,
                                src_eid,
                                tgt_eid,
                                &rel.rel_type,
                                &sid,
                            );
                        }
                    }
                }
            }

            // Build graph context for next turn's system prompt.
            if let Some(pm) = plugin_manager
                && let Ok(db) = crate::extras::session_db::SessionDb::open(&paths.session_db_path())
            {
                let sid = crate::text::db_session_id(ctx.session.id.as_str());
                if let Ok(context) =
                    crate::extras::entity_compress::build_graph_context(&db.conn, &sid)
                    && !context.is_empty()
                {
                    let mut mgr = pm.lock_ignore_poison();
                    let escaped = crate::plugin::escape_janet_string(&context);
                    let _ = mgr.eval(&format!("(harness/append-system-prompt {})", escaped));
                }
            }
        }

        finalize_idle_turn(
            ctx.session,
            ctx.last_user_prompt,
            &response,
            ctx.tool_calls_buf,
            agent,
            bg_store,
            interjection_queue,
            agent_rx,
            agent_abort,
            agent_interject,
            agent_cancel,
            is_running,
            ctx.cfg.memory_graduation.unwrap_or(true),
        )?;
        if !*is_running {
            crate::ui::desktop_notify::notify(
                ctx.cfg,
                crate::ui::desktop_notify::DesktopNotifyEvent::Completion,
            );
        }
    }
    // Settle the avatar to match the real run state: idle `Done` only when
    // nothing (a `/plan` reviewer, a drained interjection, a loop/plugin
    // follow-up) kept the runner busy; otherwise a working face (GH #621).
    ctx.renderer
        .set_avatar_state(avatar::AvatarState::settled(*is_running));
    Ok(())
}

/// Finalize a turn that left the session idle: persist it to the search DB,
/// spawn the (fire-and-forget) post-session learning pass, and drain any queued
/// interjections into a fresh turn. Extracted from `handle_done` so the spawned
/// `/plan` reviewer can run the SAME finalization from the `review_phase` arm
/// once its verdict lands (dirge-4koy) — the reviewer keeps the loop busy, so
/// `handle_done` can't finalize inline without racing the reviewer's relaunch.
/// The caller must only invoke this when the run is actually idle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_idle_turn(
    session: &mut crate::session::Session,
    last_user_prompt: &mut String,
    response: &str,
    tool_calls: &[crate::session::ToolCallEntry],
    agent: &AnyAgent,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    is_running: &mut bool,
    graduation_enabled: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);

    // Persist the completed turn to the SQLite session DB for future search
    // (stable session id groups same-session messages; tool names + results
    // feed FTS5).
    persist_turn_to_db(session, last_user_prompt, response, tool_calls);

    // dirge-a62g: prepend a deterministic, model-free ground-truth digest
    // (files touched, commands run, goal, where we stopped, git diff --stat) so
    // the review ranks/classifies KNOWN facts instead of rediscovering them.
    // dirge-6rtt: build the session-derived digest on-thread (cheap), but defer
    // its `git diff --stat` subprocess to the post-session task.
    let base = crate::agent::review::build_transcript(session);
    let digest = crate::agent::session_digest::SessionDigest::from_session(session);

    // dirge-ba0m: unified post-session learning orchestrator — review, then
    // skills curator, then memory curator, strictly ordered inside ONE detached
    // task so a skill the review creates is flushed before the curator reads it
    // and the three runners never fire concurrently. Fire-and-forget.
    // dirge-1elu.7: deterministic expectation signals. The verifier gate is a
    // run_loop local and is gone by the time this idle path runs, so the run
    // hands its final status over through the session-keyed slot in
    // `verifier::run_status`. Taking it is consuming: a later run that
    // verified nothing finds an empty slot and reads as "could not tell"
    // (NEITHER counter moves) rather than inheriting an earlier green.
    let expectation_commands = digest.commands.clone();
    let final_verification =
        crate::agent::agent_loop::verifier::take_run_verification(session.id.as_ref());
    crate::agent::post_session::spawn_post_session(
        agent.clone(),
        paths,
        digest,
        base,
        graduation_enabled,
        crate::extras::memory_db::ExpectationSignals {
            final_verification,
            commands: expectation_commands,
        },
    );

    // Drain the interjection queue: concatenate all queued messages into one
    // new user turn and launch it against the now-stable agent/cwd. No
    // write_user_lines here — the loop's MessageStart{User} →
    // AgentEvent::UserMessage bridge renders the text once (commit 7584bdf).
    if !interjection_queue.lock().unwrap().is_empty() {
        let queued: Vec<String> = interjection_queue.lock().unwrap().drain(..).collect();
        let combined = queued.join("\n\n");
        last_user_prompt.clone_from(&combined);
        let history = crate::agent::runner::convert_history(session);
        session.add_message(MessageRole::User, &combined);

        let runner = agent.clone().spawn_runner(
            crate::provider::Prompt::text(
                crate::agent::tools::background::prepend_pending_notifications(
                    &combined,
                    bg_store.as_ref(),
                ),
            ),
            history,
            Some(interjection_queue.clone()),
            Some(session.assets_dir()),
        );
        runner.install_into(
            agent_rx,
            agent_abort,
            agent_interject,
            agent_cancel,
            is_running,
        );
    }
    Ok(())
}

// dirge-m6ut: the plugin prepare-next-run swap now hops providers. These
// exercise the client-swapping half (`prepare_next_model_client`) directly —
// the pure routing decision is covered by `provider::route` tests. Gated
// on `plugin` because that's what compiles the function.
#[cfg(all(test, feature = "plugin"))]
mod next_model_tests {
    use super::*;
    use crate::config::{Config, ProviderEntry};
    use crate::session::Session;
    use std::collections::HashMap;

    /// deepseek default + a configured glm provider, both with a literal key so
    /// `create_client_with_auth` builds without touching the environment.
    fn cfg_with_glm() -> Config {
        let providers = HashMap::from([
            (
                "deepseek".to_string(),
                ProviderEntry {
                    model: Some("deepseek-v4-pro".to_string()),
                    api_key: Some("fake-ds".to_string()),
                    ..Default::default()
                },
            ),
            (
                "glm".to_string(),
                ProviderEntry {
                    provider_type: Some("glm".to_string()),
                    model: Some("glm-5.2".to_string()),
                    api_key: Some("fake-glm".to_string()),
                    ..Default::default()
                },
            ),
        ]);
        Config {
            providers: Some(providers),
            ..Default::default()
        }
    }

    fn deepseek_client(cfg: &Config) -> crate::provider::AnyClient {
        crate::provider::create_client_with_auth("deepseek", None, &cfg.providers_map(), cfg.auth)
            .expect("build deepseek client")
    }

    fn provider_of(client: &crate::provider::AnyClient) -> &'static str {
        client.completion_model("probe".to_string()).provider_name()
    }

    #[test]
    fn free_form_glm_id_swaps_the_live_client_to_glm() {
        let cfg = cfg_with_glm();
        let mut client = deepseek_client(&cfg);
        assert_eq!(provider_of(&client), "deepseek");
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let session = Session::new("deepseek", "deepseek-v4-pro", 128_000);

        // `glm-4.6` isn't pinned anywhere, but its family maps to the configured
        // glm provider — the client must actually hop there (dirge-m6ut).
        let action =
            prepare_next_model_client(&mut client, &mut renderer, &session, &cfg, "glm-4.6")
                .expect("prepare");
        match action {
            NextModelAction::Apply { swapped_provider } => {
                assert_eq!(swapped_provider.as_deref(), Some("glm"))
            }
            NextModelAction::Skip => panic!("expected a provider swap, got Skip"),
        }
        assert_eq!(provider_of(&client), "glm", "live client must now be glm");
    }

    #[test]
    fn same_provider_rename_keeps_the_client() {
        let cfg = cfg_with_glm();
        let mut client = deepseek_client(&cfg);
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let session = Session::new("deepseek", "deepseek-v4-pro", 128_000);

        let action = prepare_next_model_client(
            &mut client,
            &mut renderer,
            &session,
            &cfg,
            "deepseek-v4-flash",
        )
        .expect("prepare");
        assert!(
            matches!(
                action,
                NextModelAction::Apply {
                    swapped_provider: None
                }
            ),
            "same-provider rename must not swap the client"
        );
        assert_eq!(provider_of(&client), "deepseek");
    }

    #[test]
    fn unconfigured_family_is_skipped_and_client_untouched() {
        let cfg = cfg_with_glm(); // no anthropic provider
        let mut client = deepseek_client(&cfg);
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let session = Session::new("deepseek", "deepseek-v4-pro", 128_000);

        let action =
            prepare_next_model_client(&mut client, &mut renderer, &session, &cfg, "claude-opus-4")
                .expect("prepare");
        assert!(matches!(action, NextModelAction::Skip));
        assert_eq!(
            provider_of(&client),
            "deepseek",
            "an unroutable family must leave the client untouched"
        );
    }

    #[test]
    fn empty_or_unchanged_model_is_skipped() {
        let cfg = cfg_with_glm();
        let mut client = deepseek_client(&cfg);
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let session = Session::new("deepseek", "deepseek-v4-pro", 128_000);

        for id in ["", "   ", "deepseek-v4-pro"] {
            let action = prepare_next_model_client(&mut client, &mut renderer, &session, &cfg, id)
                .expect("prepare");
            assert!(matches!(action, NextModelAction::Skip), "id {id:?}");
        }
        assert_eq!(provider_of(&client), "deepseek");
    }
}
