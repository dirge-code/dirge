#[allow(unused_imports)]
use crate::sync_util::LockExt;
use crossterm::style::Color;
use smallvec::SmallVec;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::{MessageRole, Session};
use crate::ui::events::render_session;
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::theme;

pub(crate) mod aliases;
mod cmd;
#[cfg(feature = "slash-completion")]
mod completion;

/// Outcome of dispatching a slash command. `Handled` is a normal completion
/// (the handler did its own rendering); the `Defer*` variants carry work the
/// command can't do itself and hand back to the UI event loop in `ui/mod.rs`.
///
/// This replaces the former `Err(anyhow!("DEFER_...:payload"))` control-flow
/// protocol, which matched on error-message prefixes and was non-exhaustive.
#[derive(Debug)]
pub(crate) enum SlashOutcome {
    Handled,
    /// `/compress` / `/compact`: run a compaction pass. `instructions` is the
    /// raw trailing text (None when bare); the consumer normalizes empty /
    /// `(none)` to a default compaction.
    DeferCompress {
        instructions: Option<String>,
    },
    /// `/learn`, `/prompt <name> <text>`: launch a streamed turn on `prompt`.
    DeferPromptRun {
        prompt: String,
    },
    /// `/btw <question>`: launch a turn on the user's question.
    DeferBtw {
        query: String,
    },
    /// `/edit`: open the input buffer in `$EDITOR`.
    #[cfg(unix)]
    DeferExternalEditor,
    /// `/wt-merge`: merge the worktree branch back into a target.
    #[cfg(feature = "git-worktree")]
    DeferWtMerge(cmd::wt_defer::WtMerge),
    /// `/wt-exit`: leave the worktree and return to the main repo.
    #[cfg(feature = "git-worktree")]
    DeferWtExit(cmd::wt_defer::WtExit),
}

#[cfg(feature = "slash-completion")]
pub use completion::{CompletionResult, format_completion_preview, ghost_suffix, try_complete};
// Only meaningful with plugins; gated to match its sole caller so a
// plugin-less build (e.g. windows-default) doesn't re-export a dead fn.
#[cfg(all(feature = "slash-completion", feature = "plugin"))]
pub use completion::register_plugin_commands;

#[cfg(feature = "slash-completion")]
pub use completion::register_alias_commands;

#[inline]
pub(super) fn c_agent() -> Color {
    theme::agent()
}
#[inline]
pub(super) fn c_result() -> Color {
    theme::result()
}
#[inline]
pub(super) fn c_error() -> Color {
    theme::error()
}

/// Bundle of mutable references that slash-command handlers need.
/// Keeps individual handler signatures tractable.
pub(super) struct SlashCtx<'a> {
    pub agent: &'a mut AnyAgent,
    // `&mut` so `/model` can swap the live client when switching to a model
    // that belongs to a different configured provider.
    pub client: &'a mut AnyClient,
    pub renderer: &'a mut Renderer,
    pub session: &'a mut Session,
    pub cli: &'a Cli,
    pub cfg: &'a Config,
    pub context: &'a mut ContextFiles,
    pub show_reasoning: &'a mut bool,
    pub is_running: &'a mut bool,
    pub input: &'a mut InputEditor,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub question_tx: &'a Option<crate::agent::tools::question::QuestionSender>,
    pub plan_tx: &'a Option<crate::agent::tools::plan::PlanSwitchSender>,
    pub todo_tools_enabled: &'a mut bool,
    pub bg_store: &'a Option<crate::agent::tools::background::BackgroundStore>,
    pub sandbox: &'a Sandbox,
    /// mpsc sender for restarting the input reader after `/sandbox attach`.
    #[cfg(unix)]
    pub user_tx: &'a tokio::sync::mpsc::UnboundedSender<crate::event::UserEvent>,
    #[cfg(feature = "loop")]
    pub loop_state: &'a mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<&'a McpClientManager>,
    #[cfg(feature = "semantic")]
    pub semantic_manager: Option<&'a SemanticManager>,
    #[cfg(feature = "lsp")]
    pub lsp_manager: Option<&'a std::sync::Arc<crate::lsp::manager::LspManager>>,
    /// `/plan` spawns its explore→plan forks on a task and writes the handle
    /// here; the UI loop drains its events, launches the streamed implement run
    /// on `Ready`, and can Ctrl+C-abort it (slash handlers can't touch the
    /// loop's `agent_rx`/`is_running`/`select!` directly) [dirge-vuzz].
    pub plan_phase: &'a mut Option<crate::agent::plan::runtime::PlanPhaseHandle>,
}

/// Walk `cut_idx` forward until the message at that index is a
/// `User` message (or the index reaches `messages.len()`). This
/// guarantees the kept tail after compress starts with a User
/// message, which is what every provider expects after a System
/// summary. If `cut_idx` already points at a User message or is
/// past the end, no change. If no user message exists in the
/// tail, return `messages.len()` — caller surfaces the "nothing to
/// compress" message.
///
/// Matches opencode's `splitTurn` discipline
/// (`session/compaction.ts:161-184`).
/// Outcome of `undo_last`. `removed` is the number of messages popped;
/// `had_tool_calls` is set when at least one of the popped messages
/// had tool calls attached — the caller should surface a warning
/// because tool side effects (file writes, bash, MCP calls) are NOT
/// reverted by undo.
#[derive(Debug, Default)]
pub struct UndoOutcome {
    pub removed: usize,
    pub had_tool_calls: bool,
}

pub fn undo_last(session: &mut Session) -> UndoOutcome {
    let len = session.messages.len();
    if len == 0 {
        return UndoOutcome::default();
    }
    let mut outcome = UndoOutcome::default();
    let pop = |session: &mut Session, outcome: &mut UndoOutcome| {
        if let Some(last) = session.messages.last()
            && !last.tool_calls.is_empty()
        {
            outcome.had_tool_calls = true;
        }
        session.pop_last_message();
        outcome.removed += 1;
    };
    // Route through `pop_last_message` so the tree + message_store
    // stay in sync — P4c made direct .messages.pop() incorrect for
    // branched sessions.
    if session.messages[len - 1].role == MessageRole::Assistant {
        pop(session, &mut outcome);
        if session
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::User)
        {
            pop(session, &mut outcome);
        }
        return outcome;
    }
    if session.messages[len - 1].role == MessageRole::User {
        pop(session, &mut outcome);
    }
    outcome
}

