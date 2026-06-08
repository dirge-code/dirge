use crate::dap::types::SourceBreakpoint;
use crate::ui::slash::{SlashCtx, c_error, c_result};

use super::{DEFAULT_TIMEOUT, get_manager};

pub(super) async fn cmd_breakpoint(ctx: &mut SlashCtx<'_>, args: &[&str]) -> anyhow::Result<()> {
    if args.len() < 2 {
        ctx.renderer
            .write_line("usage: /debug breakpoint <file> <line>", c_error())?;
        return Ok(());
    }

    let file = args[0];
    let line: u32 = match args[1].parse() {
        Ok(l) => l,
        Err(_) => {
            ctx.renderer
                .write_line(&format!("invalid line number: {}", args[1]), c_error())?;
            return Ok(());
        }
    };

    let mgr = match get_manager() {
        Some(m) => m,
        None => {
            ctx.renderer.write_line(
                "no debug session manager — start a conversation first",
                c_error(),
            )?;
            return Ok(());
        }
    };

    let bp = SourceBreakpoint {
        line: line as i64,
        ..Default::default()
    };

    match mgr.set_breakpoints(file, vec![bp], DEFAULT_TIMEOUT).await {
        Ok(results) => {
            ctx.renderer.write_line(
                &format!("set {} breakpoint(s) in {file}", results.len()),
                c_result(),
            )?;
            for r in &results {
                ctx.renderer.write_line(
                    &format!("  line {} — verified: {}", r.line.unwrap_or(0), r.verified),
                    c_result(),
                )?;
            }
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("breakpoint failed: {e}"), c_error())?;
        }
    }
    Ok(())
}
