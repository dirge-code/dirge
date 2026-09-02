//! /model, /reasoning handlers.

use std::collections::HashMap;

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use compact_str::CompactString;

use crate::config::ProviderEntry;
use crate::provider::{apply_model_route, resolve_model_route};
use crate::ui::slash::cmd::agent;
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

/// Build the sorted list of models the config pins, one per provider that
/// sets a `model`. Each row is `(model, provider-alias, is_active)`, sorted
/// by model then alias for stable output. `current` is the active session
/// model, used to flag the selected row. (issue #492 — `/model` listed only
/// the current model with nothing to switch to.)
fn configured_models(
    providers: &HashMap<String, ProviderEntry>,
    current: &str,
) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = providers
        .iter()
        .filter_map(|(alias, entry)| {
            entry
                .model
                .as_ref()
                .map(|m| (m.clone(), alias.clone(), m == current))
        })
        .collect();
    rows.sort();
    rows
}

pub(crate) async fn cmd_model(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line(&format!("current model: {}", ctx.session.model), c_agent())?;

        // List the models pinned across the configured providers so there's
        // something to switch to, marking the active one (issue #492).
        let providers = ctx.cfg.providers_map();
        let rows = configured_models(&providers, ctx.session.model.as_str());
        if rows.is_empty() {
            ctx.renderer.write_line(
                "no models pinned in `providers` config — /model <id> switches to any model your provider supports",
                c_result(),
            )?;
        } else {
            ctx.renderer.write_line("configured models:", c_agent())?;
            for (model, alias, is_active) in &rows {
                let marker = if *is_active { "* " } else { "  " };
                ctx.renderer
                    .write_line(&format!("{marker}{model}  ·  {alias}"), c_result())?;
            }
            ctx.renderer
                .write_line("usage: /model <id> to switch", c_agent())?;
        }
    } else {
        let new_model = CompactString::new(parts[1].trim());

        // Decide whether the chosen id routes to a *different* provider, then
        // apply the model AND any client swap as one operation — otherwise we'd
        // POST that id to whatever endpoint happened to be live and 400/404.
        // Same-provider and unclassifiable ids keep the current client.
        let old_ctx = ctx.session.context_window;
        let route = resolve_model_route(ctx.cfg, ctx.session.provider.as_str(), new_model.as_str());
        // GH #831: read before the route is consumed. An id matching no
        // configured alias and no known model family still applies — that
        // permissive fallthrough is what lets a brand-new model id work before
        // dirge knows it — but `/model off` reporting a clean switch onto a
        // string no endpoint serves is what the report is about.
        let unrecognized = matches!(
            route,
            crate::provider::ModelRoute::Active {
                recognized: false,
                ..
            }
        );
        // A refusal leaves the session untouched: renaming onto a client that
        // can't serve the id would just point the session at a model that can't
        // work. Keep it functional and say how to make the id routable.
        let switched_to = match apply_model_route(ctx.cfg, ctx.client, ctx.session, route) {
            Ok(switched_to) => switched_to,
            Err(refusal) => {
                ctx.renderer.write_line(
                    &format!(
                        "{refusal} Keeping model '{}' on '{}'.",
                        ctx.session.model, ctx.session.provider,
                    ),
                    c_error(),
                )?;
                return Ok(());
            }
        };

        agent::rebuild_agent(ctx).await;
        let new_ctx = ctx.session.context_window;
        let provider_note = switched_to
            .as_deref()
            .map(|a| format!("  ·  {a}"))
            .unwrap_or_default();
        // GH #825: report the model the session actually landed on, not the
        // raw argument — `/model <provider-alias>` resolves to the alias's
        // pinned model, so echoing the argument would print the alias string
        // as if it were a model id. Identical on every non-alias path.
        // The clause asserts RECOGNITION, not validity: only the provider
        // knows whether an id is servable, and the two come apart for a
        // new-but-valid id (`claude-opus-6` on release day). So it says what
        // dirge knows and leaves the consequence conditional.
        let unknown_note = if unrecognized {
            "  (unrecognised — your provider may not serve it)"
        } else {
            ""
        };
        ctx.renderer.write_line(
            &format!(
                "switched to model: {}{provider_note}{unknown_note}",
                ctx.session.model
            ),
            c_agent(),
        )?;
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

pub(crate) async fn cmd_reasoning(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
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

/// What a `/effort <arg>` argument asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffortArg {
    /// Drop the session override and fall back to the provider config default.
    Clear,
    /// Pin an explicit level for the session. `Off` is one of these — it
    /// disables reasoning, which is NOT the same as clearing the override.
    Set(crate::agent::agent_loop::types::ThinkingLevel),
    /// Not a level and not a clear word.
    Unknown,
}

/// Classify a `/effort` argument.
///
/// `off` names a level, so it must set `ThinkingLevel::Off` rather than clear.
/// It used to clear, which meant a provider with `effort` in config re-seeded
/// that default on the way out and reasoning could never actually be turned
/// off from the UI. Clearing is spelled `default` (or `clear`).
///
/// Lowercased up front so the clear words and the level names agree on case —
/// `from_effort_str` lowercases internally, so a case-sensitive check here made
/// `/effort OFF` and `/effort off` do opposite things.
pub(crate) fn parse_effort_arg(raw: &str) -> EffortArg {
    use crate::agent::agent_loop::types::ThinkingLevel;
    let raw = raw.trim().to_ascii_lowercase();
    if matches!(raw.as_str(), "default" | "clear") {
        return EffortArg::Clear;
    }
    // `none` is a UI synonym for `off`, not a wire name — `from_effort_str`
    // deliberately only knows the wire names. It used to be a clear word.
    if raw == "none" {
        return EffortArg::Set(ThinkingLevel::Off);
    }
    match ThinkingLevel::from_effort_str(&raw) {
        Some(level) => EffortArg::Set(level),
        None => EffortArg::Unknown,
    }
}

/// `/effort [off|minimal|low|medium|high|xhigh|max|default]` — set the reasoning effort the
/// next turn runs at. With no arg, reports the active level (the live
/// override, else the per-provider config default, else the loop default
/// `off`). The override is session-scoped (not persisted) and survives
/// `/model` swaps and rebuilds. `off` disables reasoning; `default` clears the override. `xhigh` and `max`
/// are distinct tiers on OpenAI and Anthropic; providers that lack `xhigh`
/// (DeepSeek, GLM-5.3) fold `xhigh` up to `max`.
pub(crate) async fn cmd_effort(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    use crate::agent::agent_loop::types::ThinkingLevel;

    if parts.len() < 2 {
        let active = ctx
            .session
            .effort_override
            .or(ctx.agent.reasoning())
            .unwrap_or(ThinkingLevel::Off);
        let label = active.effort_label();
        let source = if ctx.session.effort_override.is_some() {
            "(session override)"
        } else if ctx.agent.reasoning().is_some() {
            "(provider config)"
        } else {
            "(default)"
        };
        ctx.renderer
            .write_line(&format!("current effort: {label} {source}"), c_agent())?;
        ctx.renderer.write_line(
            "usage: /effort <off|minimal|low|medium|high|xhigh|max|default>",
            c_result(),
        )?;
        return Ok(());
    }

    let level = match parse_effort_arg(parts[1]) {
        // Re-resolve from config so the agent reflects the clear immediately
        // (not only on the next rebuild). Keyed by the config ALIAS from the
        // session, not `agent.provider_name()` — that returns the provider
        // TYPE ("anthropic"), which misses any entry named anything else.
        EffortArg::Clear => {
            ctx.session.effort_override = None;
            let config_default = ctx
                .cfg
                .providers_map()
                .get(ctx.session.provider.as_str())
                .and_then(|e| e.resolved_effort().ok().flatten());
            ctx.agent.set_reasoning(config_default);
            let now = config_default.unwrap_or(ThinkingLevel::Off).effort_label();
            ctx.renderer
                .write_line(&format!("effort override cleared — now {now}"), c_agent())?;
            return Ok(());
        }
        EffortArg::Unknown => {
            let raw = parts[1].trim();
            ctx.renderer.write_line(
                &format!(
                    "unknown effort `{raw}` — expected \
                     off/minimal/low/medium/high/xhigh/max, or `default` to clear"
                ),
                c_error(),
            )?;
            return Ok(());
        }
        EffortArg::Set(level) => level,
    };

    ctx.session.effort_override = Some(level);
    ctx.agent.set_reasoning(Some(level));
    ctx.renderer.write_line(
        &format!(
            "effort set to {} — applies on the next turn",
            level.effort_label()
        ),
        c_agent(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(model: Option<&str>) -> ProviderEntry {
        ProviderEntry {
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn lists_pinned_models_sorted_and_flags_active() {
        let providers = HashMap::from([
            (
                "openrouter".to_string(),
                entry(Some("deepseek/deepseek-v4")),
            ),
            ("anthropic".to_string(), entry(Some("claude-opus-4"))),
            // No model pinned → excluded from the list.
            ("local-vllm".to_string(), entry(None)),
        ]);
        let rows = configured_models(&providers, "claude-opus-4");
        assert_eq!(
            rows,
            vec![
                ("claude-opus-4".to_string(), "anthropic".to_string(), true),
                (
                    "deepseek/deepseek-v4".to_string(),
                    "openrouter".to_string(),
                    false,
                ),
            ],
            "sorted by model; the active one is flagged; model-less providers dropped",
        );
    }

    #[test]
    fn empty_when_no_providers_pin_a_model() {
        let providers = HashMap::from([("local-vllm".to_string(), entry(None))]);
        assert!(configured_models(&providers, "anything").is_empty());
        assert!(configured_models(&HashMap::new(), "anything").is_empty());
    }

    #[test]
    fn same_model_under_two_aliases_flags_both() {
        let providers = HashMap::from([
            ("a".to_string(), entry(Some("m"))),
            ("b".to_string(), entry(Some("m"))),
        ]);
        let rows = configured_models(&providers, "m");
        assert!(rows.iter().all(|(_, _, active)| *active));
        assert_eq!(rows.len(), 2);
    }
}

#[cfg(test)]
mod effort_arg_tests {
    use super::*;
    use crate::agent::agent_loop::types::ThinkingLevel;

    /// `off` is a real level, not a request to clear the override. With
    /// `effort` set in provider config, clearing re-seeds that default —
    /// so if `off` cleared, there would be no way to turn reasoning off.
    #[test]
    fn off_disables_rather_than_clearing() {
        assert_eq!(
            parse_effort_arg("off"),
            EffortArg::Set(ThinkingLevel::Off),
            "`off` must disable reasoning, not fall back to the config default",
        );
        assert_eq!(parse_effort_arg("none"), EffortArg::Set(ThinkingLevel::Off));
    }

    /// Only `default`/`clear` drop the session override.
    #[test]
    fn default_and_clear_drop_the_override() {
        assert_eq!(parse_effort_arg("default"), EffortArg::Clear);
        assert_eq!(parse_effort_arg("clear"), EffortArg::Clear);
    }

    /// `/effort OFF` and `/effort off` used to do opposite things: the
    /// clear-check was case-sensitive while `from_effort_str` lowercases.
    #[test]
    fn case_does_not_change_meaning() {
        for (upper, lower) in [
            ("OFF", "off"),
            ("Default", "default"),
            ("HIGH", "high"),
            ("XHigh", "xhigh"),
            ("MAX", "max"),
        ] {
            assert_eq!(
                parse_effort_arg(upper),
                parse_effort_arg(lower),
                "`{upper}` and `{lower}` must mean the same thing",
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(parse_effort_arg("  high  "), parse_effort_arg("high"));
    }

    #[test]
    fn every_level_round_trips_through_its_label() {
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ] {
            assert_eq!(
                parse_effort_arg(level.effort_label()),
                EffortArg::Set(level),
                "{level:?} must survive a round trip through its own label",
            );
        }
    }

    #[test]
    fn unknown_values_are_rejected() {
        assert_eq!(parse_effort_arg("turbo"), EffortArg::Unknown);
        assert_eq!(parse_effort_arg(""), EffortArg::Unknown);
    }
}
