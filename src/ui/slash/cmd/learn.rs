//! /learn handler (dirge-s99m).
//!
//! Builds the standards-guided learn instruction and hands it to the
//! loop as an agent turn via the `DEFER_PROMPT_RUN:` sentinel — the same
//! control-flow channel `/prompt` and `/btw` use, since slash handlers
//! can't touch the loop's run slots directly. Bare `/learn` (no request)
//! is valid: the prompt falls back to distilling the conversation.

use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_learn(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let request = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    let prompt = crate::agent::learn::build_learn_prompt(&request);
    let _ = ctx;
    Err(anyhow::anyhow!("DEFER_PROMPT_RUN:{}", prompt))
}