/// Result of an attempted compression. `Compacted` means messages
/// were actually replaced; `NoOp` covers every path that returned
/// without shrinking the session (already-within-limits, nothing to
/// cut, summary too large). Callers driving auto-recovery (the
/// `ContextOverflow` handler) MUST distinguish these — respawning
/// the run against an unchanged history just re-emits the same
/// ContextLength error and loops.
pub enum CompressOutcome {
    Compacted,
    NoOp,
}

/// dirge-tv3p: the inputs a spawned compaction task + its install need —
/// produced by [`prepare_compaction`] on the UI thread.
pub(crate) struct CompactionRequest {
    /// The summarizer model, resolved on-thread. Cloneable, sent to the task.
    pub model: crate::provider::AnyModel,
    /// The fully-built compaction prompt (serialize + assemble done on-thread).
    pub prompt: String,
    /// Message prefix length to drop on install (`session.messages[..cut_idx]`).
    pub cut_idx: usize,
    /// Token cost of the dropped prefix — for the net-savings check on install.
    pub tokens_before: u64,
}

pub(crate) struct PruneOnlyCompactionRequest {
    /// Deterministic local summary; no side LLM call is made.
    pub summary: String,
    /// Message prefix length to drop on install (`session.messages[..cut_idx]`).
    pub cut_idx: usize,
    /// Token cost of the dropped prefix — for the net-savings check on install.
    pub tokens_before: u64,
}

/// Outcome of the cheap on-thread compaction decision.
pub(crate) enum CompactionDecision {
    /// Nothing to do — the reason was already rendered.
    NoOp,
    /// Run the summarizer (off-thread) then [`install_compaction`]. Boxed —
    /// `CompactionRequest` (model + prompt) is much larger than `NoOp`.
    Ready(Box<CompactionRequest>),
}

/// Fraction (percent) of the usable token budget at which proactive,
/// pre-send compaction kicks in. Below the hard 100% limit so we compact
/// BEFORE a send would overflow rather than paying the reactive round-trip.
pub(crate) const PROACTIVE_COMPACTION_PERCENT: u64 = 85;

/// Whether a proactive (pre-send) compaction is warranted: the current context
/// plus the `incoming` prompt would cross [`PROACTIVE_COMPACTION_PERCENT`] of
/// the usable budget (`max_tokens` = context window minus reserve). `total > 0`
/// skips a fresh session with nothing to compact; the caller still gates on
/// `compact_enabled`.
///
/// This trigger OWNS the proactive decision and uniquely factors `incoming`
/// (which [`prepare_compaction`] cannot see), so the caller must invoke
/// `prepare_compaction` with `forced = true`. Otherwise prepare's stricter
/// within-limits (100%) gate re-rejects everything in the 85–100% band — it
/// announced "compressing…" then no-op'd, so proactive compaction never ran
/// (dirge-rz4i).
pub(crate) fn preemptive_compaction_due(total: u64, incoming: u64, max_tokens: u64) -> bool {
    total > 0 && total.saturating_add(incoming) > max_tokens * PROACTIVE_COMPACTION_PERCENT / 100
}

/// Session-space compaction cut. Delegates to the canonical
/// `session::compact::compaction_cut_idx` so the `/compress` path and the
/// auto-fold persistence handler share ONE cut policy (dirge-4kgk — they must
/// not drift, or one path reintroduces the loop-vs-session over-drain).
fn compaction_cut_idx(session: &Session, cfg: &Config) -> usize {
    crate::session::compact::compaction_cut_idx(session, cfg.resolve_keep_recent_tokens())
}

fn tokens_before_cut(session: &Session, cut_idx: usize) -> u64 {
    session.messages[..cut_idx]
        .iter()
        .map(|m| m.estimated_tokens)
        .sum()
}

fn build_prune_only_summary(session: &Session, cut_idx: usize, reason: &str) -> String {
    let first_retained = session.messages.get(cut_idx).map(|m| {
        let preview: String = m.content.chars().take(160).collect();
        format!("{:?}: {}", m.role, preview.replace('\n', " "))
    });
    let previous_summary_note = session
        .compactions
        .last()
        .map(|_| "- A previous compaction summary existed, but was not re-summarized by an LLM during this emergency fallback.\n")
        .unwrap_or("");
    format!(
        "## Emergency prune-only compaction\n\
         - Reason: {reason}\n\
         - Dropped {cut_idx} older messages without LLM summarization to recover usable context.\n\
         - The dropped turns were not summarized; treat the remaining recent conversation as the source of truth.\n\
         {previous_summary_note}\
         - If important context is missing, ask the user for clarification instead of guessing.\n\
         - First retained message: {}",
        first_retained.unwrap_or_else(|| "(none)".to_string())
    )
}

pub(crate) fn prepare_prune_only_compaction(
    renderer: &mut Renderer,
    session: &Session,
    cfg: &Config,
    reason: &str,
) -> anyhow::Result<Option<PruneOnlyCompactionRequest>> {
    renderer.write_line("applying prune-only emergency compaction...", c_agent())?;
    let cut_idx = compaction_cut_idx(session, cfg);
    if cut_idx == 0 {
        renderer.write_line("nothing to prune (entire context is recent)", c_agent())?;
        return Ok(None);
    }
    let tokens_before = tokens_before_cut(session, cut_idx);
    Ok(Some(PruneOnlyCompactionRequest {
        summary: build_prune_only_summary(session, cut_idx, reason),
        cut_idx,
        tokens_before,
    }))
}

