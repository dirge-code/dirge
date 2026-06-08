//! /allow remove <idx> — remove an entry by index.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_allow_remove(
    ctx: &mut SlashCtx<'_>,
    perm: &crate::permission::checker::PermCheck,
    parts: &[&str],
) -> anyhow::Result<()> {
    let idx_str = parts.get(2).copied().unwrap_or("");
    let idx: usize = match idx_str.parse() {
        Ok(n) => n,
        Err(_) => {
            ctx.renderer.write_line(
                "usage: /allow remove <idx>  (run /allow list to see indices)",
                c_error(),
            )?;
            return Ok(());
        }
    };
    let removed = {
        let mut guard = perm.lock_ignore_poison();
        guard.remove_session_allowlist_at(idx)
    };
    match removed {
        Some((tool, pat)) => {
            ctx.session
                .permission_allowlist
                .retain(|e| !(e.tool == tool && e.pattern == pat));
            ctx.renderer
                .write_line(&format!("removed [{}]: {} {}", idx, tool, pat), c_agent())?;
        }
        None => {
            ctx.renderer
                .write_line(&format!("no allowlist entry at index {}", idx), c_error())?;
        }
    }
    Ok(())
}
