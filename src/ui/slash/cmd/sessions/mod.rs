//! /sessions command dispatch.

pub(crate) mod delete;
pub(crate) mod list;
pub(crate) mod switch;

use crate::session::Session;
use crate::ui::slash::{SlashCtx, c_agent, c_result};

/// Parsed `/sessions` request. Split from the handler so the verb routing
/// is unit-testable without a live `SlashCtx`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionAction<'a> {
    List,
    /// Show the live session's FULL id (+ resume hint). The footer only has
    /// room for a compact glance id (`text::session_glance_id`).
    Current,
    Switch(&'a str),
    Delete(&'a str),
    /// A verb that needs an id but didn't get one; carries the usage hint.
    Usage(&'static str),
}

/// Route `/sessions [verb] [arg]`. Verbs (`list`/`switch`/`delete`) are
/// matched first, so a bare positional is only ever a session id.
pub(crate) fn parse_sessions_command<'a>(parts: &[&'a str]) -> SessionAction<'a> {
    let verb = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());
    let arg = parts.get(2).map(|s| s.trim()).filter(|s| !s.is_empty());
    match verb {
        None | Some("list") => SessionAction::List,
        Some("current") | Some("id") => SessionAction::Current,
        Some("delete") => match arg {
            Some(id) => SessionAction::Delete(id),
            None => SessionAction::Usage("delete <id>"),
        },
        Some("switch") => match arg {
            Some(id) => SessionAction::Switch(id),
            None => SessionAction::Usage("switch <id>"),
        },
        // Bare positional: treat as a session id to switch to.
        Some(id) => SessionAction::Switch(id),
    }
}

/// Smallest id-prefix length (floored at 8) at which every id in `ids` is
/// unique, capped at the longest id. The list and ambiguity views truncate
/// ids to this so the handles shown are actually distinguishable — and thus
/// retypeable. `compacted-<uuid>` sessions share the first 10 chars, so the
/// old fixed 8-char head ("compacte") was identical for every one and
/// "be more specific" was impossible (dirge).
pub(crate) fn distinct_id_len(ids: &[&str]) -> usize {
    // Floor: at least 8, but never cut inside an id's leading marker — the
    // run before its first `-`. A `compacted-<uuid>` session must read as
    // "compacted…", not the confusing mid-word "compacte" (dirge). Plain
    // UUID ids have their first `-` at index 8, so they stay at 8.
    let floor = ids
        .iter()
        .map(|s| s.find('-').unwrap_or(s.len()).min(s.len()))
        .max()
        .unwrap_or(8)
        .max(8);
    let max = ids.iter().map(|s| s.len()).max().unwrap_or(floor);
    for n in floor..=max {
        let mut seen = std::collections::HashSet::new();
        if ids.iter().all(|s| seen.insert(crate::text::head(s, n))) {
            return n;
        }
    }
    max.max(floor)
}

#[cfg(test)]
mod distinct_id_len_tests {
    use super::distinct_id_len;

    #[test]
    fn floors_at_8_for_short_distinct_ids() {
        // Plain UUID heads differ within 8 chars → stays at the floor.
        let ids = ["550e8400-x", "a1b2c3d4-y", "deadbeef-z"];
        assert_eq!(distinct_id_len(&ids), 8);
    }

    #[test]
    fn grows_past_a_shared_prefix() {
        // `compacted-` is 10 chars; the distinguishing byte is at index 10,
        // so we need length 11 to tell them apart.
        let ids = ["compacted-aaaa", "compacted-bbbb", "compacted-cccc"];
        let n = distinct_id_len(&ids);
        assert_eq!(n, 11, "must extend past the shared `compacted-` prefix");
        // And the resulting handles are all distinct.
        let heads: Vec<&str> = ids.iter().map(|s| crate::text::head(s, n)).collect();
        assert_eq!(heads, ["compacted-a", "compacted-b", "compacted-c"]);
    }

    #[test]
    fn single_compacted_id_reads_as_compacted_not_compacte() {
        // The floor never cuts the leading marker, so a lone compacted
        // session shows "compacted", not the mid-word "compacte" (dirge).
        let n = distinct_id_len(&["compacted-whatever"]);
        assert_eq!(n, 9);
        assert_eq!(crate::text::head("compacted-whatever", n), "compacted");
    }

    #[test]
    fn plain_uuid_ids_stay_at_floor_8() {
        // A first `-` at index 8 (UUID heads) keeps the compact 8-char view.
        let ids = ["550e8400-aaa", "a1b2c3d4-bbb"];
        assert_eq!(distinct_id_len(&ids), 8);
    }
}

/// Dispatch `/sessions`. Verbs are first-class so a bare positional is only
/// ever a session id — previously `parts[1]` did double duty as both the
/// `delete` sentinel and a session id, so no session could be addressed as
/// "delete" (dirge):
///
///   /sessions              → list
///   /sessions list         → list
///   /sessions <id>         → switch (shortcut)
///   /sessions switch <id>  → switch (explicit)
///   /sessions delete <id>  → delete
pub(crate) async fn cmd_sessions(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    match parse_sessions_command(parts) {
        SessionAction::List => list::cmd_sessions_list(ctx).await,
        SessionAction::Current => current(ctx),
        SessionAction::Switch(id) => switch::cmd_sessions_switch(ctx, id).await,
        SessionAction::Delete(id) => delete::cmd_sessions_delete(ctx, id).await,
        SessionAction::Usage(what) => usage(ctx, what),
    }
}

