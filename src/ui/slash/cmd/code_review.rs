//! /code-review handler (dirge-iyf5).
//!
//! Runs the diff-aware reviewer's two-pass review over the current
//! working-tree diff on demand, regardless of loop state, and prints the
//! findings. Reuses the same reviewer judge the in-loop gate uses (built
//! from `critic_provider`), so it's the same review with no extra config.
//! Unlike the in-loop gate it never blocks or feeds back — it's a manual
//! report, so every finding (blocking and advisory) is shown, ranked.

use crate::ui::slash::{SlashCtx, c_result};
use crate::ui::theme;

pub(crate) async fn cmd_code_review(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    // Reuse the wired reviewer judge; clone the Arc so we don't hold a
    // borrow of `ctx.agent` across the later `&mut ctx.renderer` writes.
    let Some(review_fn) = ctx.agent.code_review_fn().cloned() else {
        ctx.renderer.write_line(
            "/code-review needs a reviewer — set `critic_provider` in your config to enable it.",
            theme::dim(),
        )?;
        return Ok(());
    };

    let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let diff = tokio::task::spawn_blocking(move || {
        crate::agent::agent_loop::code_review::capture_run_diff(&repo)
    })
    .await
    .ok()
    .flatten();
    let Some(diff) = diff else {
        ctx.renderer
            .write_line("no uncommitted changes to review.", c_result())?;
        return Ok(());
    };

    ctx.renderer
        .write_line("reviewing working-tree diff…", theme::dim())?;
    let findings = crate::agent::agent_loop::code_review::run_code_review(
        &review_fn,
        "",
        &diff,
        "(on-demand /code-review — no session transcript)",
    )
    .await;

    if findings.is_empty() {
        ctx.renderer
            .write_line("code review: no issues found.", c_result())?;
        return Ok(());
    }

    ctx.renderer.write_line(
        &format!("code review: {} finding(s)", findings.len()),
        c_result(),
    )?;
    for f in &findings {
        ctx.renderer
            .write_line(&format!("  ── [{}] ──", f.severity.label()), theme::dim())?;
        for line in f.body.trim().lines() {
            ctx.renderer.write_line(&format!("  {line}"), c_result())?;
        }
    }
    Ok(())
}
