//! `/issues` handler — human view over the native issue board (the same
//! `IssueStore` the agent's `issue` tool writes and the harness injects at
//! turn start).
//!
//!   /issues              list the live board (open / in_progress / blocked)
//!   /issues list [status]   filter by a status
//!   /issues search <q>   substring search over title + body
//!   /issues <id>         show one issue's details (accepts 7 or #7)

use std::path::Path;

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::issue_db::{IssueStore, parse_issue_id};
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_issues(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let paths = ProjectPaths::new(Path::new(ctx.session.working_dir.as_str()));
    let store = match IssueStore::open(&paths) {
        Ok(s) => s,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("issues unavailable: {e}"), c_error())?;
            return Ok(());
        }
    };

    let sub = parts.get(1).copied().unwrap_or("").trim();
    match sub {
        "" | "list" => {
            let status = parts
                .get(2)
                .copied()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            list(ctx, &store, status)
        }
        "search" => {
            let query = parts.get(2..).map(|r| r.join(" ")).unwrap_or_default();
            search(ctx, &store, query.trim())
        }
        // Anything else is treated as an id (7 / #7 / iss-7).
        maybe_id => match parse_issue_id(maybe_id) {
            Some(id) => show(ctx, &store, id),
            None => {
                ctx.renderer.write_line(
                    "usage: /issues [list [status]] | /issues search <query> | /issues <id>",
                    c_error(),
                )?;
                Ok(())
            }
        },
    }
}

fn list(ctx: &mut SlashCtx<'_>, store: &IssueStore, status: Option<&str>) -> anyhow::Result<()> {
    let issues = match status {
        Some(s) => match store.list_by_status(s) {
            Ok(v) => v,
            Err(e) => {
                ctx.renderer.write_line(&e.to_string(), c_error())?;
                return Ok(());
            }
        },
        None => store.board(None).unwrap_or_default(),
    };
    if issues.is_empty() {
        ctx.renderer.write_line("no matching issues", c_agent())?;
        return Ok(());
    }
    let header = match status {
        Some(s) => format!("issues ({s}):"),
        None => format!("issue board ({} live):", issues.len()),
    };
    ctx.renderer.write_line(&header, c_agent())?;
    for i in &issues {
        ctx.renderer
            .write_line(&format!("  {}", i.one_line()), c_result())?;
    }
    Ok(())
}

fn search(ctx: &mut SlashCtx<'_>, store: &IssueStore, query: &str) -> anyhow::Result<()> {
    if query.is_empty() {
        ctx.renderer
            .write_line("usage: /issues search <query>", c_error())?;
        return Ok(());
    }
    let hits = store.search(query, 30).unwrap_or_default();
    if hits.is_empty() {
        ctx.renderer
            .write_line(&format!("no issues match '{query}'"), c_agent())?;
        return Ok(());
    }
    ctx.renderer
        .write_line(&format!("{} match(es):", hits.len()), c_agent())?;
    for i in &hits {
        ctx.renderer
            .write_line(&format!("  {}", i.one_line()), c_result())?;
    }
    Ok(())
}

fn show(ctx: &mut SlashCtx<'_>, store: &IssueStore, id: i64) -> anyhow::Result<()> {
    match store.get(id) {
        Ok(Some(i)) => {
            ctx.renderer.write_line(
                &format!("#{} [{}] ({})", i.id, i.status, i.priority),
                c_agent(),
            )?;
            ctx.renderer
                .write_line(&format!("  {}", i.title), c_result())?;
            if !i.body.trim().is_empty() {
                ctx.renderer
                    .write_line(&format!("  {}", i.body.replace('\n', "\n  ")), c_result())?;
            }
            let mut meta = format!("  created {} · updated {}", i.created_at, i.updated_at);
            if let Some(closed) = &i.closed_at {
                meta.push_str(&format!(" · closed {closed}"));
            }
            ctx.renderer.write_line(&meta, c_agent())?;
        }
        Ok(None) => {
            ctx.renderer
                .write_line(&format!("no issue #{id}"), c_error())?;
        }
        Err(e) => {
            ctx.renderer.write_line(&e.to_string(), c_error())?;
        }
    }
    Ok(())
}
