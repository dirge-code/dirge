//! `/dap-repl` — terse, REPL-style aliases over the same DAP session
//! the `/debug` command drives (dirge-h68s).
//!
//! This is `/debug` with a debugger's muscle-memory shorthand (`c`,
//! `n`, `s`, `p`, `bp`, …). Every alias resolves to a [`DapOp`] and
//! routes to the SAME `pub(super)` subcommand handler `/debug` uses —
//! there is no second implementation of the debug operations, only a
//! second name table.

use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

/// A resolved REPL operation. Splitting alias-parsing (pure, tested)
/// from handler dispatch keeps the alias table verifiable without a
/// live `SlashCtx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DapOp {
    Launch,
    Attach,
    Terminate,
    Continue,
    StepOver,
    StepIn,
    StepOut,
    Breakpoint,
    Evaluate,
    Sessions,
    Help,
}

/// Map a REPL alias to its operation. `None` = unrecognized.
pub(crate) fn resolve_alias(alias: &str) -> Option<DapOp> {
    Some(match alias {
        "launch" | "l" => DapOp::Launch,
        "attach" | "a" => DapOp::Attach,
        "terminate" | "term" | "q" | "quit" | "kill" => DapOp::Terminate,
        "c" | "cont" | "continue" => DapOp::Continue,
        "n" | "next" | "step" | "over" => DapOp::StepOver,
        "s" | "si" | "stepin" | "step_in" => DapOp::StepIn,
        "o" | "so" | "stepout" | "step_out" => DapOp::StepOut,
        "bp" | "break" | "breakpoint" => DapOp::Breakpoint,
        "p" | "print" | "eval" | "evaluate" => DapOp::Evaluate,
        "bt" | "status" | "sessions" | "info" => DapOp::Sessions,
        "help" | "?" => DapOp::Help,
        _ => return None,
    })
}

/// Dispatch a `/dap-repl <alias> [args…]` line. `parts[0]` is
/// `/dap-repl`; `parts[1]` is the alias; the rest are args.
pub(crate) async fn cmd_dap_repl(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let Some(alias) = parts.get(1) else {
        return print_usage(ctx).await;
    };
    let args = &parts[2..];

    let Some(op) = resolve_alias(alias) else {
        ctx.renderer
            .write_line(&format!("unknown dap-repl command: {alias}"), c_error())?;
        return print_usage(ctx).await;
    };

    match op {
        DapOp::Launch => super::launch::cmd_launch(ctx, args).await?,
        DapOp::Attach => super::attach::cmd_attach(ctx, args).await?,
        DapOp::Terminate => super::terminate::cmd_terminate(ctx).await?,
        DapOp::Continue => super::cont::cmd_continue(ctx).await?,
        DapOp::StepOver => super::stepping::cmd_step_over(ctx).await?,
        DapOp::StepIn => super::stepping::cmd_step_in(ctx).await?,
        DapOp::StepOut => super::stepping::cmd_step_out(ctx).await?,
        DapOp::Breakpoint => super::breakpoint::cmd_breakpoint(ctx, args).await?,
        DapOp::Evaluate => super::evaluate::cmd_evaluate(ctx, args).await?,
        DapOp::Sessions => super::sessions::cmd_sessions(ctx).await?,
        DapOp::Help => print_usage(ctx).await?,
    }
    Ok(())
}

async fn print_usage(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    ctx.renderer.write_line(
        "usage: /dap-repl <cmd> [args…]   (terse aliases for /debug)",
        c_agent(),
    )?;
    for (alias, desc) in DAP_REPL_HELP {
        ctx.renderer
            .write_line(&format!("  {alias:<22}{desc}"), c_result())?;
    }
    Ok(())
}

/// (alias-summary, description) rows for `/dap-repl` usage output.
const DAP_REPL_HELP: &[(&str, &str)] = &[
    ("launch <file> | l", "start debugging a program"),
    ("attach <pid> | a", "attach to a running process"),
    ("bp <file> <line>", "set a breakpoint"),
    ("c | continue", "resume execution"),
    ("n | next | step", "step over current line"),
    ("s | si | step_in", "step into a call"),
    ("o | so | step_out", "step out of the current function"),
    ("p <expr> | print", "evaluate an expression"),
    ("bt | status", "show the active session"),
    ("terminate | q", "end the debug session"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every canonical alias advertised to tab completion must
    /// resolve to an op — otherwise completion offers a token the
    /// dispatcher rejects. Mirrors the `/dap-repl` entry in
    /// completion.rs's `SUBCOMMAND_ENTRIES`; keep the two in sync.
    #[test]
    fn completion_aliases_all_resolve() {
        const COMPLETION_TOKENS: &[&str] = &[
            "launch",
            "attach",
            "bp",
            "c",
            "n",
            "s",
            "o",
            "p",
            "bt",
            "status",
            "terminate",
            "help",
        ];
        for alias in COMPLETION_TOKENS {
            assert!(
                resolve_alias(alias).is_some(),
                "completion advertises '{alias}' but resolve_alias rejects it",
            );
        }
    }

    /// The documented short forms from docs/dap.md route to the right
    /// operations.
    #[test]
    fn documented_aliases_route_correctly() {
        assert_eq!(resolve_alias("launch"), Some(DapOp::Launch));
        assert_eq!(resolve_alias("bp"), Some(DapOp::Breakpoint));
        assert_eq!(resolve_alias("c"), Some(DapOp::Continue));
        assert_eq!(resolve_alias("p"), Some(DapOp::Evaluate));
        assert_eq!(resolve_alias("n"), Some(DapOp::StepOver));
        assert_eq!(resolve_alias("terminate"), Some(DapOp::Terminate));
        assert_eq!(resolve_alias("nonsense"), None);
    }
}
