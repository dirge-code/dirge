//! /allow clear — clear all session allowlist entries.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_allow_clear(
    ctx: &mut SlashCtx<'_>,
    perm: &crate::permission::checker::PermCheck,
) -> anyhow::Result<()> {
    {
        let mut guard = perm.lock_ignore_poison();
        guard.clear_session_allowlist();
    }
    ctx.session.permission_allowlist.clear();
    ctx.renderer
        .write_line("session allowlist cleared", c_agent())?;
    Ok(())
}
