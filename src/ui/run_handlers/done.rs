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
use crate::ui::tool_display::{chamber_bottom, chamber_widths};

/// Optional loop-feature state passed through to `handle_done`.
/// Behind `cfg(feature = "loop")` we hand the real mutable state;
/// without the feature, the placeholder type is `()` so the call
/// site doesn't need to thread a sentinel.
#[cfg(feature = "loop")]
pub(crate) struct LoopBits<'a> {
    pub state: &'a mut Option<crate::extras::r#loop::LoopState>,
    pub label: &'a mut Option<String>,
}

// False positive for `await_holding_lock`: the plugin-manager guard
// IS held during dispatch calls (sync) but is explicitly `drop(mgr)`-ed
// at line 209 BEFORE the `build_agent(...).await` at line 236, and
// re-acquired in a new scope at line 263. Clippy can't trace the
// `drop()` so it flags the outer `let mut mgr` as held across the
// await even though it isn't.
// `unused_mut` allowed: `response`'s `mut` is consumed only by the
// plugin-gated `message-end` rewrite, so non-plugin builds see it
// as unused.
#[allow(clippy::too_many_arguments, clippy::await_holding_lock, unused_mut)]
pub(crate) async fn handle_done(
    ctx: &mut RunCtx<'_>,
    // dirge-lsoq: `mut` so the `message-end` plugin hook can rewrite
    // the finalized assistant text before it is stored/persisted.
    mut response: CompactString,
    tokens: u64,
    cost: f64,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    agent: &mut AnyAgent,
    // Used only by the plugin-gated model-swap rebuild below; mark it used in
    // no-plugin builds so `-D warnings` stays clean.
    #[cfg_attr(not(feature = "plugin"), allow(unused_variables))] context: &mut ContextFiles,
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
    #[cfg(feature = "loop")] loop_bits: LoopBits<'_>,
) -> anyhow::Result<()> {
    // Rebind the bundled deps to locals so the body below reads unchanged.
    // Most of these feed ONLY the plugin-gated model-swap rebuild below (a
    // plugin's `prepare-next-run` setting `harness-next-model`), so they're
    // gated to `plugin` — otherwise a no-plugin build (e.g. windows-default)
    // sees them unused and `-D warnings` fails. `bg_store` stays ungated: it
    // also feeds `finalize_idle_turn`.
    #[cfg(feature = "plugin")]
    let client = deps.client;
    #[cfg(feature = "plugin")]
    let permission = deps.permission;
    #[cfg(feature = "plugin")]
    let ask_tx = deps.ask_tx;
    #[cfg(feature = "plugin")]
    let question_tx = deps.question_tx;
    #[cfg(feature = "plugin")]
    let plan_tx = deps.plan_tx;
    let bg_store = deps.bg_store;
    #[cfg(feature = "plugin")]
    let sandbox = deps.sandbox;
    #[cfg(all(feature = "mcp", feature = "plugin"))]
    let mcp_manager = deps.mcp_manager;
    #[cfg(all(feature = "semantic", feature = "plugin"))]
    let semantic_manager = deps.semantic_manager;
    #[cfg(all(feature = "lsp", feature = "plugin"))]
    let lsp_manager = deps.lsp_manager;
    *was_reasoning = false;
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
            let (frame_w, _) = chamber_widths(ctx.renderer);
            ctx.renderer
                .write_line_raw(&chamber_bottom(frame_w), theme::dim())?;
        }
        *ctx.tool_chamber_open = false;
        *ctx.chamber_top_start = None;
        *ctx.chamber_top_end = None;
    }
    *ctx.last_tool_name = None;
    ctx.renderer.set_avatar_state(avatar::AvatarState::Done);
    #[cfg(feature = "experimental-ui-terminal-tab")]
    ctx.renderer.set_last_tool_name("");

    #[allow(unused_mut, unused_variables)]
    let mut plugin_followup: Option<String> = None;
    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        let mut mgr = pm.lock_ignore_poison();
        match mgr.dispatch(
            "on-response",
            &format!(
                "@{{:response \"{}\"}}",
                crate::plugin::escape_janet_string(&response)
            ),
        ) {
            Ok(results) if !results.is_empty() => {
                for line in &results {
                    // Sanitize plugin output (ANSI injection defense).
                    let safe = crate::ui::events::sanitize_output(line);
                    ctx.renderer
                        .write_line(&format!("[plugin] {}", safe), theme::dim())?;
                }
                plugin_followup = Some(results.join("\n"));
            }
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] on-response error: {e}"), c_error())?;
            }
        }
        // Check for pending prompts queued by on-response
        if let Some(pending) = mgr.take_pending_prompt() {
            plugin_followup = Some(pending);
        }
        // dirge-lsoq: fire `message-end` so a plugin can rewrite the
        // finalized assistant text via `harness/rewrite-message`. The
        // text already streamed to the screen; this rewrites what is
        // STORED + persisted (session DB, store_response), enabling
        // post-hoc redaction/annotation of stored history.
        match mgr.dispatch(
            "message-end",
            &format!(
                "@{{:message \"{}\"}}",
                crate::plugin::escape_janet_string(&response)
            ),
        ) {
            Ok(_) => {
                if let Some(rewritten) = mgr.take_message_rewrite() {
                    response = compact_str::CompactString::new(&rewritten);
                }
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] message-end error: {e}"), c_error())?;
            }
        }
        mgr.store_response(&response);
        // Fire on-complete after on-response so
        // plugins can react to "turn fully done."
        // Previously this hook was in HOOK_NAMES
        // (so plugins defining it got auto-aliased)
        // but no host site dispatched — silent fail.
        match mgr.dispatch("on-complete", "@{}") {
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] on-complete error: {e}"), c_error())?;
            }
        }
        // Fire `prepare-next-run` so plugins can
        // signal session-level state changes for
        // the next run. Closes the gap vs pi's
        // `prepareNextTurn` for the auto-apply
        // piece: when `harness-next-model` is
        // set, the agent is rebuilt with the new
        // model RIGHT HERE so the next user
        // prompt runs against it without
        // requiring `/model X`.
        //
        // Scope difference vs pi: pi fires
        // `prepareNextTurn` between TURNS within
        // a single agent run (and can swap model
        // mid-stream). dirge fires
        // `prepare-next-run` only between RUNS
        // (after Done). Mid-stream swap requires
        // breaking rig's multi-turn stream and
        // restarting with a new agent — that
        // would lose partial assistant state, so
        // we keep the swap at run boundaries.
        match mgr.dispatch("prepare-next-run", "@{}") {
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] prepare-next-run error: {e}"), c_error())?;
            }
        }
        let pending_next_model = mgr.take_pending_next_model();
        // Release the plugin-manager guard before any `.await` below —
        // `std::sync::MutexGuard` is `!Send`, and the agent rebuild
        // path awaits a future. Re-acquire after the await for the
        // final `set harness-response nil`.
        drop(mgr);
        if let Some(next_model) = pending_next_model {
            // Validate: empty string is a
            // misconfiguration. Don't replace the
            // active model with nothing.
            let trimmed = next_model.trim();
            if !trimmed.is_empty() && trimmed != ctx.session.model.as_str() {
                let new_model_compact = CompactString::new(trimmed);
                let model_obj = client.completion_model(new_model_compact.to_string());
                *agent = crate::provider::build_agent(
                    model_obj,
                    ctx.cli,
                    ctx.cfg,
                    context,
                    permission.clone(),
                    ask_tx.clone(),
                    question_tx.clone(),
                    plan_tx.clone(),
                    bg_store.clone(),
                    #[cfg(feature = "lsp")]
                    lsp_manager.cloned(),
                    sandbox.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_manager,
                    #[cfg(feature = "semantic")]
                    semantic_manager,
                    Some(ctx.session.id.to_string()),
                )
                .await;
                let old_model = ctx.session.model.clone();
                ctx.session.model = new_model_compact.clone();
                ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
                // Re-resolve context window for
                // the new model — mirrors the
                // `/model` slash behavior so a
                // 128k→1M jump (or vice versa)
                // updates the status indicator.
                let new_ctx = ctx.cfg.resolve_context_window(new_model_compact.as_str());
                if new_ctx != ctx.session.context_window {
                    ctx.session.context_window = new_ctx;
                }
                ctx.renderer.write_line(
                    &format!(
                        "[plugin] swapped model: {} → {}",
                        old_model, new_model_compact,
                    ),
                    c_agent(),
                )?;
            }
        }
        // Clear `harness-response` so the next hook
        // doesn't see stale text from this turn. Re-acquire the
        // lock here since we released it above to satisfy
        // `clippy::await_holding_lock`.
        {
            let mut mgr = pm.lock_ignore_poison();
            let _ = mgr.eval("(set harness-response nil)");
        }
    }

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
    ctx.renderer.commit_stream();

    ctx.renderer.write_line("", Color::White)?;
    ctx.renderer.write_line("", Color::White)?;
    // Phase 3: persist structured tool calls
    // alongside the assistant text so the next
    // resume sees the full tool_use/tool_result
    // pairs in convert_history.
    ctx.session.add_message_with_tool_calls(
        MessageRole::Assistant,
        &response,
        std::mem::take(ctx.tool_calls_buf),
    );
    // TODO(cost-tracking): `tokens` here is the heuristic
    // estimate (text.len()/4) and `cost` is always 0.0 —
    // these accumulate into placeholder fields and won't
    // reflect actual provider usage / billing until we
    // pipe rig's `FinalResponse.usage()` through into
    // `AgentEvent::Done`. Kept as no-op-ish additions so
    // the wiring is in place when real values arrive.
    ctx.session.total_tokens = ctx.session.total_tokens.saturating_add(tokens);
    ctx.session.total_cost += cost;
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
                crate::agent::tools::background::prepend_pending_notifications(
                    &followup_prompt,
                    bg_store.as_ref(),
                ),
                crate::agent::runner::convert_history(ctx.session),
                Some(interjection_queue.clone()),
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
                    crate::agent::tools::background::prepend_pending_notifications(
                        &prompt,
                        bg_store.as_ref(),
                    ),
                    Vec::new(),
                    Some(interjection_queue.clone()),
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
                if !entities.is_empty() || !relations.is_empty() {
                    if let Ok(db) =
                        crate::extras::session_db::SessionDb::open(&paths.session_db_path())
                    {
                        use crate::extras::entity_db;
                        let sid =
                            format!("dirge-{}", crate::text::short_id(ctx.session.id.as_str()));
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
                            let source_id = entity_db::resolve_entity(
                                &db.conn,
                                &rel.source_kind,
                                &rel.source_name,
                            );
                            let target_id = entity_db::resolve_entity(
                                &db.conn,
                                &rel.target_kind,
                                &rel.target_name,
                            );
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
            }

            // Build graph context for next turn's system prompt.
            if let Some(pm) = plugin_manager {
                if let Ok(db) = crate::extras::session_db::SessionDb::open(&paths.session_db_path())
                {
                    let sid = format!("dirge-{}", crate::text::short_id(ctx.session.id.as_str()));
                    if let Ok(context) =
                        crate::extras::entity_compress::build_graph_context(&db.conn, &sid)
                    {
                        if !context.is_empty() {
                            let mut mgr = pm.lock_ignore_poison();
                            let escaped = crate::plugin::escape_janet_string(&context);
                            let _ =
                                mgr.eval(&format!("(harness/append-system-prompt {})", escaped));
                        }
                    }
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
        )?;
    }
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
    crate::agent::post_session::spawn_post_session(agent.clone(), paths, digest, base);

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
            crate::agent::tools::background::prepend_pending_notifications(
                &combined,
                bg_store.as_ref(),
            ),
            history,
            Some(interjection_queue.clone()),
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
