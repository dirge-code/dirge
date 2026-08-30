//! /agent <name> — activate a named agent profile.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::context::agent_defs::resolve_model_alias as resolve_agent_model;
use crate::provider::{ModelRoute, apply_model_route, resolve_model_route};
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

use super::rebuild_agent;

pub(crate) async fn cmd_agent_switch(ctx: &mut SlashCtx<'_>, arg: &str) -> anyhow::Result<()> {
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

    if ctx.context.agent_layer.is_none() {
        // Capture the PAIR, not just the model: `/agent off` has to put the
        // session back on the client it left, and the id alone can't say which
        // provider that was (dirge-fhr5).
        ctx.context.route_before_agent = Some(ModelRoute::pinned(
            ctx.session.provider.to_string(),
            ctx.session.model.to_string(),
        ));
    }
    ctx.context.set_agent_layer(def.clone());
    crate::permission::apply_prompt_deny(ctx.permission, &ctx.context.current_prompt_deny_tools);

    // A profile pinning a model from another family has to move the CLIENT too
    // — renaming the model on the live client sent e.g. `glm-5.2` to a
    // ChatGPT/Codex endpoint and 400'd every turn (dirge-fhr5, the `/agent`
    // sibling of #711). Same routing decision `/model` makes.
    let resolved_model = resolve_agent_model(ctx.cfg, def.model.as_deref());
    let mut switched_to = None;
    if let Some(model) = &resolved_model {
        let route = resolve_model_route(ctx.cfg, ctx.session.provider.as_str(), model);
        match apply_model_route(ctx.cfg, ctx.client, ctx.session, route) {
            Ok(alias) => switched_to = alias,
            // Keep the profile active (its prompt and tool policy are still
            // valid) but leave the session on a model that works, mirroring
            // `/model`'s refusal.
            Err(refusal) => ctx.renderer.write_line(
                &format!(
                    "agent '{}': {refusal} Keeping model '{}' on '{}'.",
                    def.name, ctx.session.model, ctx.session.provider,
                ),
                c_error(),
            )?,
        }
    }

    // Apply the profile's `reasoning` frontmatter (GH #828) — parsed since
    // the key was introduced but never consumed. It layers exactly like the
    // model: the profile's level wins at activation (over any live `/effort`
    // override), a later `/effort` overrides it as it would any current
    // level, and `/agent off` restores what was captured here. Applied even
    // when the model route was refused above — the profile stays active in
    // that case, so its reasoning does too. Written to
    // `session.effort_override` (not `set_reasoning` directly) so the
    // `rebuild_agent` below installs it on the live agent and every later
    // rebuild keeps it sticky, the same way `/effort` survives `/model`.
    let mut applied_effort = None;
    if let Some(raw) = def.reasoning.as_deref() {
        match apply_profile_reasoning(
            raw,
            &mut ctx.session.effort_override,
            &mut ctx.context.effort_before_agent,
        ) {
            Ok(level) => applied_effort = Some(level),
            // Fail soft: warn and leave the session's effort alone, the same
            // way `/effort` treats an unknown level. Never abort the switch —
            // the rest of the profile is still valid.
            Err(msg) => ctx
                .renderer
                .write_line(&format!("agent '{}': {msg}", def.name), c_error())?,
        }
    }

    rebuild_agent(ctx).await;

    let mut summary = format!("active agent: {}", def.name);
    if resolved_model.is_some() {
        summary.push_str(&format!("  · model {}", ctx.session.model));
    }
    if let Some(alias) = &switched_to {
        summary.push_str(&format!("  ·  {alias}"));
    }
    if let Some(level) = applied_effort {
        summary.push_str(&format!("  · effort {}", level.effort_label()));
    }
    ctx.renderer.write_line(&summary, c_agent())?;
    Ok(())
}

/// Apply an agent profile's `reasoning` frontmatter value to the session's
/// effort override (GH #828), capturing the pre-profile override on the
/// FIRST profile application so `/agent off` can restore it. The capture
/// guard is `effort_before_agent`, not the agent layer: profile A without a
/// `reasoning` key followed by profile B with one must capture at B, and
/// A-with-B-with must keep A's capture (the pre-agent value) — mirroring
/// how `route_before_agent` holds the pre-agent route across profile hops.
///
/// Returns the level applied, or `Err` with a warning message (worded like
/// `/effort`'s unknown-level error) when `raw` is not a recognised level —
/// in which case NOTHING is touched: no capture, no override change.
pub(crate) fn apply_profile_reasoning(
    raw: &str,
    effort_override: &mut Option<crate::agent::agent_loop::types::ThinkingLevel>,
    effort_before_agent: &mut Option<Option<crate::agent::agent_loop::types::ThinkingLevel>>,
) -> Result<crate::agent::agent_loop::types::ThinkingLevel, String> {
    use crate::agent::agent_loop::types::ThinkingLevel;
    let Some(level) = ThinkingLevel::from_effort_str(raw) else {
        return Err(format!(
            "unknown reasoning `{}` — expected off/minimal/low/medium/high/xhigh/max; \
             leaving effort unchanged",
            raw.trim(),
        ));
    };
    if effort_before_agent.is_none() {
        *effort_before_agent = Some(*effort_override);
    }
    *effort_override = Some(level);
    Ok(level)
}

