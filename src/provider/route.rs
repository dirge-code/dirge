//! Model routing: deciding *which client* runs a model id, and applying that
//! decision to the live session.
//!
//! A model change and a client change are ONE operation. Every place that set
//! `session.model` without also rebuilding the client sent the new id to
//! whatever endpoint happened to be live — `glm-5.2` POSTed to a ChatGPT/Codex
//! endpoint 400s on the first turn (#711, dirge-fhr5). This module is the
//! single place that keeps the two coupled, so a new caller gets the routing
//! for free instead of hand-rolling a fourth copy of it.
//!
//! Three layers, used in whatever combination a caller needs:
//!   1. [`resolve_model_route`] — pure decision: id + active provider → route.
//!      Wraps [`super::resolve_model_switch`], the decision `/model` makes.
//!   2. [`swap_client_for_route`] — apply the route to the live `AnyClient`.
//!      For callers that must defer the session update (the plugin
//!      `prepare-next-run` swap resolves its hook chain in between).
//!   3. [`apply_model_route`] — 2 plus the session's model / provider /
//!      context-window fields. What an interactive command wants.
//!
//! A route can also be [`ModelRoute::pinned`] rather than inferred, for the one
//! case inference can't serve: RESTORING a `(provider, model)` pair that was
//! live earlier. Re-deriving it from the id can pick a different alias of the
//! same family, or refuse outright when the original provider was a built-in
//! with no `providers` entry.

use crate::config::Config;
use crate::session::Session;

use super::AnyClient;

/// Where a model id should be built: under which name, on whose client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRoute {
    /// Build on the ACTIVE client — the id belongs to the active provider's
    /// family, or carries no cross-provider signal.
    Active(String),
    /// Build on `alias`'s client: the id's family differs from the active
    /// client's and a provider of that family is configured.
    Provider { alias: String, model: String },
    /// The id's family has no configured provider. There is nowhere better to
    /// route, so callers either refuse (interactive: keep the session working)
    /// or fall back to the active client (startup: keep prior behavior).
    Unroutable { model: String, family: String },
}

impl ModelRoute {
    /// The model id this route carries, whatever the variant.
    pub fn model(&self) -> &str {
        match self {
            ModelRoute::Active(model)
            | ModelRoute::Provider { model, .. }
            | ModelRoute::Unroutable { model, .. } => model,
        }
    }

    /// A route the caller already knows, bypassing inference. Used to RESTORE a
    /// `(provider, model)` pair that was live earlier (`/agent off`): the pair
    /// was valid when captured, but re-inferring it from the id alone can land
    /// on a different alias of the same family, or refuse when the original
    /// provider has no `providers` entry to infer from.
    pub fn pinned(alias: impl Into<String>, model: impl Into<String>) -> Self {
        ModelRoute::Provider {
            alias: alias.into(),
            model: model.into(),
        }
    }
}

/// Why a route could not be applied. Carries the parts so callers can word the
/// message in their own voice; [`std::fmt::Display`] gives the canonical
/// sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteRefusal {
    /// No configured provider serves the id's family.
    NoProviderForFamily { model: String, family: String },
    /// The target provider's client failed to build (bad config, missing key).
    ProviderBuildFailed {
        alias: String,
        model: String,
        error: String,
    },
}

impl std::fmt::Display for RouteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteRefusal::NoProviderForFamily { model, family } => write!(
                f,
                "'{model}' matches the {family} model family, but no {family} provider is \
                 configured. Add a provider of type {family} to config.json to use it."
            ),
            RouteRefusal::ProviderBuildFailed {
                alias,
                model,
                error,
            } => write!(
                f,
                "'{model}' routes to provider '{alias}', which failed to build ({error})."
            ),
        }
    }
}

/// Decide where `model` should run, given the currently active provider alias.
/// Pure — no client is built and nothing is mutated.
pub fn resolve_model_route(cfg: &Config, active_provider: &str, model: &str) -> ModelRoute {
    use super::ModelSwitch;
    let model = model.to_string();
    match super::resolve_model_switch(&cfg.providers_map(), active_provider, &model) {
        ModelSwitch::Keep => ModelRoute::Active(model),
        ModelSwitch::Switch(alias) => ModelRoute::Provider { alias, model },
        ModelSwitch::NoProviderForFamily(family) => ModelRoute::Unroutable { model, family },
    }
}

/// Build the client a route targets. The shared primitive under
/// [`swap_client_for_route`] (which installs it live) and
/// [`super::resolve_profile_model`] (which caches it to build detached models).
pub fn build_route_client(
    cfg: &Config,
    alias: &str,
    model: &str,
) -> Result<AnyClient, RouteRefusal> {
    super::create_client_with_auth(alias, None, &cfg.providers_map(), cfg.auth).map_err(|e| {
        RouteRefusal::ProviderBuildFailed {
            alias: alias.to_string(),
            model: model.to_string(),
            error: e.to_string(),
        }
    })
}

