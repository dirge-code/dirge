//! /allow list — list session allowlist entries.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_result};
use crate::ui::theme;

pub(crate) async fn cmd_allow_list(
    ctx: &mut SlashCtx<'_>,
    perm: &crate::permission::checker::PermCheck,
) -> anyhow::Result<()> {
    let entries = {
        let guard = perm.lock_ignore_poison();
        guard.allowlist_entries()
    };
    if entries.is_empty() {
        ctx.renderer.write_line(
            "session allowlist is empty (use '(a) allow always' in a permission prompt to add entries)",
            c_agent(),
        )?;
    } else {
        ctx.renderer.write_line(
            &format!("session allowlist ({} entries):", entries.len()),
            c_agent(),
        )?;
        for (i, (tool, pat)) in entries.iter().enumerate() {
            ctx.renderer
                .write_line(&format!("  [{}] {} {}", i, tool, pat), c_result())?;
        }
        ctx.renderer.write_line(
            "use '/allow remove <idx>' to drop a single entry; '/allow clear' to drop all",
            theme::dim(),
        )?;
    }
    Ok(())
}