/// dirge-tv3p: the SYNCHRONOUS, on-UI-thread half of compaction — decide
/// whether/what to compact and build the summarizer prompt. Cheap (token math +
/// conversation serialization), so it stays on the loop; the slow LLM call it
/// sets up runs off-thread. Renders any "nothing to do" message itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_compaction(
    instructions: Option<&str>,
    forced: bool,
    agent: &AnyAgent,
    client: &AnyClient,
    renderer: &mut Renderer,
    session: &Session,
    cfg: &Config,
) -> anyhow::Result<CompactionDecision> {
    renderer.write_line("compressing...", c_agent())?;
    renderer.write_line("", Color::White)?;

    let reserve = cfg.resolve_reserve_tokens();
    let max_tokens = session.context_window.saturating_sub(reserve);

    // Non-forced reactive callers skip when context is within the hard limit;
    // an explicit `/compact` AND the proactive pre-send trigger pass
    // `forced = true` so they compact at their own (85%) threshold without
    // being re-rejected here (dirge-rz4i). The downstream gates (nothing to
    // compress, summary-larger-than-savings) still apply to both [dirge-fgtj].
    if !forced && session.total_estimated_tokens <= max_tokens {
        renderer.write_line("context within limits, no compression needed", c_agent())?;
        return Ok(CompactionDecision::NoOp);
    }

    let cut_idx = compaction_cut_idx(session, cfg);

    if cut_idx == 0 {
        renderer.write_line("nothing to compress (entire context is recent)", c_agent())?;
        return Ok(CompactionDecision::NoOp);
    }

    let messages_to_summarize = &session.messages[..cut_idx];
    let previous_summary = session.compactions.last().map(|c| c.summary.as_str());

    // dirge-7tvq: give the memory provider a chance to inject
    // provider-extracted insights into the compression prompt before
    // the to-be-discarded messages are summarized. Returns an empty
    // string for providers that don't override the hook.
    let provider_insights = agent.memory_provider().map(|p| {
        let pre_compress_transcript =
            crate::agent::review::build_transcript_from_slice(messages_to_summarize);
        crate::agent::review::fire_pre_compress(p.as_ref(), &pre_compress_transcript)
    });
    let augmented_instructions: Option<String> = match (instructions, provider_insights) {
        (Some(user), Some(extra)) if !extra.trim().is_empty() => {
            Some(format!("{}\n\nProvider insights:\n{}", user, extra))
        }
        (None, Some(extra)) if !extra.trim().is_empty() => {
            Some(format!("Provider insights:\n{}", extra))
        }
        (Some(user), _) => Some(user.to_string()),
        _ => None,
    };

    let prompt = crate::provider::build_compaction_prompt(
        messages_to_summarize,
        previous_summary,
        augmented_instructions.as_deref(),
    )?;
    let tokens_before = tokens_before_cut(session, cut_idx);
    let model = crate::provider::build_compaction_model(cfg, client, &session.model)?;

    Ok(CompactionDecision::Ready(Box::new(CompactionRequest {
        model,
        prompt,
        cut_idx,
        tokens_before,
    })))
}

/// Rebuild the agent from its constituent parts — the single shared body
/// every "rebuild after a config/session change" site routes through.
/// Reads `session.model` for model resolution. `rebuild_agent(&mut
/// SlashCtx)` is the SlashCtx-flavored wrapper; this bare-params form is
/// for sites that aren't holding a SlashCtx (the compaction install path
/// and `/wt-exit`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rebuild_agent_parts(
    agent: &mut AnyAgent,
    client: &AnyClient,
    session: &Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    question_tx: &Option<crate::agent::tools::question::QuestionSender>,
    plan_tx: &Option<crate::agent::tools::plan::PlanSwitchSender>,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) {
    let model = client.completion_model(session.model.to_string());
    *agent = crate::provider::build_agent(
        model,
        cli,
        cfg,
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
        Some(session.id.to_string()),
    )
    .await;
}

/// dirge-tv3p: the on-UI-thread INSTALL half — given the summary from the
/// (off-thread) summarizer, rotate the session and rebuild the agent. Cheap
/// relative to the LLM call. Refuses to install a summary larger than the
/// messages it replaces (audit M9).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn install_compaction(
    summary: String,
    cut_idx: usize,
    tokens_before: u64,
    agent: &mut AnyAgent,
    client: &AnyClient,
    renderer: &mut Renderer,
    session: &mut Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    question_tx: &Option<crate::agent::tools::question::QuestionSender>,
    plan_tx: &Option<crate::agent::tools::plan::PlanSwitchSender>,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> anyhow::Result<CompressOutcome> {
    // F13: estimate the summary's own token cost so we can
    // report TRUE net savings instead of just "tokens replaced".
    // A pathological summary longer than the messages it
    // replaces means we just paid more tokens for less context.
    // We still proceed with the compress (the new prefix is the
    // SHAPE the LLM expects), but we want to surface the
    // misfire so the user can adjust `keep_recent_tokens` or
    // their custom compress prompt. opencode validates the
    // summary fits the budget BEFORE issuing the LLM call
    // (`compaction.ts:136-294`); dirge validates AFTER because
    // we don't know the summary's size until the LLM returns.
    let summary_tokens_est = crate::session::Session::estimate_tokens(&summary);
    let net_saved: i64 = tokens_before as i64 - summary_tokens_est as i64;

    // Audit M9: previously the summary was installed via
    // `compress_reporting` BEFORE the net-saved check, so an
    // oversized summary still landed in the session — we only
    // told the user *afterwards*. Refuse to install when the
    // summary would cost more than the messages it replaces; the
    // user can adjust `keep_recent_tokens` / their compress prompt
    // and re-issue. Skipping the install also avoids polluting the
    // session-tree with a node we'd want to revert.
    if net_saved < 0 {
        renderer.write_line(
            &format!(
                "compress aborted — summary ({}t) is LARGER than the {} messages it would replace ({}t); net cost +{}t. Compression rejected. Consider lowering keep_recent_tokens or refining compress instructions, then re-run /compress.",
                summary_tokens_est,
                cut_idx,
                tokens_before,
                -net_saved,
            ),
            c_error(),
        )?;
        return Ok(CompressOutcome::NoOp);
    }

    // `compress_reporting` returns the count of non-active-path
    // tree nodes (sibling branches) pruned. We notify the user
    // about that loss explicitly — without the notification a
    // branched session could silently lose forks during auto-
    // compaction. opencode (`session/compaction.ts:386-396`) drops
    // siblings silently; dirge prefers the explicit notification.
    let pruned_branches = session.compress_reporting(summary, cut_idx, tokens_before);

    rebuild_agent_parts(
        agent,
        client,
        session,
        cli,
        cfg,
        context,
        permission,
        ask_tx,
        question_tx,
        plan_tx,
        bg_store,
        sandbox,
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
        #[cfg(feature = "lsp")]
        lsp_manager,
    )
    .await;

    render_session(renderer, session, cli, cfg, context)?;
    if pruned_branches > 0 {
        // Tell the user the branched topology shrunk. Without this,
        // they'd notice missing forks in `/tree` without any
        // explanation.
        renderer.write_line(
            &format!(
                "discarded {} forked branch node{} that were rooted in the compressed region",
                pruned_branches,
                if pruned_branches == 1 { "" } else { "s" },
            ),
            c_error(),
        )?;
    }
    // Net-saved is guaranteed non-negative here: the early-return
    // above (audit M9) aborts the compress when the summary would
    // cost more than the messages it replaced.
    {
        renderer.write_line(
            &format!(
                "compressed {} messages (saved ~{} tokens; summary uses {}t)",
                cut_idx, net_saved, summary_tokens_est,
            ),
            c_agent(),
        )?;
    }

    Ok(CompressOutcome::Compacted)
}

