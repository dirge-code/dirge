//! Model, mode, prompt, reasoning, toggle, regen-prompts handlers.
//!
//! These commands typically read or rebuild the active model/agent
//! and live together because they share the build_agent rebuild
//! pattern. Extracted from the original mega-match in slash.rs as
//! part of the arch/split-large-modules refactor.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use super::{SlashCtx, c_agent, c_error, c_result};
use crate::permission::SecurityMode;

pub(super) async fn cmd_model(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line(&format!("current model: {}", ctx.session.model), c_agent())?;
    } else {
        let new_model = CompactString::new(parts[1].trim());
        let model = ctx.client.completion_model(new_model.to_string());
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
        ctx.session.model = new_model.clone();
        ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
        let new_ctx = ctx.cfg.resolve_context_window(new_model.as_str());
        let old_ctx = ctx.session.context_window;
        if new_ctx != old_ctx {
            ctx.session.context_window = new_ctx;
        }
        ctx.renderer
            .write_line(&format!("switched to model: {}", new_model), c_agent())?;
        let reserve = ctx.cfg.resolve_reserve_tokens();
        let budget = new_ctx.saturating_sub(reserve);
        if new_ctx < old_ctx && ctx.session.total_estimated_tokens > budget {
            ctx.renderer.write_line(
                &format!(
                    "warning: session uses ~{}k tokens but new model's context budget is ~{}k. Run /compress before the next prompt or the next turn may overflow.",
                    ctx.session.total_estimated_tokens / 1_000,
                    budget / 1_000,
                ),
                c_error(),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_reasoning(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.show_reasoning = !*ctx.show_reasoning;
    ctx.renderer.write_line(
        &format!(
            "reasoning visibility: {}",
            if *ctx.show_reasoning { "on" } else { "off" }
        ),
        c_agent(),
    )?;
    Ok(())
}

pub(super) async fn cmd_mode(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let current_mode = ctx
        .permission
        .as_ref()
        .map(|p| p.lock_ignore_poison().mode())
        .unwrap_or(SecurityMode::Standard);

    if parts.len() < 2 {
        ctx.renderer.write_line("security mode:", c_agent())?;
        ctx.renderer
            .write_line(&format!("  current: {}", current_mode), c_result())?;
        ctx.renderer.write_line("", c_agent())?;
        ctx.renderer.write_line(
            "  /mode standard      use configured permission rules",
            c_result(),
        )?;
        ctx.renderer
            .write_line("  /mode restrictive   default all tools to ask", c_result())?;
        ctx.renderer.write_line(
            "  /mode accept        auto-accept within working directory",
            c_result(),
        )?;
        ctx.renderer.write_line(
            "  /mode yolo          auto-accept ALL operations",
            c_result(),
        )?;
        ctx.renderer.write_line("", c_agent())?;
    } else {
        match parts[1] {
            "standard" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Standard);
                    ctx.renderer
                        .write_line("security mode: standard", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "restrictive" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Restrictive);
                    ctx.renderer
                        .write_line("security mode: restrictive", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "accept" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Accept);
                    ctx.renderer
                        .write_line("security mode: accept (auto-allow within CWD)", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            "yolo" => {
                if let Some(p) = ctx.permission {
                    p.lock_ignore_poison().set_mode(SecurityMode::Yolo);
                    ctx.renderer
                        .write_line("security mode: YOLO (all operations allowed)", c_agent())?;
                } else {
                    ctx.renderer
                        .write_line("permission system not active", c_error())?;
                }
            }
            _ => {
                ctx.renderer
                    .write_line(&format!("unknown mode: {}", parts[1]), c_error())?;
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_toggle(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line("usage: /toggle <feature> [on|off]", c_agent())?;
        ctx.renderer.write_line("features:", c_agent())?;
        ctx.renderer.write_line(
            &format!(
                "  todo  {}",
                if *ctx.todo_tools_enabled { "on" } else { "off" }
            ),
            c_result(),
        )?;
    } else {
        let new_state = match parts.get(2).copied() {
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                ctx.renderer
                    .write_line(&format!("invalid: '{}', use on or off", other), c_error())?;
                return Ok(());
            }
            None => !*ctx.todo_tools_enabled,
        };
        if new_state == *ctx.todo_tools_enabled {
            ctx.renderer.write_line(
                &format!(
                    "todo tools already {}",
                    if new_state { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        } else {
            *ctx.todo_tools_enabled = new_state;
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
            ctx.renderer.write_line(
                &format!(
                    "todo tools: {}",
                    if *ctx.todo_tools_enabled { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_prompt(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let mut sorted: Vec<String> = ctx.context.prompts.keys().cloned().collect();
    sorted.sort();
    if parts.len() < 2 {
        if sorted.is_empty() {
            ctx.renderer.write_line("no prompts available", c_agent())?;
        } else {
            let current = ctx
                .context
                .current_prompt_name
                .as_deref()
                .unwrap_or("(none)")
                .to_string();
            ctx.renderer.write_line(
                &format!("available prompts (current: {}):", current),
                c_agent(),
            )?;
            let max_name = sorted.iter().map(|n| n.len()).max().unwrap_or(0);
            for name in &sorted {
                let desc = ctx
                    .context
                    .prompts
                    .get(name)
                    .and_then(|p| p.description.as_deref())
                    .map(|s| s.to_string());
                match desc {
                    Some(d) => ctx.renderer.write_line(
                        &format!("  {:<width$}  {}", name, d, width = max_name),
                        c_result(),
                    )?,
                    None => ctx
                        .renderer
                        .write_line(&format!("  {}", name), c_result())?,
                }
            }
            ctx.renderer.write_line("", c_agent())?;
            ctx.renderer
                .write_line("usage: /prompt <name>  |  /prompt default", c_result())?;
        }
    } else if parts[1] == "default" && !ctx.context.prompts.contains_key("default") {
        if ctx.context.current_prompt.is_none() {
            ctx.renderer
                .write_line("no active prompt to clear", c_agent())?;
        } else {
            ctx.context.current_prompt = None;
            ctx.context.current_prompt_name = None;
            ctx.context.current_prompt_deny_tools.clear();
            crate::permission::apply_prompt_deny(
                ctx.permission,
                &ctx.context.current_prompt_deny_tools,
            );
            ctx.session.current_prompt_name = None;
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
        }
    } else {
        let name = parts[1].trim().to_string();
        if let Some(p) = ctx.context.prompts.get(&name).cloned() {
            ctx.context.current_prompt = Some(p.body.clone());
            ctx.context.current_prompt_name = Some(name.clone());
            ctx.context.current_prompt_deny_tools = p.deny_tools.clone();
            crate::permission::apply_prompt_deny(
                ctx.permission,
                &ctx.context.current_prompt_deny_tools,
            );
            ctx.session.current_prompt_name = Some(name.clone());
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
            ctx.renderer
                .write_line(&format!("active prompt: {}", name), c_agent())?;
        } else {
            ctx.renderer
                .write_line(&format!("unknown prompt: '{}'", name), c_error())?;
            if !sorted.is_empty() {
                ctx.renderer.write_line("available prompts:", c_agent())?;
                for p in &sorted {
                    ctx.renderer.write_line(&format!("  {}", p), c_result())?;
                }
            }
        }
    }
    Ok(())
}

/// Rebuild the active agent in place from current `session.model` + `context`
/// state. Mirrors the `build_agent` rebuild used by `/model` and `/prompt`.
async fn rebuild_agent(ctx: &mut SlashCtx<'_>) {
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
}

/// Resolve an agent's `model` field to a model string for the current client.
/// If it names a `providers` alias carrying a `model`, use that model; else
/// treat the value itself as the model name. `None` → keep the current model.
///
/// NOTE (dirge-ykeu): this is a *same-client* resolution. If the alias points
/// at a different backend (provider_type/base_url/api_key), only its model
/// string is taken — cross-provider client switching is a later phase. The
/// built-in role routing (critic/escalation) already does full per-role
/// clients and is unaffected.
fn resolve_agent_model(cfg: &crate::config::Config, model: Option<&str>) -> Option<String> {
    let m = model?;
    if let Some(providers) = &cfg.providers
        && let Some(entry) = providers.get(m)
        && let Some(model_str) = &entry.model
    {
        return Some(model_str.clone());
    }
    Some(m.to_string())
}

/// `/agent` — list / switch / clear the active agent profile (dirge-ykeu).
///
///   /agent              list profiles, marking the active one
///   /agent <name>       activate a profile (its prompt + model + tool policy)
///   /agent off          deactivate (clear the profile's prompt + tool deny)
///
/// Applying a profile reuses the `/prompt` machinery (system prompt +
/// `deny_tools` enforced at the permission layer) and the `/model` machinery
/// (same-client model swap + agent rebuild). The built-in critic / role
/// routing is untouched — this only drives the main loop's active persona.
pub(super) async fn cmd_agent(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    // Bare `/agent`, or the `/agents` alias → list (delegated to cmd_misc).
    if parts.len() < 2 || parts[0] == "/agents" {
        return super::cmd_misc::cmd_agents(ctx, parts).await;
    }

    let arg = parts[1].trim();
    if matches!(arg, "off" | "none" | "default") {
        if ctx.context.current_agent.is_none() {
            ctx.renderer
                .write_line("no active agent to clear", c_agent())?;
            return Ok(());
        }
        ctx.context.current_agent = None;
        ctx.context.current_prompt = None;
        ctx.context.current_prompt_name = None;
        ctx.context.current_prompt_deny_tools.clear();
        crate::permission::apply_prompt_deny(
            ctx.permission,
            &ctx.context.current_prompt_deny_tools,
        );
        rebuild_agent(ctx).await;
        ctx.renderer.write_line(
            "agent deactivated (model unchanged — use /model to switch back)",
            c_agent(),
        )?;
        return Ok(());
    }

    let Some(def) = ctx.context.agent_defs.get(arg).cloned() else {
        ctx.renderer
            .write_line(&format!("unknown agent: '{}'", arg), c_error())?;
        if !ctx.context.agent_defs.is_empty() {
            ctx.renderer.write_line("available agents:", c_agent())?;
            for a in ctx.context.agent_defs.iter() {
                ctx.renderer
                    .write_line(&format!("  {}", a.name), c_result())?;
            }
        }
        return Ok(());
    };

    // Prompt: only override when the profile defines one (otherwise keep the
    // user's active prompt — the profile may only change model/tools).
    if let Some(body) = &def.prompt {
        ctx.context.current_prompt = Some(body.clone());
        ctx.context.current_prompt_name = Some(def.name.clone());
    }
    // Tool policy → deny-list at the permission layer.
    ctx.context.current_prompt_deny_tools = def
        .tools
        .to_deny_list(crate::agent::tools::BUILTIN_TOOL_NAMES);
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);
    // Model: same-client swap (see resolve_agent_model).
    let resolved_model = resolve_agent_model(ctx.cfg, def.model.as_deref());
    if let Some(model) = &resolved_model {
        ctx.session.model = CompactString::new(model.as_str());
        ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
        ctx.session.context_window = ctx.cfg.resolve_context_window(ctx.session.model.as_str());
    }
    ctx.context.current_agent = Some(def.name.clone());
    rebuild_agent(ctx).await;

    // Report what changed.
    let mut summary = format!("active agent: {}", def.name);
    if let Some(m) = &resolved_model {
        summary.push_str(&format!("  · model {}", m));
    }
    ctx.renderer.write_line(&summary, c_agent())?;
    if !ctx.context.current_prompt_deny_tools.is_empty() {
        ctx.renderer.write_line(
            &format!(
                "  denied tools: {}",
                ctx.context.current_prompt_deny_tools.join(", ")
            ),
            c_result(),
        )?;
    }
    Ok(())
}

pub(super) async fn cmd_regen_prompts(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match crate::context::prompts::regen() {
        Ok(()) => {
            ctx.context.prompts = crate::context::prompts::load();
            if let Some(name) = ctx.context.current_prompt_name.clone()
                && let Some(p) = ctx.context.prompts.get(&name).cloned()
            {
                ctx.context.current_prompt = Some(p.body.clone());
                ctx.context.current_prompt_deny_tools = p.deny_tools.clone();
                crate::permission::apply_prompt_deny(
                    ctx.permission,
                    &ctx.context.current_prompt_deny_tools,
                );
            }
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
            ctx.renderer.write_line(
                "default prompts regenerated; agent rebuilt with refreshed prompt",
                c_agent(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed to regenerate prompts: {}", e), c_error())?;
        }
    }
    Ok(())
}
