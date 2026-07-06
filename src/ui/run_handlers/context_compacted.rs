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

/// Prepend the conversation's verbatim original request to a fold
/// summary so it anchors resumed context. The summary's own `## Goal`
/// can drift as the body is re-summarized fold after fold; the original
/// ask rides along verbatim through the existing summary-injection path
/// (`convert_history`), symmetric for headless and TUI resume without
/// touching that hot path's many callers. Empty intent or summary
/// returns the summary unchanged.
fn anchor_summary_with_intent(intent: &str, summary: &str) -> String {
    if intent.is_empty() || summary.is_empty() {
        return summary.to_string();
    }
    format!("Original request: {intent}\n\n{summary}")
}

/// Persist an INCREMENTAL checkpoint: a background summary fired at a usage
/// threshold (MiMo cadence) without folding. Unlike the fold handler this
/// writes ONLY the durable checkpoint — no session rotation, no message
/// drop, no save — keyed by the conversation's current origin, with the
/// write-once intent recovered from any prior checkpoint (else the live
/// first prompt). Synchronous but light (one small SQLite upsert).
pub(crate) fn handle_checkpoint_refresh(session: &crate::session::Session, summary: &str) {
    if summary.is_empty() {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    if let Ok(db) = crate::extras::session_db::SessionDb::open(&paths.session_db_path()) {
        let origin = session.effective_origin().to_string();
        let mut intent = session.first_user_prompt().unwrap_or("").to_string();
        if let Some(cp) = db.get_checkpoint(&origin).ok().flatten()
            && !cp.intent.is_empty()
        {
            intent = cp.intent;
        }
        db.checkpoint_after_fold(&origin, &intent, summary);
    }
}

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
    // The conversation's stable origin and its durable verbatim intent.
    // Prefer the checkpoint's write-once original — it survives later
    // folds where the first user prompt has already been folded out of
    // the live messages — falling back to the current first user prompt
    // on the first fold.
    let origin = ctx.session.effective_origin().to_string();
    let mut intent = ctx.session.first_user_prompt().unwrap_or("").to_string();
    if let Ok(db) = crate::extras::session_db::SessionDb::open(&paths.session_db_path()) {
        let old_sid = crate::text::db_session_id(ctx.session.id.as_str());
        // The rotated session's turns will persist under
        // `db_session_id(new_session_id)` (agent_io), so the fold row must
        // be inserted and linked under the SAME canonical key — otherwise
        // the turns and the parent link land in different rows and
        // `resolve_parent` can't walk the fold (dirge-g1ze).
        let new_sid = crate::text::db_session_id(new_session_id);
        let _ = db.end_session(&old_sid, "compression");
        let now = chrono::Utc::now().to_rfc3339();
        let _ = db.insert_session(
            &new_sid,
            "cli",
            &ctx.session.model,
            &ctx.session.provider,
            &now,
        );
        let _ = db.set_parent_session(&new_sid, &old_sid);
        // On a later fold the original ask is no longer in the live
        // messages; recover it from the write-once checkpoint slot.
        if let Some(cp) = db.get_checkpoint(&origin).ok().flatten()
            && !cp.intent.is_empty()
        {
            intent = cp.intent;
        }
        // Persist the durable session checkpoint (schema v10): the
        // structured fold summary plus the verbatim intent, keyed by the
        // stable origin id so a resume that resolves any chain member to
        // its origin recovers it. The slot is write-once, so passing the
        // recovered intent here keeps the original.
        db.checkpoint_after_fold(&origin, &intent, summary);
    }
    // SESS-2 follow-up #1: mutate the in-memory Session to match the
    // rotation and push a Compaction entry, then persist to disk. Without
    // this the on-disk session file kept the OLD id and the compaction was
    // lost on next resume. Mirrors Hermes conversation_compression.py
    // lines 380-397.
    let token_savings = tokens_before.saturating_sub(tokens_after);
    if !summary.is_empty() {
        // Anchor the verbatim original request at the head of the stored
        // summary so it reaches resumed context through the existing
        // summary-injection path.
        let anchored = anchor_summary_with_intent(&intent, summary);
        // dirge-4kgk: `first_kept_index` arrives in LOOP space (the agent
        // loop's `current_context.messages`, where every tool_result is its
        // own entry). `compress_reporting` drains `session.messages`, where
        // tool results are embedded in their assistant message — a much
        // shorter vec. Feeding the loop-space index straight in over-drains
        // and destroys the recent verbatim tail, so a crash-resume recovers
        // only the lossy summary. Recompute the cut in session space using
        // the same keep-recent policy as the `/compress` path.
        let _ = first_kept_index;
        let session_cut = crate::session::compact::compaction_cut_idx(
            ctx.session,
            ctx.cfg.resolve_keep_recent_tokens(),
        );
        ctx.session
            .compress_reporting(anchored, session_cut, token_savings);
    }
    // dirge-hs61: capture the outgoing id, do ALL the mutations (id
    // rotation + disk save), THEN fire the on_session_switch hook. Pre-fix
    // the hook fired in the middle: DB rotated, messages drained, but
    // on-disk JSON still had the old id — providers querying either store
    // saw inconsistent triple state.
    let parent_id = ctx.session.id.to_string();
    // Carry the conversation's stable origin forward onto the rotated
    // session BEFORE swapping `id`: on the first fold `effective_origin`
    // is still the original id (origin_id was None), which becomes the
    // chain's permanent origin; later folds re-stamp the same value.
    // This is the id resume/list/checkpoint all key on.
    let origin = ctx.session.effective_origin().to_string();
    ctx.session.origin_id = Some(compact_str::CompactString::new(origin));
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
    // Memory formation on compaction: a summary fold clears conversation
    // context, so capture the session's learnings into the durable memory
    // store before it's gone — the same background review/curate pass that
    // runs at session end, reused here. It's self-throttled and
    // single-runner (spawn_post_session's review slot + orchestrator
    // guard), so the more frequent folds under a capped budget don't pile
    // up. Skipped for a prune-only pass (empty summary = nothing folded).
    if !summary.is_empty() {
        // dirge-a62g: same deterministic ground-truth preamble as the
        // session-end path so a compaction fold's review gets it too. dirge-6rtt:
        // build the digest on-thread, defer its git subprocess to the task.
        let base = crate::agent::review::build_transcript(ctx.session);
        let digest = crate::agent::session_digest::SessionDigest::from_session(ctx.session);
        crate::agent::post_session::spawn_post_session(
            agent.clone(),
            paths,
            digest,
            base,
            ctx.cfg.memory_graduation.unwrap_or(true),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_prepends_verbatim_intent() {
        assert_eq!(
            anchor_summary_with_intent("fix the resume bug", "## Goal\nresume works"),
            "Original request: fix the resume bug\n\n## Goal\nresume works"
        );
    }

    #[test]
    fn anchor_is_a_noop_when_intent_or_summary_empty() {
        assert_eq!(anchor_summary_with_intent("", "the summary"), "the summary");
        assert_eq!(anchor_summary_with_intent("intent", ""), "");
    }
}