/// Point the live `client` at `route`'s provider, given the alias it is
/// currently on. `Ok(Some(alias))` when the client was rebuilt, `Ok(None)` when
/// the active client already serves the route. The client is left untouched on
/// `Err`.
pub fn swap_client_for_route(
    cfg: &Config,
    client: &mut AnyClient,
    active_provider: &str,
    route: &ModelRoute,
) -> Result<Option<String>, RouteRefusal> {
    match route {
        ModelRoute::Active(_) => Ok(None),
        // Already on that provider — this is a rename, not a swap. Rebuilding
        // would be wasted work and can fail outright: the rebuild re-resolves
        // credentials from config/env, so a session launched with `--api-key`
        // has nothing to rebuild from. Restores (`/agent off`) name a provider
        // explicitly on every deactivation, so this is a hot path.
        ModelRoute::Provider { alias, .. } if alias.eq_ignore_ascii_case(active_provider) => {
            Ok(None)
        }
        ModelRoute::Provider { alias, model } => {
            // Build BEFORE installing: a failed build must leave the live
            // client exactly as it was.
            let built = build_route_client(cfg, alias, model)?;
            *client = built;
            tracing::info!(
                target: "dirge::provider",
                alias = %alias,
                model = %model,
                "model routed to a different provider; client rebuilt",
            );
            Ok(Some(alias.clone()))
        }
        ModelRoute::Unroutable { model, family } => Err(RouteRefusal::NoProviderForFamily {
            model: model.clone(),
            family: family.clone(),
        }),
    }
}