/// Split a slash-command line into whitespace-separated parts
/// (`parts[0]` = command, `parts[1]` = first arg, …). Runs of
/// whitespace collapse to a single separator, so `/sessions  <id>`
/// (extra spaces) parses identically to `/sessions <id>` — previously
/// a stray double space produced an empty middle token and broke the
/// arg. `parts[1..].join(" ")` still recovers the remainder for the
/// commands that want it.
fn split_command_parts(text: &str) -> SmallVec<[&str; 3]> {
    text.split_whitespace().collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_slash(
    text: &str,
    agent: &mut AnyAgent,
    client: &mut AnyClient,
    renderer: &mut Renderer,
    session: &mut Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    show_reasoning: &mut bool,
    is_running: &mut bool,
    input: &mut InputEditor,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    // Audit followup: same threading as ask_tx — every build_agent
    // rebuild inside handle_slash previously passed None for
    // question_tx + plan_tx, silently killing the `question` tool
    // and plan-switch hooks after any rebuild-triggering slash
    // command. Companion to the C8 LSP fix.
    question_tx: &Option<crate::agent::tools::question::QuestionSender>,
    plan_tx: &Option<crate::agent::tools::plan::PlanSwitchSender>,
    todo_tools_enabled: &mut bool,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(unix)] user_tx: &tokio::sync::mpsc::UnboundedSender<crate::event::UserEvent>,
    #[cfg(feature = "loop")] loop_state: &mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    // C8 (audit fix): every prior agent-rebuild path (/model,
    // /prompt, /mode, /cd, /worktree, /wt-exit, /regen-prompts,
    // /loop start/stop, /toggle) passed None for lsp_manager into
    // build_agent. The user lost LSP silently after the first such
    // command. Thread the actual manager through.
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
    plan_phase: &mut Option<crate::agent::plan::runtime::PlanPhaseHandle>,
) -> anyhow::Result<SlashOutcome> {
    let parts: SmallVec<[&str; 3]> = split_command_parts(text);
    let mut ctx = SlashCtx {
        agent,
        client,
        renderer,
        session,
        cli,
        cfg,
        context,
        show_reasoning,
        is_running,
        input,
        permission,
        ask_tx,
        question_tx,
        plan_tx,
        todo_tools_enabled,
        bg_store,
        sandbox,
        #[cfg(unix)]
        user_tx,
        #[cfg(feature = "loop")]
        loop_state,
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
        #[cfg(feature = "lsp")]
        lsp_manager,
        plan_phase,
    };
    match parts[0] {
        "/model" => cmd::model::cmd_model(&mut ctx, &parts).await?,
        "/sessions" => cmd::sessions::cmd_sessions(&mut ctx, &parts).await?,
        "/reasoning" => cmd::model::cmd_reasoning(&mut ctx).await?,
        "/mode" => cmd::mode::cmd_mode(&mut ctx, &parts).await?,
        #[cfg(feature = "mcp")]
        "/mcp" => cmd::mcp::cmd_mcp(&mut ctx, &parts).await?,
        "/toggle" => cmd::toggle::cmd_toggle(&mut ctx, &parts).await?,
        "/compress" | "/compact" => {
            // Deferred: the outer event loop in `ui/mod.rs` consumes the
            // DeferCompress outcome and runs `handle_compress` with the
            // freshly-built dependencies.
            return Ok(SlashOutcome::DeferCompress {
                instructions: compress_instructions(&parts),
            });
        }
        "/loop" => cmd::loop_cmd::cmd_loop(&mut ctx, &parts, text).await?,
        "/prompt" => return cmd::prompt::cmd_prompt(&mut ctx, &parts).await,
        "/agent" | "/agents" => cmd::agent::cmd_agent(&mut ctx, &parts).await?,
        "/plan" => cmd::plan::cmd_plan(&mut ctx, &parts, text).await?,
        "/plugins" => cmd::plugins::cmd_plugins(&mut ctx, &parts).await?,
        #[cfg(feature = "git-worktree")]
        "/worktree" => cmd::worktree::cmd_worktree(&mut ctx, &parts).await?,
        #[cfg(feature = "git-worktree")]
        "/wt-merge" => return cmd::worktree::cmd_wt_merge(&mut ctx, &parts).await,
        #[cfg(feature = "git-worktree")]
        "/wt-exit" => return cmd::worktree::cmd_wt_exit(&mut ctx, &parts).await,
        "/regen-prompts" => cmd::regen::cmd_regen_prompts(&mut ctx).await?,
        "/quit" => {
            cmd::quit::cmd_quit(&mut ctx).await?;
        }
        "/spec" => cmd::spec::cmd_spec(&mut ctx, &parts).await?,
        "/tasks" => cmd::tasks::cmd_tasks(&mut ctx).await?,
        "/cache" => cmd::cache::cmd_cache(&mut ctx).await?,
        "/clear" => cmd::clear::cmd_clear(&mut ctx).await?,
        "/tree" => cmd::tree::cmd_tree(&mut ctx, &parts).await?,
        "/fork" => cmd::fork::cmd_fork(&mut ctx, &parts).await?,
        "/clone" => cmd::clone::cmd_clone(&mut ctx, &parts).await?,
        "/panel" => cmd::panel::cmd_panel(&mut ctx, &parts).await?,
        "/display" => cmd::panel::cmd_display(&mut ctx, &parts).await?,
        "/btw" => return cmd::btw::cmd_btw(&mut ctx, &parts).await,
        "/learn" => return cmd::learn::cmd_learn(&mut ctx, &parts).await,
        "/code-review" => cmd::code_review::cmd_code_review(&mut ctx).await?,
        "/cd" => cmd::cd::cmd_cd(&mut ctx, text).await?,
        #[cfg(unix)]
        "/edit" => return Ok(SlashOutcome::DeferExternalEditor),
        "/undo" => cmd::undo::cmd_undo(&mut ctx).await?,
        "/retry" => cmd::retry::cmd_retry(&mut ctx).await?,
        "/allow" => cmd::allow::cmd_allow(&mut ctx, &parts, text).await?,
        "/why" => cmd::allow::why::cmd_why(&mut ctx, &parts).await?,
        "/help" => cmd::help::cmd_help(&mut ctx).await?,
        "/graph" => cmd::graph::cmd_graph(&mut ctx, &parts).await?,
        "/issues" => cmd::issues::cmd_issues(&mut ctx, &parts).await?,
        "/memory" => cmd::memory::cmd_memory(&mut ctx, &parts).await?,
        "/kill" => cmd::kill::cmd_kill(&mut ctx, &parts).await?,
        #[cfg(unix)]
        "/sandbox" => cmd::sandbox::cmd_sandbox(&mut ctx, &parts).await?,
        #[cfg(feature = "dap")]
        "/debug" => cmd::debug::cmd_debug(&mut ctx, &parts).await?,
        #[cfg(feature = "dap")]
        "/dap-repl" => cmd::debug::cmd_dap_repl(&mut ctx, &parts).await?,
        _ => {
            // If `slash_command_names()` advertised this command
            // but no match arm above caught it, the lists drifted
            // (added to the canonical list without wiring up the
            // dispatch). Emit a loud error here rather than falling
            // through to plugin lookup / "unknown command", so the
            // mistake is obvious in dev/test rather than silently
            // shadowed by either path. Plugin commands by
            // convention don't have a leading `/` in the canonical
            // list, so this won't false-fire on them.
            if is_known_slash_command(parts[0]) {
                ctx.renderer.write_line(
                    &format!(
                        "internal error: {} is listed in slash_command_names() but has no dispatch arm in handle_slash — wire it up or remove from the list",
                        parts[0]
                    ),
                    c_error(),
                )?;
                return Ok(SlashOutcome::Handled);
            }

            // Fall through to plugin-registered commands. The process-global
            // PluginManager is the same one HookedToolDyn uses, so we don't
            // need to thread an Arc through handle_slash's already long
            // parameter list.
            #[cfg(feature = "plugin")]
            if let Some(pm_arc) = crate::plugin::hook::global() {
                let cmd = parts[0].trim_start_matches('/');
                let args = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
                let handler = {
                    let mut mgr = pm_arc.lock_ignore_poison();
                    mgr.list_commands()
                        .into_iter()
                        .find(|(name, _)| name == cmd)
                        .map(|(_, h)| h)
                };
                if let Some(handler_fn) = handler {
                    let result = {
                        let mut mgr = pm_arc.lock_ignore_poison();
                        mgr.invoke_command(&handler_fn, &args)
                    };
                    match result {
                        Ok(Some(text)) => {
                            // Strip ANSI escapes from plugin output to
                            // prevent repaint/screen-manipulation attacks.
                            let safe = crate::ui::ansi::strip_escapes(
                                &text,
                                crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                            );
                            for line in safe.lines() {
                                ctx.renderer.write_line(line, c_agent())?;
                            }
                        }
                        Ok(None) => {
                            // Handler ran cleanly but had nothing to say — no-op.
                        }
                        Err(e) => {
                            ctx.renderer.write_line(
                                &format!("[plugin] {} failed: {}", cmd, e),
                                c_error(),
                            )?;
                        }
                    }
                    return Ok(SlashOutcome::Handled);
                }
            }
            ctx.renderer.write_line(
                &format!("unknown command: {} (try /help)", parts[0]),
                c_error(),
            )?;
        }
    }
    Ok(SlashOutcome::Handled)
}

