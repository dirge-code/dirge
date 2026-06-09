//! /loop start <prompt> — start a loop.

#[cfg(feature = "loop")]
use crate::ui::slash::c_error;
use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_loop_start(
    ctx: &mut SlashCtx<'_>,
    _parts: &[&str],
    #[cfg(feature = "loop")] text: &str,
    #[cfg(not(feature = "loop"))] _text: &str,
) -> anyhow::Result<()> {
    #[cfg(feature = "loop")]
    {
        let after = text.trim().strip_prefix("/loop").unwrap_or("").trim_start();
        let tokens: Vec<&str> = after.split_whitespace().collect();
        let mut max_iterations: Option<u32> = Some(20);
        let mut prompt_tokens: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            if tokens[i] == "--max" && i + 1 < tokens.len() {
                match tokens[i + 1].parse::<u32>() {
                    Ok(0) => max_iterations = None,
                    Ok(n) => max_iterations = Some(n),
                    Err(_) => {
                        ctx.renderer.write_line(
                            &format!(
                                "invalid --max value: {} (use a positive integer, or 0 for unbounded)",
                                tokens[i + 1]
                            ),
                            c_error(),
                        )?;
                        return Ok(());
                    }
                }
                i += 2;
            } else {
                prompt_tokens.push(tokens[i]);
                i += 1;
            }
        }
        let prompt = prompt_tokens.join(" ");
        if prompt.is_empty() {
            ctx.renderer.write_line(
                "usage: /loop [--max N] <prompt>  (default cap: 20 iterations; --max 0 = unbounded)",
                c_error(),
            )?;
            return Ok(());
        }
        let plan_file = std::path::PathBuf::from("LOOP_PLAN.md");
        let ls = crate::extras::r#loop::LoopState::new(prompt, plan_file, max_iterations, None);
        *ctx.loop_state = Some(ls);
        let cap_msg = match max_iterations {
            Some(n) => format!(
                "loop started (max {n} iterations) — iteration 1 will run after this message"
            ),
            None => "loop started (unbounded — use /loop stop to cancel) — iteration 1 will run after this message".to_string(),
        };
        ctx.renderer.write_line(&cap_msg, c_agent())?;
    }
    #[cfg(not(feature = "loop"))]
    ctx.renderer.write_line(
        "/loop requires the 'loop' feature: cargo build --features loop",
        c_agent(),
    )?;
    Ok(())
}
