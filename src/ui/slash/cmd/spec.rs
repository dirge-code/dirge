//! /spec handler — read-only view of the spec-driven workflow tracker.
//!
//! The agent drives the workflow through the `spec` tool; this command lets
//! a human inspect it. `/spec` lists changes, `/spec <slug>` shows one in
//! detail, `/spec specs [capability]` reads the living specs.

use crate::ui::slash::{SlashCtx, c_result};
use crate::ui::theme;

/// Anchor the spec store on the session's working dir, matching /issues,
/// /graph, and /clear. dirge-s5oh: this used to build ProjectPaths from the
/// process cwd, so after `/sessions <id>` into a session saved elsewhere
/// (swap_to_session updates session.working_dir but never `set_current_dir`),
/// /spec read the cwd project's store while its siblings read the session
/// project's — two commands over the "same" session DB disagreeing.
fn resolve_project_paths(working_dir: &str) -> crate::extras::dirge_paths::ProjectPaths {
    crate::extras::dirge_paths::ProjectPaths::new(std::path::Path::new(working_dir))
}

pub(crate) async fn cmd_spec(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let paths = resolve_project_paths(ctx.session.working_dir.as_str());
    let store = match crate::extras::spec_db::SpecStore::open(&paths) {
        Ok(s) => s,
        Err(e) => {
            ctx.renderer
                .write_line(&format!("spec store unavailable: {e}"), c_result())?;
            return Ok(());
        }
    };

    // /spec specs [capability]
    if parts.get(1).copied() == Some("specs") {
        return show_specs(ctx, &store, parts.get(2).copied());
    }

    // /spec <slug>
    if let Some(slug) = parts.get(1).copied() {
        return show_change(ctx, &store, slug);
    }

    // /spec — list changes
    list_changes(ctx, &store)
}

fn list_changes(
    ctx: &mut SlashCtx<'_>,
    store: &crate::extras::spec_db::SpecStore,
) -> anyhow::Result<()> {
    let changes = store.list_changes(None).unwrap_or_default();
    if changes.is_empty() {
        ctx.renderer.write_line(
            "no spec changes yet — the agent creates them via the `spec` tool.",
            c_result(),
        )?;
        return Ok(());
    }
    ctx.renderer.write_line("spec changes:", c_result())?;
    for c in &changes {
        let (done, total) = store.task_progress(&c.slug).unwrap_or((0, 0));
        let title = if c.title.is_empty() {
            String::new()
        } else {
            format!(" — {}", c.title)
        };
        ctx.renderer.write_line(
            &format!(
                "  [{}] {}{}  ({done}/{total} tasks)",
                c.status, c.slug, title
            ),
            c_result(),
        )?;
    }
    ctx.renderer.write_line(
        "  /spec <slug> for detail · /spec specs for living specs",
        theme::dim(),
    )?;
    Ok(())
}

fn show_change(
    ctx: &mut SlashCtx<'_>,
    store: &crate::extras::spec_db::SpecStore,
    slug: &str,
) -> anyhow::Result<()> {
    let Some(change) = store.get_change(slug).ok().flatten() else {
        ctx.renderer
            .write_line(&format!("no change '{slug}'"), c_result())?;
        return Ok(());
    };
    let heading = if change.title.is_empty() {
        change.slug.clone()
    } else {
        format!("{} ({})", change.title, change.slug)
    };
    ctx.renderer
        .write_line(&format!("{heading} [{}]", change.status), c_result())?;
    if !change.why.is_empty() {
        ctx.renderer
            .write_line(&format!("  why:  {}", change.why), theme::dim())?;
    }
    if !change.what.is_empty() {
        ctx.renderer
            .write_line(&format!("  what: {}", change.what), theme::dim())?;
    }
    if !change.design.is_empty() {
        ctx.renderer
            .write_line(&format!("  design: {}", change.design), theme::dim())?;
    }

    let deltas = store.list_deltas(slug).unwrap_or_default();
    if !deltas.is_empty() {
        ctx.renderer.write_line("  deltas:", c_result())?;
        for d in &deltas {
            ctx.renderer.write_line(
                &format!("    {} {}:{}", d.op, d.capability, d.requirement),
                c_result(),
            )?;
        }
    }

    let tasks = store.list_tasks(slug).unwrap_or_default();
    let (done, total) = store.task_progress(slug).unwrap_or((0, 0));
    if !tasks.is_empty() {
        ctx.renderer
            .write_line(&format!("  tasks ({done}/{total}):"), c_result())?;
        for t in &tasks {
            let mark = match t.status.as_str() {
                "done" => "x",
                "in_progress" => "~",
                "blocked" => "!",
                _ => " ",
            };
            ctx.renderer.write_line(
                &format!(
                    "    [{mark}] {}.{} {} (#{})",
                    t.group_no, t.seq, t.text, t.id
                ),
                c_result(),
            )?;
        }
    }
    Ok(())
}

fn show_specs(
    ctx: &mut SlashCtx<'_>,
    store: &crate::extras::spec_db::SpecStore,
    capability: Option<&str>,
) -> anyhow::Result<()> {
    match capability {
        Some(cap) => {
            let reqs = store.capability_requirements(cap).unwrap_or_default();
            if reqs.is_empty() {
                ctx.renderer.write_line(
                    &format!("no requirements for capability '{cap}'"),
                    c_result(),
                )?;
                return Ok(());
            }
            ctx.renderer.write_line(&format!("{cap}:"), c_result())?;
            for r in &reqs {
                ctx.renderer
                    .write_line(&format!("  • {} — {}", r.name, r.text), c_result())?;
                for s in &r.scenarios {
                    ctx.renderer.write_line(
                        &format!("      ◦ {}: {}", s.name, s.when_then),
                        theme::dim(),
                    )?;
                }
            }
        }
        None => {
            let caps = store.list_capabilities().unwrap_or_default();
            if caps.is_empty() {
                ctx.renderer
                    .write_line("no living specs yet.", c_result())?;
                return Ok(());
            }
            ctx.renderer.write_line("capabilities:", c_result())?;
            for c in &caps {
                ctx.renderer.write_line(&format!("  {c}"), c_result())?;
            }
            ctx.renderer
                .write_line("  /spec specs <capability> for requirements", theme::dim())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // dirge-s5oh: /spec must anchor its ProjectPaths on the session's
    // working_dir (like /issues, /graph, /clear), not the process cwd.
    #[test]
    fn resolves_paths_from_the_given_working_dir_not_process_cwd() {
        // If an explicit project-root override is active, anchoring is
        // env-driven and this assertion doesn't apply.
        if crate::extras::dirge_paths::project_root_override().is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("dirge-spec-s5oh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let canon = dir.canonicalize().unwrap();

        let paths = resolve_project_paths(dir.to_str().unwrap());
        // No .git above /tmp → project root is the working_dir itself.
        assert_eq!(
            paths.root, canon,
            "spec must anchor on the session working_dir"
        );
        // And specifically not the process cwd (the s5oh bug).
        assert_ne!(
            paths.root,
            std::env::current_dir().unwrap(),
            "spec must not fall back to the process cwd"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