/// `/compress [text...]`: the raw trailing text to feed the compaction pass,
/// or `None` when bare (so the consumer applies a default). This is the
/// payload of [`SlashOutcome::DeferCompress`]; the consumer normalizes empty
/// / `(none)` exactly as it did on the old `DEFER_COMPRESS:` string.
fn compress_instructions(parts: &[&str]) -> Option<String> {
    (parts.len() > 1).then(|| parts[1..].join(" "))
}

/// Canonical list of built-in slash commands paired with their
/// short `/help` description. **Single source of truth** — both
/// `slash_command_names()` (tab completion, `is_known_slash_command`)
/// and `slash_command_descriptions()` (the `/help` render) derive
/// from this, so adding a command means one entry here plus one
/// match arm in `handle_slash`. Nothing else.
///
/// **When you add a new slash command to `handle_slash`'s match
/// arms, add it here too.** A drift in the other direction (listed
/// here but no match arm in `handle_slash`) surfaces at runtime as
/// an explicit `internal error: known command X reached default
/// arm` so the mistake is loud rather than silent.
///
/// Always-compiled (not feature-gated) because `handle_slash`'s
/// default arm consults it regardless of the tab-completion
/// feature.
fn slash_commands() -> Vec<(&'static str, &'static str)> {
    let mut cmds = vec![
        ("/agent", "switch to a named agent, or turn agents off"),
        ("/agents", "list available agents"),
        ("/allow", "manage the session permission allowlist"),
        (
            "/btw",
            "ask a one-shot side question without disrupting the session",
        ),
        ("/cache", "show the cumulative prefix-cache hit ratio"),
        ("/cd", "change the working directory"),
        ("/clear", "clear the conversation and session state"),
        ("/clone", "clone the conversation path up to a message"),
        (
            "/compact",
            "summarize and compact the conversation (alias of /compress)",
        ),
        ("/compress", "summarize and compact the conversation"),
        ("/display", "choose which panes (left/main/right) to show"),
        #[cfg(unix)]
        ("/edit", "open the input buffer in $EDITOR"),
        (
            "/fork",
            "fork the conversation at a message; restore the original prompt",
        ),
        ("/graph", "query the entity/relation graph"),
        ("/help", "show this help"),
        ("/issues", "view the native issue board"),
        ("/kill", "kill a running subagent"),
        (
            "/code-review",
            "review the working-tree diff for issues (needs critic_provider)",
        ),
        (
            "/learn",
            "distill sources or this session into a reusable skill",
        ),
        ("/memory", "reload the memory snapshot mid-session"),
        ("/mode", "view or set the permission/security mode"),
        ("/model", "list configured models, or switch to one"),
        ("/panel", "toggle the side panels on or off"),
        ("/plan", "run the phased plan workflow on a request"),
        ("/plugins", "list or load plugins"),
        ("/prompt", "list, switch, or reset the active prompt layer"),
        ("/quit", "quit dirge"),
        ("/reasoning", "toggle reasoning visibility"),
        (
            "/regen-prompts",
            "regenerate built-in prompts and rebuild the agent",
        ),
        ("/retry", "edit and resend your last message"),
        #[cfg(unix)]
        (
            "/sandbox",
            "attach, snapshot, or reboot the microVM sandbox",
        ),
        ("/sessions", "list, switch, or delete saved sessions"),
        ("/spec", "inspect the spec-driven workflow tracker"),
        ("/tasks", "list subagent chats and background shells"),
        ("/toggle", "turn a feature (e.g. todo tools) on or off"),
        ("/tree", "show the conversation tree, or switch to a branch"),
        ("/undo", "undo the last user/agent message pair"),
        ("/why", "trace why an operation was allowed or denied"),
    ];
    #[cfg(feature = "git-worktree")]
    {
        cmds.push(("/worktree", "create and switch to a new git worktree"));
        cmds.push(("/wt-exit", "leave a worktree and return to its base"));
        cmds.push((
            "/wt-merge",
            "merge a worktree's work back to its base branch",
        ));
    }
    #[cfg(feature = "mcp")]
    cmds.push(("/mcp", "list connected MCP servers and their tools"));
    // `/loop` is always dispatched (its handler prints a "requires the
    // 'loop' feature" message when built without it), so it's a KNOWN
    // command regardless of the feature — keep the canonical list in sync
    // (dirge-3p8j). The gated entry made it un-completable / "unknown" in
    // no-loop builds even though the arm handled it.
    cmds.push(("/loop", "start, stop, or show a background prompt loop"));
    #[cfg(feature = "dap")]
    cmds.push((
        "/debug",
        "control the DAP debugger (launch, step, breakpoints)",
    ));
    #[cfg(feature = "dap")]
    cmds.push((
        "/dap-repl",
        "evaluate expressions in the paused debug session",
    ));
    cmds.sort_unstable_by_key(|(name, _)| *name);
    cmds
}

