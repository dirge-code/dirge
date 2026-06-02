//! `/plan <request>` — the phased plan workflow entry (vix port, P3e-b).
//!
//! Runs the two read-only forks (explore → plan) inline here, exactly like
//! `/compress` does its heavy work in the slash handler, then hands a
//! [`PlanKickoff`] back to the UI loop, which launches the *streamed* implement
//! run and seeds the reviewer loop (driven in `run_handlers/done.rs`). The
//! phases are separate forks → genuine context resets, per the chosen
//! "separate-agent phases" model.

use super::SlashCtx;
use crate::agent::phased_orchestrator::{ActivePlan, PlanKickoff, collect_runner_text};
use crate::agent::plan_workflow::{READONLY_PHASE_TOOLS, explore_prompt, plan_prompt};
use crate::ui::colors::{c_agent, c_error};

pub(super) async fn cmd_plan(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    _text: &str,
) -> anyhow::Result<()> {
    if !ctx.cfg.resolve_phased_workflow_enabled() {
        ctx.renderer.write_line(
            "/plan is off — set phased_workflow_enabled = true in your config to enable the phased workflow",
            c_error(),
        )?;
        return Ok(());
    }

    let request = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    if request.trim().is_empty() {
        ctx.renderer
            .write_line("usage: /plan <request>", c_error())?;
        return Ok(());
    }

    // A frozen snapshot of the conversation so far — the same view every phase
    // fork explores from.
    let transcript = crate::agent::review::build_transcript(ctx.session);

    // Phase 1: Explore (read-only fork, fresh context).
    ctx.renderer.write_line(
        "Phase: Explore — mapping the codebase (read-only)…",
        c_agent(),
    )?;
    let explore_runner = ctx.agent.spawn_phase_runner(
        explore_prompt(&request),
        transcript.clone(),
        READONLY_PHASE_TOOLS,
    );
    let findings = match collect_runner_text(explore_runner).await {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            ctx.renderer
                .write_line("Phase: Explore — produced no findings; aborting", c_error())?;
            return Ok(());
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("Phase: Explore — error: {e}; aborting"), c_error())?;
            return Ok(());
        }
    };

    // Phase 2: Plan (read-only fork; the ONLY thing carried over is the
    // findings report — a true context reset between phases).
    ctx.renderer.write_line(
        "Phase: Plan — turning findings into an implementation plan…",
        c_agent(),
    )?;
    let plan_runner = ctx.agent.spawn_phase_runner(
        plan_prompt(&request, &findings),
        transcript,
        READONLY_PHASE_TOOLS,
    );
    let plan = match collect_runner_text(plan_runner).await {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            ctx.renderer
                .write_line("Phase: Plan — produced no plan; aborting", c_error())?;
            return Ok(());
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("Phase: Plan — error: {e}; aborting"), c_error())?;
            return Ok(());
        }
    };

    // Hand off to the UI loop: it launches the streamed implement run and
    // arms the reviewer loop.
    let cycles = ctx.cfg.resolve_phased_workflow_max_review_cycles();
    let impl_prompt = format!(
        "{request}\n\n--- Implementation plan (from the planning phase) ---\n{plan}\n\n\
         Implement this plan now. Make the edits and run the build/tests to verify.",
    );
    ctx.renderer.write_line(
        "Phase: Implement — executing the plan (you'll watch it run)…",
        c_agent(),
    )?;
    *ctx.plan_kickoff = Some(PlanKickoff {
        impl_prompt,
        active: ActivePlan {
            plan,
            cycles_left: cycles,
        },
    });
    Ok(())
}
