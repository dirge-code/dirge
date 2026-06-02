//! `AgentEvent::ContextCompacted` handler extracted from `run_interactive`.
//!
//! A compaction pass rotated the session: persist the rotation to the
//! session DB (end old / insert new / link parent), mutate the in-memory
//! session to match (id + Compaction reporting entry) and save it to disk,
//! rebuild the agent so `SessionSearchTool` picks up the new id, then fire
//! the `on_session_switch` hook only once all three stores are consistent.
//!
//! The caller keeps the `tracing::debug!` line (it needs the
//! `compaction_kind` / `summary_model` fields the UI otherwise ignores).
//! Behavior is identical to the inline code; pure refactor (dirge-4y4l).

use crossterm::style::Color;

use crate::context::ContextFiles;
use crate::provider::AnyAgent;
use crate::ui::run_handlers::{AgentBuildDeps, RunCtx};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_context_compacted(
    ctx: &mut RunCtx<'_>,
    deps: &AgentBuildDeps<'_>,
    agent: &mut AnyAgent,
    context: &mut ContextFiles,
    new_session_id: &str,
    tokens_before: u64,
    tokens_after: u64,
    summary: &str,
    first_kept_index: usize,
) -> anyhow::Result<()> {
    // Rebind the bundled deps to locals so the body reads like the original.
    let client = deps.client;
    let permission = deps.permission;
    let ask_tx = deps.ask_tx;
    let question_tx = deps.question_tx;
    let plan_tx = deps.plan_tx;
    let bg_store = deps.bg_store;
    let sandbox = deps.sandbox;
    #[cfg(feature = "mcp")]
    let mcp_manager = deps.mcp_manager;
    #[cfg(feature = "semantic")]
    let semantic_manager = deps.semantic_manager;
    #[cfg(feature = "lsp")]
    let lsp_manager = deps.lsp_manager;

    // Persist session rotation to DB: end the old session with reason
    // "compression", insert the new session.
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    if let Ok(db) = crate::extras::session_db::SessionDb::open(&paths.session_db_path()) {
        let old_sid = format!("dirge-{}", crate::text::short_id(ctx.session.id.as_str()));
        let _ = db.end_session(&old_sid, "compression");
        let now = chrono::Utc::now().to_rfc3339();
        let _ = db.insert_session(
            new_session_id,
            "cli",
            &ctx.session.model,
            &ctx.session.provider,
            &now,
        );
        let _ = db.set_parent_session(new_session_id, &old_sid);
    }
    // SESS-2 follow-up #1: mutate the in-memory Session to match the
    // rotation and push a Compaction entry, then persist to disk. Without
    // this the on-disk session file kept the OLD id and the compaction was
    // lost on next resume. Mirrors Hermes conversation_compression.py
    // lines 380-397.
    let token_savings = tokens_before.saturating_sub(tokens_after);
    if !summary.is_empty() {
        ctx.session
            .compress_reporting(summary.to_string(), first_kept_index, token_savings);
    }
    // dirge-hs61: capture the outgoing id, do ALL the mutations (id
    // rotation + disk save), THEN fire the on_session_switch hook. Pre-fix
    // the hook fired in the middle: DB rotated, messages drained, but
    // on-disk JSON still had the old id — providers querying either store
    // saw inconsistent triple state.
    let parent_id = ctx.session.id.to_string();
    ctx.session.id = compact_str::CompactString::new(new_session_id);
    if let Err(e) = crate::session::storage::save_session(ctx.session) {
        tracing::warn!(
            target: "dirge::ui",
            error = %e,
            "could not persist rotated session after compaction",
        );
    }
    // dirge-g72y: rebuild the agent so SessionSearchTool picks up the new
    // id. Pre-fix the tool was constructed with the pre-rotation id and
    // silently excluded the wrong session — same bug class as the
    // dirge-502b regression that cmd_session.rs already handles by
    // rebuilding on swap.
    let model = client.completion_model(ctx.session.model.to_string());
    *agent = crate::provider::build_agent(
        model,
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
    // dirge-5gn6: fire on_session_switch only AFTER everything is
    // consistent: id rotated in memory, JSON saved to disk under new id,
    // agent rebuilt. `reset=false` — compaction continues the conversation.
    crate::agent::review::maybe_fire_session_switch(
        &*agent,
        new_session_id,
        &parent_id,
        /* reset = */ false,
    );
    ctx.renderer.write_line(
        &format!("  context compacted: {tokens_before} → {tokens_after} tokens (session {new_session_id})"),
        Color::DarkGrey,
    )?;
    Ok(())
}
