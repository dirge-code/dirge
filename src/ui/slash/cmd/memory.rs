//! /memory handler — show what is remembered, edit it, review queued
//! writes, reload the snapshot.

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_memory(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("").trim();
    match sub {
        "reload" => {
            let provider = match ctx.agent.memory_provider() {
                Some(p) => p,
                None => {
                    ctx.renderer
                        .write_line("no memory provider loaded", c_error())?;
                    return Ok(());
                }
            };
            match provider.refresh_snapshot() {
                Ok(()) => {
                    ctx.renderer
                        .write_line("memory snapshot refreshed", c_agent())?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("refresh failed: {e}"), c_error())?;
                }
            }
        }
        #[cfg(unix)]
        "edit" => {
            cmd_memory_edit(ctx).await?;
        }
        #[cfg(unix)]
        "review" => {
            cmd_memory_review(ctx).await?;
        }
        "help" => {
            ctx.renderer
                .write_line("/memory          — list what is remembered", c_agent())?;
            #[cfg(unix)]
            ctx.renderer
                .write_line("/memory edit     — open the store in $EDITOR", c_agent())?;
            #[cfg(unix)]
            ctx.renderer.write_line(
                "/memory review   — confirm, edit or add to memories awaiting review",
                c_agent(),
            )?;
            ctx.renderer.write_line(
                "/memory reload   — refresh the frozen snapshot so recent writes appear in the prompt",
                c_agent(),
            )?;
        }
        // Bare `/memory` shows the store. It used to print its own help,
        // which meant there was no way at all to see what dirge had
        // remembered about you without asking the agent to go and look.
        "" => {
            cmd_memory_list(ctx)?;
        }
        other => {
            ctx.renderer
                .write_line(&format!("unknown /memory sub-command: {other}"), c_error())?;
        }
    }
    Ok(())
}

/// Open the project's memory store, or explain why not.
///
/// Opened by path rather than through the agent's `MemoryProvider`: the
/// provider may be a hybrid wrapper, and this must work in a session whose
/// memory tool failed to load — that is exactly when you want to look.
fn open_store() -> Result<crate::extras::memory_db::SqliteMemoryStore, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    crate::extras::memory_db::SqliteMemoryStore::load(&paths)
}

fn cmd_memory_list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot open memory store: {e}"), c_error())?;
            return Ok(());
        }
    };
    let entries = match store.list_all() {
        Ok(e) => e,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot read memories: {e}"), c_error())?;
            return Ok(());
        }
    };
    for line in crate::ui::memory_document::summarize(&entries) {
        ctx.renderer.write_line(&line, c_agent())?;
    }
    #[cfg(unix)]
    if !entries.is_empty() {
        ctx.renderer
            .write_line("(/memory edit to change them)", c_agent())?;
    }
    Ok(())
}