/// `/sessions current` — print the live session's FULL id with a copy-pasteable
/// resume command. The status footer only shows a compact glance id, so this is
/// the way to read the whole thing (e.g. to relaunch with `--session`).
fn current(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let id = ctx.session.id.to_string();
    ctx.renderer
        .write_line(&format!("session: {id}"), c_agent())?;
    ctx.renderer.write_line(
        &format!(
            "  {} · {} msgs",
            ctx.session.model,
            ctx.session.messages.len()
        ),
        c_result(),
    )?;
    ctx.renderer
        .write_line(&format!("  resume: dirge --session {id}"), c_result())?;
    Ok(())
}

fn usage(ctx: &mut SlashCtx<'_>, what: &str) -> anyhow::Result<()> {
    ctx.renderer
        .write_line(&format!("usage: /sessions {}", what), c_agent())?;
    Ok(())
}

/// Tear down the current session and make `next` the live one: cancel
/// background work, fire the session-end/switch review hooks, restore the
/// new session's prompt layer + panels, and rebuild the agent so it targets
/// the new id. Shared by `/sessions switch <id>` (a session loaded from
/// disk) and the delete-current path (a fresh session), so booting between
/// sessions is identical no matter the trigger.
pub(crate) async fn swap_to_session(ctx: &mut SlashCtx<'_>, next: Session) -> anyhow::Result<()> {
    if let Some(store) = ctx.bg_store.as_ref() {
        store.cancel_all();
    }
    crate::agent::review::maybe_fire_session_end(ctx.agent, ctx.session);
    let old_id = ctx.session.id.to_string();
    *ctx.session = next;
    crate::agent::review::maybe_fire_session_switch(ctx.agent, &ctx.session.id, &old_id, false);

    let restored = ctx.session.current_prompt_name.clone();
    if let Some(name) = restored.as_deref()
        && let Some(p) = ctx.context.prompts.get(name).cloned()
    {
        ctx.context.set_prompt_layer(
            Some(name.to_string()),
            Some(p.body.clone()),
            p.deny_tools.clone(),
        );
        crate::permission::apply_prompt_deny(
            ctx.permission,
            &ctx.context.current_prompt_deny_tools,
        );
    }

    // Rebuild the TODOS / MODIFIED panels from the switched-to session's
    // history; these globals don't carry across a swap.
    crate::session::rehydrate::restore_panels(ctx.session);

    let model = ctx.client.completion_model(ctx.session.model.to_string());
    *ctx.agent = crate::provider::build_agent(
        model,
        ctx.cli,
        ctx.cfg,
        ctx.context,
        ctx.permission.clone(),
        ctx.ask_tx.clone(),
        ctx.question_tx.clone(),
        ctx.plan_tx.clone(),
        ctx.bg_store.clone(),
        #[cfg(feature = "lsp")]
        ctx.lsp_manager.cloned(),
        ctx.sandbox.clone(),
        #[cfg(feature = "mcp")]
        ctx.mcp_manager,
        #[cfg(feature = "semantic")]
        ctx.semantic_manager,
        Some(ctx.session.id.to_string()),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SessionAction, parse_sessions_command};

    fn route(cmd: &str) -> SessionAction<'_> {
        // Mirror the real dispatch: parts[0] is the command itself.
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        parse_sessions_command(&parts)
    }

    #[test]
    fn bare_and_explicit_list() {
        assert_eq!(route("/sessions"), SessionAction::List);
        assert_eq!(route("/sessions list"), SessionAction::List);
    }

    #[test]
    fn current_verb_routes_to_current() {
        assert_eq!(route("/sessions current"), SessionAction::Current);
        assert_eq!(route("/sessions id"), SessionAction::Current);
    }

    #[test]
    fn bare_positional_switches() {
        assert_eq!(route("/sessions abc123"), SessionAction::Switch("abc123"));
        assert_eq!(
            route("/sessions switch abc123"),
            SessionAction::Switch("abc123")
        );
    }

    #[test]
    fn delete_needs_an_id() {
        assert_eq!(
            route("/sessions delete abc123"),
            SessionAction::Delete("abc123")
        );
        // A verb without its id is a usage hint, NOT a switch to a session
        // named "delete" — the old dispatch fell through to switch here.
        assert_eq!(
            route("/sessions delete"),
            SessionAction::Usage("delete <id>")
        );
        assert_eq!(
            route("/sessions switch"),
            SessionAction::Usage("switch <id>")
        );
    }

    /// The verb is matched before the bare-id path, so a session can't be
    /// addressed as "delete"/"switch" via the shortcut — but that's the
    /// point: ids no longer collide with verbs, and the explicit form makes
    /// intent unambiguous (dirge).
    #[test]
    fn verbs_take_precedence_over_bare_id() {
        assert_eq!(route("/sessions delete x"), SessionAction::Delete("x"));
        assert_eq!(route("/sessions switch x"), SessionAction::Switch("x"));
    }
}
