//! /agents handler (shared by /agent list).

use crate::config::ConfigRole;
use crate::context::agent_defs::ToolPolicy;
use crate::ui::slash::{SlashCtx, c_agent, c_result};

pub(crate) async fn cmd_agents(ctx: &mut SlashCtx<'_>, _parts: &[&str]) -> anyhow::Result<()> {
    let reg = &ctx.context.agent_defs;
    if reg.is_empty() {
        ctx.renderer.write_line(
            "no agent profiles defined — add .dirge/agents/<name>.md or a config.json \"agents\" entry",
            c_agent(),
        )?;
    } else {
        let active = ctx.context.current_agent.clone();
        ctx.renderer
            .write_line(&format!("agent profiles ({}):", reg.len()), c_agent())?;
        for a in reg.iter() {
            let model = a.model.as_deref().unwrap_or("(default model)");
            let tools = match &a.tools {
                ToolPolicy::All => "all tools".to_string(),
                ToolPolicy::Allow(v) => format!("allow: {}", v.join(", ")),
                ToolPolicy::Deny(v) => format!("deny: {}", v.join(", ")),
            };
            let marker = if active.as_deref() == Some(a.name.as_str()) {
                "* "
            } else {
                "  "
            };
            ctx.renderer.write_line(
                &format!(
                    "{}{}  [{}]  {}  ·  {}",
                    marker,
                    a.name,
                    a.source.label(),
                    model,
                    tools
                ),
                c_result(),
            )?;
            if let Some(d) = &a.description {
                ctx.renderer
                    .write_line(&format!("      {}", d), c_result())?;
            }
        }
        ctx.renderer
            .write_line("usage: /agent <name>  |  /agent off", c_agent())?;

        let roles: [(&str, ConfigRole); 6] = [
            ("review", ConfigRole::Review),
            ("escalation", ConfigRole::Escalation),
            ("summarization", ConfigRole::Summarization),
            ("subagent", ConfigRole::Subagent),
            ("critic", ConfigRole::Critic),
            ("approval", ConfigRole::Approval),
        ];
        let configured: Vec<(&str, String, Option<String>)> = roles
            .iter()
            .filter_map(|(label, role)| {
                let explicit = match role {
                    ConfigRole::Review => ctx.cfg.review_provider.is_some(),
                    ConfigRole::Escalation => ctx.cfg.escalation_provider.is_some(),
                    ConfigRole::Summarization => ctx.cfg.summarization_provider.is_some(),
                    ConfigRole::Subagent => ctx.cfg.subagent_provider.is_some(),
                    ConfigRole::Critic => ctx.cfg.critic_provider.is_some(),
                    ConfigRole::Approval => ctx.cfg.approval_provider.is_some(),
                    ConfigRole::Default => true,
                };
                if !explicit {
                    return None;
                }
                ctx.cfg
                    .resolve_role(*role)
                    .map(|(alias, entry)| (*label, alias, entry.model))
            })
            .collect();

        if configured.is_empty() {
            ctx.renderer.write_line(
                "built-in roles: all on the default model (set *_provider in config.json to route a role elsewhere)",
                c_agent(),
            )?;
        } else {
            ctx.renderer
                .write_line("built-in role routing:", c_agent())?;
            for (label, alias, model) in configured {
                let model = model.as_deref().unwrap_or("(provider default)");
                ctx.renderer
                    .write_line(&format!("  {label:<14} → {alias}  ·  {model}"), c_result())?;
            }
        }
    }
    Ok(())
}