/// Names of the built-in slash commands. Order is inherited from
/// `slash_commands()` (sorted by name) — do not re-sort here; tab
/// completion cycles previews in that stable order.
pub fn slash_command_names() -> Vec<&'static str> {
    slash_commands().into_iter().map(|(name, _)| name).collect()
}

/// `(name, description)` pairs for the `/help` render.
pub fn slash_command_descriptions() -> Vec<(&'static str, &'static str)> {
    slash_commands()
}

/// Returns true if `name` (with leading `/`) is a built-in slash
/// command. Used by `handle_slash`'s default arm to distinguish
/// "command name we should have dispatched but didn't" (internal
/// error) from "command name we don't know about" (plugin fallback /
/// unknown).
pub fn is_known_slash_command(name: &str) -> bool {
    slash_command_names().contains(&name)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "slash-completion")]
    use super::completion::all_commands;
    use super::*;
    use crate::session::compact::align_cut_to_user_boundary;
    use crate::session::{Session, SessionMessage};

    #[test]
    fn compress_instructions_joins_trailing_text() {
        // `/compress focus text` -> DeferCompress { instructions: Some("focus text") }
        let parts = ["/compress", "focus", "text"];
        assert_eq!(compress_instructions(&parts).as_deref(), Some("focus text"));
    }

    #[test]
    fn compress_instructions_is_none_when_bare() {
        // Bare `/compress` -> DeferCompress { instructions: None }
        let parts = ["/compress"];
        assert_eq!(compress_instructions(&parts), None);
    }

    #[test]
    fn preemptive_due_fires_in_the_85_to_100_band() {
        // dirge-rz4i: the user's case — 106.7k used of a 114.7k usable budget
        // (~93%). 85% of 114_700 = 97_495; total already exceeds it, so a
        // proactive compaction is due even with a tiny incoming prompt. The
        // OLD within-limits gate (total <= max) would have no-op'd here.
        let max_tokens = 114_700;
        assert!(preemptive_compaction_due(106_700, 50, max_tokens));
        // And it must be < the hard limit, proving this is the band that
        // regressed (preemptive fired but prepare refused).
        assert!(106_700 <= max_tokens);
    }

    #[test]
    fn preemptive_due_incoming_prompt_pushes_over() {
        // Under 85% on its own (90k of 114.7k usable ≈ 78%), but a large
        // incoming prompt crosses the threshold — the case only the trigger
        // can see, which is why it must drive the (forced) compaction.
        let max_tokens = 114_700;
        assert!(!preemptive_compaction_due(90_000, 0, max_tokens));
        assert!(preemptive_compaction_due(90_000, 10_000, max_tokens));
    }

    #[test]
    fn preemptive_due_below_threshold_and_empty_session() {
        let max_tokens = 100_000; // 85% = 85_000
        assert!(!preemptive_compaction_due(80_000, 1_000, max_tokens));
        // Fresh session (total == 0) never triggers, even with a huge prompt.
        assert!(!preemptive_compaction_due(0, 1_000_000, max_tokens));
    }

    #[test]
    fn split_command_parts_collapses_extra_whitespace() {
        // Single and multiple spaces parse identically.
        assert_eq!(
            split_command_parts("/sessions delete abc123").as_slice(),
            ["/sessions", "delete", "abc123"]
        );
        assert_eq!(
            split_command_parts("/sessions  delete   abc123").as_slice(),
            ["/sessions", "delete", "abc123"]
        );
        // Leading/trailing whitespace is ignored.
        assert_eq!(
            split_command_parts("  /toggle thinking on  ").as_slice(),
            ["/toggle", "thinking", "on"]
        );
        // The remainder is still recoverable for multi-word args.
        let p = split_command_parts("/compress  keep   the auth flow");
        assert_eq!(p[0], "/compress");
        assert_eq!(p[1..].join(" "), "keep the auth flow");
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn ghost_suffix_completes_a_unique_prefix() {
        // `/display` is the only command with this prefix.
        assert_eq!(ghost_suffix("/disp").as_deref(), Some("lay"));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn ghost_suffix_returns_none_when_not_completable() {
        assert_eq!(ghost_suffix("/"), None); // too short / ambiguous
        assert_eq!(ghost_suffix("not-a-command"), None); // no leading slash
        assert_eq!(ghost_suffix("/display extra"), None); // past the command token
        assert_eq!(ghost_suffix("/zzzznope"), None); // no match
    }

    fn msg(role: MessageRole, content: &str) -> SessionMessage {
        // Re-use Session::add_message to get a real msg with id/timestamp.
        let mut s = Session::new("p", "m", 0);
        s.add_message(role, content);
        s.messages.pop().unwrap()
    }

    /// F3: when the reverse-scan lands on an Assistant message, the
    /// helper advances to the next User so the kept tail begins
    /// with a User message (provider-required role sequence after
    /// the System summary).
    #[test]
    fn align_cut_advances_past_assistant_to_next_user() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::User, "u1"),
            msg(MessageRole::Assistant, "a1"), // reverse-scan landed here (cut_idx=3)
            msg(MessageRole::User, "u2"),
            msg(MessageRole::Assistant, "a2"),
        ];
        // Initial cut at idx=3 (Assistant). Should advance to 4 (User).
        assert_eq!(align_cut_to_user_boundary(&messages, 3), 4);
    }

    /// User-boundary cut is unchanged.
    #[test]
    fn align_cut_idempotent_when_already_on_user() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::User, "u1"),
        ];
        assert_eq!(align_cut_to_user_boundary(&messages, 2), 2);
        assert_eq!(align_cut_to_user_boundary(&messages, 0), 0);
    }

    /// Cut past end of array stays past end.
    #[test]
    fn align_cut_past_end_clamps() {
        let messages = vec![msg(MessageRole::User, "u0")];
        assert_eq!(align_cut_to_user_boundary(&messages, 1), 1);
        assert_eq!(align_cut_to_user_boundary(&messages, 5), 5);
    }

    /// No user in the tail (e.g. only system+assistant remain after
    /// the cut). Helper returns `messages.len()`, which is the
    /// "nothing to compress (no clean boundary)" case.
    #[test]
    fn align_cut_returns_end_when_no_user_in_tail() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::System, "system note"),
            msg(MessageRole::Assistant, "a1"),
        ];
        // cut_idx=2 points at System; no User follows.
        assert_eq!(align_cut_to_user_boundary(&messages, 2), messages.len());
    }

    /// A cut that lands on a System message (e.g. a prior summary)
    /// also advances forward to the next User.
    #[test]
    fn align_cut_skips_system_to_user() {
        let messages = vec![
            msg(MessageRole::System, "prior summary"),
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
        ];
        assert_eq!(align_cut_to_user_boundary(&messages, 0), 1);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn no_completion_without_slash() {
        assert!(try_complete("hello", 5).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn empty_buffer_returns_none() {
        assert!(try_complete("", 0).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn complete_partial_command() {
        let r = try_complete("/mod", 4).unwrap();
        assert_eq!(r.new_buffer, "/mode");
        assert_eq!(r.new_cursor, 5);
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn cycles_between_partial_matches() {
        let r = try_complete("/mod", 4).unwrap();
        assert!(r.new_buffer.starts_with("/mod"));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn cycles_beyond_single_match() {
        let r1 = try_complete("/", 1).unwrap();
        let r2 = try_complete(&r1.new_buffer, r1.new_cursor).unwrap();
        assert_ne!(r1.new_buffer, r2.new_buffer);
        assert!(!r2.new_buffer.is_empty());
        assert!(r2.new_buffer.starts_with('/'));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn cycles_from_full_command() {
        let r = try_complete("/btw", 4).unwrap();
        assert_ne!(r.new_buffer, "/btw");
        assert!(r.new_buffer.starts_with('/'));
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn cycles_through_all_commands() {
        let mut seen = std::collections::HashSet::new();
        let mut buf = "/".to_string();
        let mut cur = 1;
        for _ in 0..100 {
            let result = try_complete(&buf, cur);
            if result.is_none() {
                break;
            }
            let r = result.unwrap();
            buf = r.new_buffer;
            cur = r.new_cursor;
            seen.insert(buf.clone());
        }
        let all = all_commands();
        assert_eq!(
            seen.len(),
            all.len(),
            "should cycle through all builtin commands"
        );
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn unknown_command_returns_none() {
        assert!(try_complete("/nonexistent", 12).is_none());
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn commands_are_sorted() {
        let cmds = all_commands();
        for pair in cmds.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "{} should be before {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[cfg(feature = "slash-completion")]
    #[test]
    fn preview_includes_upcoming_commands() {
        let r = try_complete("/", 1).unwrap();
        let all = &r.all_commands;
        let cur = r.current_index;
        let upcoming = &all[(cur + 1)..];
        assert!(
            !upcoming.is_empty(),
            "should have commands after the current one"
        );
    }

    // ============================================================
    // Code-review B1 fix: cursor mid-word produces well-formed
    // buffer
    // ============================================================

    /// Regression: cursor sitting at byte 2 of `/mod` previously
    /// produced `/mcpod` (replacement appended to the tail AFTER
    /// the cursor, with `od` left over from the original word).
    /// The fix anchors replacement to the whole-word boundary
    /// instead, so cursor position inside the command name doesn't
    /// corrupt the buffer — the result is exactly one of the
    /// matching commands, no Frankenstein.
    #[cfg(feature = "slash-completion")]
    #[test]
    fn complete_with_cursor_mid_word_produces_clean_buffer() {
        // /mod, cursor at the `o` (byte 2). The new buffer must be
        // exactly a candidate command — not the candidate +
        // residual `od` from the source.
        let r = try_complete("/mod", 2).unwrap();
        let candidates = all_commands()
            .into_iter()
            .filter(|c| c.starts_with("/mod"))
            .collect::<Vec<_>>();
        assert!(
            candidates.contains(&r.new_buffer),
            "{:?} must be one of the /mod* commands {:?} — no Frankenstein concatenation",
            r.new_buffer,
            candidates,
        );
        assert_eq!(
            r.new_cursor,
            r.new_buffer.len(),
            "cursor should land at end of replacement",
        );
    }

    /// Cursor at byte 0 (Home before Tab) used to produce
    /// `/allow/mod` because the entire buffer was concatenated as
    /// the tail. Verify it now produces a clean replacement —
    /// exactly one of the matching commands, no residual `/mod`.
    #[cfg(feature = "slash-completion")]
    #[test]
    fn complete_with_cursor_at_start_produces_clean_buffer() {
        let r = try_complete("/mod", 0).unwrap();
        let candidates = all_commands()
            .into_iter()
            .filter(|c| c.starts_with("/mod"))
            .collect::<Vec<_>>();
        assert!(
            candidates.contains(&r.new_buffer),
            "{:?} must be a /mod* command (clean replacement, no /mod residual): candidates {:?}",
            r.new_buffer,
            candidates,
        );
    }

    /// Tab on a command with trailing args (e.g. `/mode standard`
    /// with cursor after `/mode`) preserves the args tail.
    #[cfg(feature = "slash-completion")]
    #[test]
    fn complete_preserves_trailing_args() {
        let r = try_complete("/mod standard", 4).unwrap();
        assert!(
            r.new_buffer.ends_with(" standard"),
            "args after the command should be preserved: {:?}",
            r.new_buffer
        );
    }

    /// Cursor past the first whitespace means the user is typing
    /// args, not a command name — no completion should fire.
    #[cfg(feature = "slash-completion")]
    #[test]
    fn no_completion_when_cursor_in_args() {
        // Cursor inside args of a command WITHOUT subcommand entries
        // (e.g. /btw freeform text) — no completion should fire.
        let buf = "/btw some arbitrary text";
        let cursor = buf.len();
        assert!(try_complete(buf, cursor).is_none());
    }

    // ============================================================
    // Code-review B2 fix: canonical command list + drift guard
    // ============================================================

    /// `is_known_slash_command` must agree with `slash_command_names`
    /// since the helper just iterates the list. Catches a future
    /// refactor that decouples them (e.g. someone introducing a
    /// second hardcoded match).
    #[test]
    fn is_known_slash_command_agrees_with_canonical_list() {
        for name in slash_command_names() {
            assert!(
                is_known_slash_command(name),
                "{name} is in slash_command_names() but is_known_slash_command rejects it",
            );
        }
        // Spot-check negatives.
        assert!(!is_known_slash_command("/not-a-real-command"));
        assert!(!is_known_slash_command(""));
        assert!(!is_known_slash_command("/"));
    }

    /// The canonical list is sorted (tab completion preview relies
    /// on stable ordering for the cycle direction).
    #[test]
    fn slash_command_names_is_sorted() {
        let cmds = slash_command_names();
        for pair in cmds.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
    }

    /// Pin that the canonical list and `handle_slash`'s actual
    /// match arms agree on the always-on commands. If a name in
    /// the list above is missing from the dispatch tree the user
    /// would hit the new "internal error" arm at runtime; this
    /// duplicates the check in plain test code so a future
    /// maintainer sees the gap before users do.
    ///
    /// This DOES NOT enforce the reverse direction (arm present in
    /// `handle_slash` but missing from `slash_command_names`) — that
    /// would require parsing source. We accept it as the lesser
    /// drift: the only user-visible cost is that the missing
    /// command isn't tab-completable.
    #[test]
    fn always_on_commands_appear_in_canonical_list() {
        // Subset that is unconditionally compiled (no cfg) and
        // therefore must always be present. Cross-checked by hand
        // against the `match parts[0]` arms in `handle_slash`.
        const ALWAYS_ON: &[&str] = &[
            "/agent",
            "/agents",
            "/allow",
            "/btw",
            "/cd",
            "/clear",
            "/clone",
            "/compact",
            "/compress",
            "/display",
            "/fork",
            "/graph",
            "/help",
            "/kill",
            "/memory",
            "/mode",
            "/model",
            "/panel",
            "/plan",
            "/plugins",
            "/prompt",
            "/quit",
            "/reasoning",
            "/regen-prompts",
            "/retry",
            "/sessions",
            "/tasks",
            "/toggle",
            "/tree",
            "/undo",
            "/why",
        ];
        let list = slash_command_names();
        for name in ALWAYS_ON {
            assert!(
                list.contains(name),
                "{name} must appear in slash_command_names() — it's an always-on dispatch arm in handle_slash",
            );
        }
    }

    /// `slash_commands()` is the single source of truth, so a name
    /// with no description (or vice versa) can't happen — the only
    /// residual copy-paste hazard is the same name twice, which would
    /// silently drop one entry from `/help` and tab completion.
    #[test]
    fn slash_commands_have_no_duplicate_names() {
        let cmds = slash_commands();
        let total = cmds.len();
        let mut names: Vec<&str> = cmds.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "duplicate command name in slash_commands()",
        );
    }
}
