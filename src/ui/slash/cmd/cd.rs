//! /cd handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_cd(ctx: &mut SlashCtx<'_>, text: &str) -> anyhow::Result<()> {
    let raw_args = text.trim().strip_prefix("/cd").unwrap_or("").trim();
    let target = raw_args;
    let path = if target.is_empty() {
        dirs::home_dir().unwrap_or_default()
    } else if let Some(rest) = target.strip_prefix('~') {
        let mut home = dirs::home_dir().unwrap_or_default();
        home.push(rest.trim_start_matches('/'));
        home
    } else {
        std::path::PathBuf::from(target)
    };
    match std::env::set_current_dir(&path) {
        Ok(()) => {
            let canonical = dunce::canonicalize(&path).unwrap_or(path);
            ctx.session.working_dir = CompactString::new(canonical.to_string_lossy().as_ref());
            if let Some(perm) = ctx.permission
                && let Ok(mut guard) = perm.lock()
            {
                guard.set_working_dir(&ctx.session.working_dir);
            }
            ctx.context.reload();
            let model = ctx.client.completion_model(ctx.session.model.to_string());
            *ctx.agent = crate::provider::build_agent(
                model,
                ctx.cli,
                ctx.cfg,
                ctx.context,
                ctx.permission.clone(),
                ctx.ask_tx.clone(),
                ctx.question_tx.clone(),
                ctx.plan_tx.clone(),
                ctx.bg_store.clone(),
                #[cfg(feature = "lsp")]
                ctx.lsp_manager.cloned(),
                ctx.sandbox.clone(),
                #[cfg(feature = "mcp")]
                ctx.mcp_manager,
                #[cfg(feature = "semantic")]
                ctx.semantic_manager,
                Some(ctx.session.id.to_string()),
            )
            .await;
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer.write_line(
                &format!("changed directory to {}", ctx.session.working_dir),
                c_agent(),
            )?;
        }
        Err(e) => {
            ctx.renderer.write_line(&format!("cd: {}", e), c_error())?;
        }
    }
    Ok(())
}
