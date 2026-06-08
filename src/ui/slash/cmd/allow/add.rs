//! /allow add — add a tool+pattern to the session allowlist.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_allow_add(
    ctx: &mut SlashCtx<'_>,
    perm: &crate::permission::checker::PermCheck,
    text: &str,
) -> anyhow::Result<()> {
    let raw_args = text.trim().strip_prefix("/allow").unwrap_or("").trim();
    let rest = raw_args.strip_prefix("add").unwrap_or("").trim();
    let mut it = rest.splitn(2, char::is_whitespace);
    let tool = it.next().unwrap_or("");
    let pattern = it.next().unwrap_or("").trim();
    let known = crate::agent::tools::BUILTIN_TOOL_NAMES;
    if tool.is_empty() || pattern.is_empty() {
        ctx.renderer.write_line(
            "usage: /allow add <tool> <pattern>  (e.g. /allow add bash 'cargo *')",
            c_error(),
        )?;
    } else if !known.contains(&tool) {
        ctx.renderer.write_line(
            &format!("unknown tool {:?}. Valid: {}", tool, known.join(", ")),
            c_error(),
        )?;
    } else {
        {
            let mut guard = perm.lock_ignore_poison();
            guard.add_session_allowlist(tool.to_string(), pattern);
        }
        let entry = crate::session::PermissionAllowEntry {
            tool: tool.to_string(),
            pattern: pattern.to_string(),
        };
        if !ctx
            .session
            .permission_allowlist
            .iter()
            .any(|e| e.tool == entry.tool && e.pattern == entry.pattern)
        {
            ctx.session.permission_allowlist.push(entry);
        }
        ctx.renderer
            .write_line(&format!("added: {} {}", tool, pattern), c_agent())?;
    }
    Ok(())
}
