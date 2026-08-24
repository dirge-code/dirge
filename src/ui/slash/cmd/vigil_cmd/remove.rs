//! /vigil remove — remove a vigil definition.

use crate::extras::dirge_paths::ProjectPaths;
use crate::extras::vigil_db::VigilStore;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

pub(crate) async fn cmd_vigil_remove(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let name = parts.get(2).copied().unwrap_or("");
    if name.is_empty() {
        ctx.renderer
            .write_line("usage: /vigil remove <name>", c_error())?;
        return Ok(());
    }

    let paths = ProjectPaths::new(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    let db_path = paths.session_db_path();
    if db_path.exists() {
        match VigilStore::open_at(&db_path) {
            Ok(store) => {
                if let Err(e) = store.remove(name) {
                    ctx.renderer
                        .write_line(&format!("vigil '{name}' not found: {e}"), c_error())?;
                    return Ok(());
                }
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("cannot open vigil store: {e}"), c_error())?;
                return Ok(());
            }
        }
    } else {
        ctx.renderer
            .write_line(&format!("vigil '{name}' not found"), c_error())?;
        return Ok(());
    }

    ctx.renderer
        .write_line(&format!("vigil '{name}' removed"), c_agent())?;
    Ok(())
}