#[cfg(test)]
mod tests {
    use super::super::clear::restore_profile_reasoning;
    use super::apply_profile_reasoning;
    use crate::agent::agent_loop::types::ThinkingLevel;

    // GH #828: a profile's `reasoning` is applied on activation and the
    // pre-profile state (no override) is captured for `/agent off`.
    #[test]
    fn profile_reasoning_is_applied_and_prior_state_captured() {
        let mut over = None;
        let mut before = None;
        let applied = apply_profile_reasoning("low", &mut over, &mut before);
        assert_eq!(applied, Ok(ThinkingLevel::Low));
        assert_eq!(over, Some(ThinkingLevel::Low));
        assert_eq!(before, Some(None), "must capture 'no prior override'");
    }

    // Precedence at activation: the profile wins over a live `/effort`
    // override, the same way its model wins over a `/model` choice — and
    // the displaced override is what `/agent off` will restore.
    #[test]
    fn profile_reasoning_wins_over_live_effort_override_at_activation() {
        let mut over = Some(ThinkingLevel::Max);
        let mut before = None;
        let applied = apply_profile_reasoning("low", &mut over, &mut before);
        assert_eq!(applied, Ok(ThinkingLevel::Low));
        assert_eq!(over, Some(ThinkingLevel::Low));
        assert_eq!(before, Some(Some(ThinkingLevel::Max)));
    }

    // `/agent off` restores the pre-profile override.
    #[test]
    fn restore_returns_the_pre_profile_override() {
        let mut over = Some(ThinkingLevel::High);
        let mut before = None;
        apply_profile_reasoning("off", &mut over, &mut before).unwrap();
        restore_profile_reasoning(&mut over, &mut before);
        assert_eq!(over, Some(ThinkingLevel::High));
        assert_eq!(before, None, "capture must be consumed");
    }

    // `/agent off` after a profile applied over NO prior override restores
    // "no override" (the rebuild then re-seeds the provider config default).
    #[test]
    fn restore_returns_no_override_when_there_was_none_before() {
        let mut over = None;
        let mut before = None;
        apply_profile_reasoning("medium", &mut over, &mut before).unwrap();
        restore_profile_reasoning(&mut over, &mut before);
        assert_eq!(over, None);
        assert_eq!(before, None);
    }

    // A `/effort` issued WHILE the profile is active is discarded by
    // `/agent off` in favour of the pre-profile value — mirroring how the
    // route restore discards a mid-profile `/model`.
    #[test]
    fn restore_discards_a_mid_profile_effort_change() {
        let mut over = Some(ThinkingLevel::Medium);
        let mut before = None;
        apply_profile_reasoning("low", &mut over, &mut before).unwrap();
        over = Some(ThinkingLevel::Xhigh); // user ran `/effort xhigh` mid-profile
        restore_profile_reasoning(&mut over, &mut before);
        assert_eq!(over, Some(ThinkingLevel::Medium));
    }

    // A profile that omits `reasoning` never calls apply — so on `/agent
    // off` there is no capture, and restore must change NOTHING (a
    // key-less profile leaves effort alone in both directions).
    #[test]
    fn restore_without_a_capture_is_a_no_op() {
        let mut over = Some(ThinkingLevel::Max);
        let mut before = None;
        restore_profile_reasoning(&mut over, &mut before);
        assert_eq!(over, Some(ThinkingLevel::Max));
    }

    // An invalid value fails soft: warn (the Err), touch nothing, never
    // abort the switch.
    #[test]
    fn invalid_reasoning_value_changes_nothing() {
        let mut over = Some(ThinkingLevel::High);
        let mut before = None;
        let res = apply_profile_reasoning("turbo", &mut over, &mut before);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("unknown reasoning `turbo`"));
        assert_eq!(over, Some(ThinkingLevel::High), "override untouched");
        assert_eq!(before, None, "no capture on failure");
    }

    // Hopping profile A -> profile B keeps A's capture: the value `/agent
    // off` restores is the PRE-AGENT one, exactly as `route_before_agent`
    // holds the pre-agent route across profile hops.
    #[test]
    fn profile_hop_keeps_the_pre_agent_capture() {
        let mut over = Some(ThinkingLevel::Minimal);
        let mut before = None;
        apply_profile_reasoning("high", &mut over, &mut before).unwrap();
        apply_profile_reasoning("max", &mut over, &mut before).unwrap();
        assert_eq!(over, Some(ThinkingLevel::Max));
        assert_eq!(before, Some(Some(ThinkingLevel::Minimal)));
        restore_profile_reasoning(&mut over, &mut before);
        assert_eq!(over, Some(ThinkingLevel::Minimal));
    }

    // All seven `/effort` levels are accepted — the profile key must never
    // diverge from `/effort`'s vocabulary (they share the parser).
    #[test]
    fn all_seven_effort_levels_parse() {
        for (raw, want) in [
            ("off", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::Xhigh),
            ("max", ThinkingLevel::Max),
        ] {
            let mut over = None;
            let mut before = None;
            assert_eq!(
                apply_profile_reasoning(raw, &mut over, &mut before),
                Ok(want)
            );
        }
    }
}
