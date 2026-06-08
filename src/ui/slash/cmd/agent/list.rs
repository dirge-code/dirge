//! /agent list (shared with /agents).

pub(crate) async fn cmd_agent_list(
    ctx: &mut crate::ui::slash::SlashCtx<'_>,
    parts: &[&str],
) -> anyhow::Result<()> {
    super::super::agents::cmd_agents(ctx, parts).await
}
