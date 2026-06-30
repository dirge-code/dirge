//! /prompt command dispatch.

pub(crate) mod default;
pub(crate) mod list;
pub(crate) mod switch;

use crate::ui::slash::SlashCtx;

pub(crate) async fn cmd_prompt(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        return list::cmd_prompt_list(ctx).await;
    }
    if parts[1] == "default" && !ctx.context.prompts.contains_key("default") {
        return default::cmd_prompt_default(ctx).await;
    }
    let name = parts[1].trim();
    switch::cmd_prompt_switch(ctx, name).await?;
    // `/prompt <name> <text...>`: switch then run a streamed turn on the
    // trailing text, mirroring `/plan`. Only when the named prompt actually
    // resolved — switch renders an "unknown prompt" notice and returns Ok for a
    // bad name, so gate the defer on the prompt existing. Slash handlers can't
    // touch the loop's run slots, so the DEFER_PROMPT_RUN sentinel hands the
    // text back to the UI loop, which launches the turn (see the match on
    // handle_slash's result in ui/mod.rs).
    if ctx.context.prompts.contains_key(name) {
        if let Some(sentinel) = defer_prompt_run_sentinel(parts) {
            return Err(anyhow::anyhow!("{}", sentinel));
        }
    }
    Ok(())
}

/// `/prompt <name> <text...>`: when the user typed trailing words after the
/// prompt name, this is the sentinel `cmd_prompt` returns so the UI loop
/// launches a streamed turn on that text (mirroring `/plan`). Returns `None`
/// for a bare `/prompt <name>`. Slash handlers can't touch the loop's run
/// slots, so control is handed back via the `DEFER_PROMPT_RUN:` sentinel — the
/// same control-flow channel as `DEFER_COMPRESS`/`DEFER_BTW`.
fn defer_prompt_run_sentinel(parts: &[&str]) -> Option<String> {
    (parts.len() > 2).then(|| format!("DEFER_PROMPT_RUN:{}", parts[2..].join(" ")))
}

#[cfg(test)]
mod tests {
    use super::defer_prompt_run_sentinel;

    #[test]
    fn defer_sentinel_joins_trailing_text_after_the_prompt_name() {
        let parts = ["/prompt", "review", "please", "review", "my", "changes"];
        assert_eq!(
            defer_prompt_run_sentinel(&parts).as_deref(),
            Some("DEFER_PROMPT_RUN:please review my changes")
        );
    }

    #[test]
    fn defer_sentinel_is_none_for_bare_prompt_switch() {
        let parts = ["/prompt", "review"];
        assert_eq!(defer_prompt_run_sentinel(&parts), None);
    }

    #[test]
    fn defer_sentinel_preserves_a_single_trailing_word() {
        let parts = ["/prompt", "plan", "go"];
        assert_eq!(
            defer_prompt_run_sentinel(&parts).as_deref(),
            Some("DEFER_PROMPT_RUN:go")
        );
    }
}
