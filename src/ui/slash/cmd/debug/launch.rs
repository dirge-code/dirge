use crate::dap::config::{self, ConnectMode};
use crate::ui::renderer::PanelMode;
use crate::ui::slash::{SlashCtx, c_agent, c_error};

use super::{DEFAULT_TIMEOUT, get_manager, parse_flag};

pub(super) async fn cmd_launch(ctx: &mut SlashCtx<'_>, args: &[&str]) -> anyhow::Result<()> {
    if args.is_empty() {
        ctx.renderer
            .write_line("usage: /debug launch <file> [--adapter <name>]", c_error())?;
        return Ok(());
    }

    let program = args[0];
    let adapter_name = parse_flag(args, "--adapter");

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let prog_path = std::path::Path::new(program);

    let adapter = if let Some(name) = adapter_name {
        config::resolve_adapter(name)
            .ok_or_else(|| anyhow::anyhow!("adapter not found on PATH: {name}"))?
    } else {
        config::select_launch_adapter(prog_path, &cwd, None).ok_or_else(|| {
            anyhow::anyhow!(
                "no debug adapter found for {program}. Install one (debugpy, gdb, lldb-dap, etc.) \
                     or specify --adapter <name>"
            )
        })?
    };

    if adapter.connect_mode == ConnectMode::Socket {
        ctx.renderer
            .write_line("socket-mode adapters are not yet supported", c_error())?;
        return Ok(());
    }

    let mgr = match get_manager() {
        Some(m) => m,
        None => {
            ctx.renderer.write_line(
                "no debug session manager — start a conversation first",
                c_error(),
            )?;
            return Ok(());
        }
    };

    ctx.renderer.write_line(
        &format!("launching {} with adapter {}...", program, adapter.name),
        c_agent(),
    )?;
    ctx.renderer.write_line(
        "  (launch runs in background — use /debug sessions to check result)",
        c_agent(),
    )?;

    ctx.renderer.set_right_panel_mode(PanelMode::Debug);
    ctx.renderer.render_viewport()?;

    let adapter_name = adapter.name.clone();
    let adapter_cmd = adapter.resolved_command.to_string_lossy().to_string();
    let adapter_args = adapter.args.clone();
    let cwd_str = cwd.to_string_lossy().to_string();
    let program = program.to_string();
    let launch_defaults = adapter.launch_defaults.clone();
    let languages = adapter.languages.clone();

    tokio::spawn(async move {
        let signal = crate::agent::agent_loop::tool::AbortSignal::new();
        match mgr
            .launch(
                &adapter_name,
                &adapter_cmd,
                &adapter_args,
                &cwd_str,
                Some(&program),
                None,
                &[],
                Some(true),
                Some(launch_defaults),
                &signal,
                DEFAULT_TIMEOUT,
                languages,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let msg = format!("/debug launch failed: {e}");
                tracing::error!("{msg}");
                crate::ui::notifications::notify_send(
                    crate::ui::notifications::Notification::Error(msg),
                );
            }
        }
    });

    Ok(())
}