/// Open the whole store in `$EDITOR` and apply what comes back.
#[cfg(unix)]
async fn cmd_memory_edit(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::ui::memory_document;

    let store = match open_store() {
        Ok(s) => s,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot open memory store: {e}"), c_error())?;
            return Ok(());
        }
    };
    let stored = match store.list_all() {
        Ok(e) => e,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot read memories: {e}"), c_error())?;
            return Ok(());
        }
    };
    if stored.is_empty() {
        ctx.renderer
            .write_line("no memories stored — nothing to edit", c_agent())?;
        return Ok(());
    }

    let doc = memory_document::render(&stored);
    let drained_stdin = match crate::ui::terminal::suspend_tui_for_subprocess(ctx.user_tx) {
        Some(d) => d,
        None => {
            ctx.renderer
                .write_line("no /dev/tty available — cannot open an editor", c_error())?;
            return Ok(());
        }
    };
    let edited = crate::ui::external_editor::edit_text(&doc, "memory");
    crate::ui::terminal::resume_tui_after_subprocess(ctx.renderer, ctx.user_tx);
    drop(drained_stdin);

    // Aborting the editor (`:cq`) must leave the store untouched, which is
    // why `edit_text` distinguishes a failed edit from an empty document —
    // an empty document legitimately means "forget everything".
    let Some(edited) = edited else {
        ctx.renderer
            .write_line("edit aborted — nothing changed", c_agent())?;
        return Ok(());
    };

    let plan = match memory_document::parse(&edited, &stored) {
        Ok(p) => p,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("could not parse the edit: {e}"), c_error())?;
            ctx.renderer
                .write_line("nothing changed — run /memory edit again", c_agent())?;
            return Ok(());
        }
    };

    let report = memory_document::apply(&store, &plan);
    ctx.renderer
        .write_line(&format!("memory: {}", report.summary()), c_agent())?;
    for failure in &report.failures {
        ctx.renderer
            .write_line(&format!("  {failure}"), c_error())?;
    }

    // The prompt snapshot is frozen at build time, so without this the agent
    // keeps using the memories as they were before the edit.
    if (report.updated > 0 || report.added > 0 || report.removed > 0)
        && let Some(provider) = ctx.agent.memory_provider()
        && let Err(e) = provider.refresh_snapshot()
    {
        ctx.renderer
            .write_line(&format!("snapshot refresh failed: {e}"), c_error())?;
    }
    Ok(())
}

/// Open the queued memory writes in `$EDITOR` and apply the result.
///
/// The stores are opened here rather than taken from the agent's
/// `MemoryProvider`: the queue is a builtin-store concept and the provider
/// may be a hybrid wrapper around it. Opening by path also means review
/// still works in a session whose memory tool failed to load.
#[cfg(unix)]
async fn cmd_memory_review(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::extras::memory_db::SqliteMemoryStore;
    use crate::ui::memory_review;

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    let project = match SqliteMemoryStore::load(&paths) {
        Ok(s) => s,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("cannot open memory store: {e}"), c_error())?;
            return Ok(());
        }
    };
    let global = SqliteMemoryStore::load_global().ok();

    let queued = memory_review::collect(&project, global.as_ref());
    if queued.is_empty() {
        ctx.renderer
            .write_line("no memories awaiting review", c_agent())?;
        return Ok(());
    }
    let doc = memory_review::render(&queued);

    let drained_stdin = match crate::ui::terminal::suspend_tui_for_subprocess(ctx.user_tx) {
        Some(d) => d,
        None => {
            ctx.renderer
                .write_line("no /dev/tty available — cannot open an editor", c_error())?;
            return Ok(());
        }
    };
    let edited = crate::ui::external_editor::edit_text(&doc, "memory-review");
    crate::ui::terminal::resume_tui_after_subprocess(ctx.renderer, ctx.user_tx);
    drop(drained_stdin);

    // An aborted edit leaves the queue exactly as it was, so review can be
    // retried. This is why `edit_text` returns None rather than an empty
    // document on abort — confusing the two would reject everything.
    let Some(edited) = edited else {
        ctx.renderer
            .write_line("review aborted — nothing changed", c_agent())?;
        return Ok(());
    };

    let reviewed = match memory_review::parse(&edited) {
        Ok(r) => r,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("could not parse the review: {e}"), c_error())?;
            ctx.renderer
                .write_line("nothing changed — run /memory review again", c_agent())?;
            return Ok(());
        }
    };

    let report = memory_review::apply(&project, global.as_ref(), &reviewed, queued.len());
    ctx.renderer
        .write_line(&format!("memory review: {}", report.summary()), c_agent())?;
    for failure in &report.failures {
        ctx.renderer
            .write_line(&format!("  {failure}"), c_error())?;
    }

    // The prompt snapshot is frozen at build time, so accepted entries would
    // otherwise stay invisible to the agent until a restart.
    if report.stored > 0
        && let Some(provider) = ctx.agent.memory_provider()
        && let Err(e) = provider.refresh_snapshot()
    {
        ctx.renderer
            .write_line(&format!("snapshot refresh failed: {e}"), c_error())?;
    }
    Ok(())
}