/// Point the live session at `route`: rebuild the client when the route targets
/// a different provider, then move the session's model, provider, and context
/// window to match. Returns the alias switched to, or `None` for a same-client
/// rename.
///
/// All-or-nothing: on `Err` neither the client nor the session is touched, so a
/// refused switch leaves a working session rather than a half-applied one.
pub fn apply_model_route(
    cfg: &Config,
    client: &mut AnyClient,
    session: &mut Session,
    route: ModelRoute,
) -> Result<Option<String>, RouteRefusal> {
    let switched_to = swap_client_for_route(cfg, client, session.provider.as_str(), &route)?;
    session.model = compact_str::CompactString::new(route.model());
    // Only follow the client when it actually moved. A same-client rename must
    // leave `provider` alone: overwriting it (with the CLI/config default, say)
    // would make the NEXT routing decision reason from the wrong active
    // provider (dirge-cfaw / dirge-m6ut).
    if let Some(alias) = &switched_to {
        session.provider = compact_str::CompactString::new(alias);
    }
    // GH #772: resolved AFTER the provider is updated, because a per-provider
    // window belongs to the provider the route lands on — reading it before
    // the swap would answer for the one being left.
    session.context_window = cfg.resolve_context_window_for(&session.provider, route.model());
    Ok(switched_to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderEntry;
    use std::collections::HashMap;

    fn entry(provider_type: &str, model: &str) -> ProviderEntry {
        ProviderEntry {
            provider_type: Some(provider_type.to_string()),
            model: Some(model.to_string()),
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        }
    }

    /// The GH #711 shape: an openai alias on a subscription plus a glm provider.
    fn cfg() -> Config {
        Config {
            providers: Some(HashMap::from([
                ("gpt-sol".to_string(), entry("openai", "gpt-5.5")),
                ("glm".to_string(), entry("glm", "glm-5.2")),
            ])),
            ..Default::default()
        }
    }

    fn client(cfg: &Config, alias: &str) -> AnyClient {
        super::super::create_client_with_auth(alias, None, &cfg.providers_map(), None).unwrap()
    }

    fn session(provider: &str, model: &str) -> Session {
        Session::new(provider, model, 128_000)
    }

    #[test]
    fn resolve_maps_the_switch_decision_to_a_route() {
        let cfg = cfg();
        assert_eq!(
            resolve_model_route(&cfg, "gpt-sol", "glm-5.2"),
            ModelRoute::Provider {
                alias: "glm".into(),
                model: "glm-5.2".into()
            },
        );
        assert_eq!(
            resolve_model_route(&cfg, "gpt-sol", "gpt-5.5-mini"),
            ModelRoute::Active("gpt-5.5-mini".into()),
        );
        assert_eq!(
            resolve_model_route(&cfg, "gpt-sol", "claude-opus-4"),
            ModelRoute::Unroutable {
                model: "claude-opus-4".into(),
                family: "anthropic".into()
            },
        );
    }

    #[test]
    fn pinned_route_names_its_provider_verbatim() {
        assert_eq!(
            ModelRoute::pinned("gpt-sol", "gpt-5.5"),
            ModelRoute::Provider {
                alias: "gpt-sol".into(),
                model: "gpt-5.5".into()
            },
        );
        assert_eq!(ModelRoute::pinned("a", "m").model(), "m");
    }

    /// dirge-fhr5: the core of the fix — a cross-provider route moves the
    /// CLIENT, not just the model name.
    #[test]
    fn applying_a_cross_provider_route_rebuilds_the_client() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");

        let switched = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            resolve_model_route(&cfg, "gpt-sol", "glm-5.2"),
        )
        .expect("a configured glm provider must be routable");

        assert_eq!(switched.as_deref(), Some("glm"));
        assert!(matches!(client, AnyClient::Glm(_)), "client must move");
        assert_eq!(session.model, "glm-5.2");
        assert_eq!(session.provider, "glm", "session must follow the client");
    }

    /// A same-client rename must NOT touch `session.provider`: resetting it
    /// would make the next routing decision reason from the wrong active
    /// provider.
    #[test]
    fn applying_a_same_client_route_leaves_the_provider_alone() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");

        let switched = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            resolve_model_route(&cfg, "gpt-sol", "gpt-5.5-mini"),
        )
        .unwrap();

        assert_eq!(switched, None);
        assert!(matches!(client, AnyClient::OpenAI(_)));
        assert_eq!(session.model, "gpt-5.5-mini");
        assert_eq!(session.provider, "gpt-sol");
    }

    #[test]
    fn a_refused_route_leaves_the_session_untouched() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");

        let err = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            resolve_model_route(&cfg, "gpt-sol", "claude-opus-4"),
        )
        .unwrap_err();

        assert_eq!(
            err,
            RouteRefusal::NoProviderForFamily {
                model: "claude-opus-4".into(),
                family: "anthropic".into()
            }
        );
        assert!(matches!(client, AnyClient::OpenAI(_)), "client untouched");
        assert_eq!(session.model, "gpt-5.5", "session untouched");
        assert_eq!(session.provider, "gpt-sol");
    }

    /// A pinned route whose provider can't be built (stale alias, bad config)
    /// refuses rather than half-applying.
    #[test]
    fn a_route_whose_provider_cannot_be_built_is_refused() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");

        let err = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            ModelRoute::pinned("no-such-provider", "whatever"),
        )
        .unwrap_err();

        assert!(matches!(err, RouteRefusal::ProviderBuildFailed { .. }));
        assert!(matches!(client, AnyClient::OpenAI(_)));
        assert_eq!(session.model, "gpt-5.5");
        assert_eq!(session.provider, "gpt-sol");
    }

    /// dirge-fhr5's restore case: `/agent off` must return to the EXACT
    /// provider it left, which inference cannot promise. Here two openai
    /// aliases serve the same family and inference picks the sorted-first one —
    /// a pinned route goes back to the one actually in use.
    #[test]
    fn a_pinned_route_restores_the_exact_provider_inference_would_miss() {
        let cfg = Config {
            providers: Some(HashMap::from([
                ("azure-gpt".to_string(), entry("openai", "gpt-5.5")),
                ("gpt-sol".to_string(), entry("openai", "gpt-5.5")),
                ("glm".to_string(), entry("glm", "glm-5.2")),
            ])),
            ..Default::default()
        };
        // Inference from the id alone lands on the sorted-first openai alias.
        assert_eq!(
            resolve_model_route(&cfg, "glm", "gpt-5.5"),
            ModelRoute::Provider {
                alias: "azure-gpt".into(),
                model: "gpt-5.5".into()
            },
        );

        // The captured pair goes back where it came from.
        let mut client = client(&cfg, "glm");
        let mut session = session("glm", "glm-5.2");
        let switched = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            ModelRoute::pinned("gpt-sol", "gpt-5.5"),
        )
        .unwrap();

        assert_eq!(switched.as_deref(), Some("gpt-sol"));
        assert_eq!(session.provider, "gpt-sol");
        assert_eq!(session.model, "gpt-5.5");
    }

    /// A route naming the provider the session is ALREADY on is a rename, not a
    /// swap. Rebuilding would be pointless work and can outright fail — the
    /// rebuild resolves credentials from config/env, so a session launched with
    /// `--api-key` has no key to rebuild from. `/agent off` restores a pinned
    /// pair on every deactivation, including profiles that never moved the
    /// client, so this is the common path.
    #[test]
    fn a_route_to_the_provider_already_live_does_not_rebuild() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");

        let switched = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            ModelRoute::pinned("gpt-sol", "gpt-4o"),
        )
        .unwrap();

        assert_eq!(switched, None, "no client swap was needed");
        assert_eq!(session.model, "gpt-4o", "the rename still applies");
        assert_eq!(session.provider, "gpt-sol");
    }

    /// The same guard makes an unbuildable provider harmless when it is the one
    /// already live — nothing has to be rebuilt to stay where we are.
    #[test]
    fn a_route_to_an_unbuildable_provider_already_live_is_not_refused() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("ephemeral-cli-provider", "some-model");

        let switched = apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            ModelRoute::pinned("ephemeral-cli-provider", "restored-model"),
        )
        .expect("staying put must never need a client build");

        assert_eq!(switched, None);
        assert_eq!(session.model, "restored-model");
    }

    #[test]
    fn applying_a_route_moves_the_context_window() {
        let cfg = cfg();
        let mut client = client(&cfg, "gpt-sol");
        let mut session = session("gpt-sol", "gpt-5.5");
        session.context_window = 1;

        apply_model_route(
            &cfg,
            &mut client,
            &mut session,
            resolve_model_route(&cfg, "gpt-sol", "glm-5.2"),
        )
        .unwrap();

        assert_eq!(
            session.context_window,
            cfg.resolve_context_window("glm-5.2"),
            "the window must follow the model, not stay stale",
        );
    }
}
