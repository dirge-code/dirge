//! `/debug` slash command — user-facing DAP debugger controls.
//!
//! Provides direct TUI access to the debugger without going through the
//! AI agent. Subcommands: launch, attach, step, continue, step_over,
//! step_in, step_out, terminate, breakpoint, evaluate, sessions, panel.

mod cont;

mod attach;
mod breakpoint;
mod evaluate;
mod launch;
mod panel;
mod repl;
mod sessions;
mod stepping;
mod terminate;

pub(crate) use repl::cmd_dap_repl;

use std::time::Duration;

use crate::dap::session::DapSessionManager;
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn cmd_debug(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        print_usage(ctx).await?;
        return Ok(());
    }

    let sub = parts[1];
    let args = &parts[2..];

    match sub {
        "launch" => launch::cmd_launch(ctx, args).await?,
        "attach" => attach::cmd_attach(ctx, args).await?,
        "step" | "step_over" => stepping::cmd_step_over(ctx).await?,
        "step_in" => stepping::cmd_step_in(ctx).await?,
        "step_out" => stepping::cmd_step_out(ctx).await?,
        "continue" => cont::cmd_continue(ctx).await?,
        "terminate" | "stop" => terminate::cmd_terminate(ctx).await?,
        "breakpoint" | "bp" => breakpoint::cmd_breakpoint(ctx, args).await?,
        "evaluate" | "eval" => evaluate::cmd_evaluate(ctx, args).await?,
        "sessions" | "status" => sessions::cmd_sessions(ctx).await?,
        "panel" => panel::cmd_debug_panel(ctx).await?,
        _ => {
            ctx.renderer
                .write_line(&format!("unknown /debug subcommand: {sub}"), c_error())?;
            print_usage(ctx).await?;
        }
    }
    Ok(())
}

async fn print_usage(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    ctx.renderer
        .write_line("usage: /debug <subcommand> [args...]", c_agent())?;
    ctx.renderer.write_line(
        "  launch <file> [--adapter <name>]    start debugging a program",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  attach <pid> [--adapter <name>]     attach to a running process",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  breakpoint <file> <line>            set a breakpoint",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  step                                step over current line",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  step_in                             step into function call",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  step_out                            step out of current function",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  continue                            resume execution",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  evaluate <expression>               evaluate an expression",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  sessions                            show active debug session",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  terminate                           end debug session",
        c_result(),
    )?;
    ctx.renderer.write_line(
        "  panel                               toggle debug panel on right",
        c_result(),
    )?;
    Ok(())
}

pub(super) fn get_manager() -> Option<std::sync::Arc<DapSessionManager>> {
    crate::dap::session::DAP_MANAGER
        .lock()
        .ok()
        .and_then(|g| g.clone())
}

pub(super) async fn require_session() -> anyhow::Result<std::sync::Arc<DapSessionManager>> {
    let mgr = match get_manager() {
        Some(m) => m,
        None => {
            anyhow::bail!("no debug session manager — start a conversation first");
        }
    };

    if mgr.active_summary().await.is_none() {
        anyhow::bail!("no active debug session — use /debug launch <file> first");
    }

    Ok(mgr)
}

pub(super) async fn print_stop(
    ctx: &mut SlashCtx<'_>,
    summary: &crate::dap::types::SessionSummary,
) -> anyhow::Result<()> {
    ctx.renderer.write_line(
        &format!(
            "stopped — reason: {}, thread: {}",
            summary.stop_reason.as_deref().unwrap_or("unknown"),
            summary.thread_id.map_or("?".into(), |t| t.to_string()),
        ),
        c_result(),
    )?;
    Ok(())
}

/// Parse a flag value like `--adapter debugpy` from a positional args slice.
/// Returns `None` if the flag isn't found or if the "value" looks like another
/// flag (starts with `--`), guarding against `--adapter --other-flag` being
/// silently interpreted as adapter `--other-flag`.
pub(super) fn parse_flag<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    for i in 0..args.len().saturating_sub(1) {
        if args[i] == flag && !args[i + 1].starts_with("--") {
            return Some(args[i + 1]);
        }
    }
    None
}
