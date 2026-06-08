use crate::ui::renderer::PanelMode;
use crate::ui::slash::{SlashCtx, c_agent};

pub(super) async fn cmd_debug_panel(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    ctx.renderer.set_right_panel_mode(PanelMode::Debug);
    ctx.renderer.render_viewport()?;
    ctx.renderer.write_line(
        "debug panel shown on right (use /panel off to hide)",
        c_agent(),
    )?;
    Ok(())
}
