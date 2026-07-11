//! Provider identity, resolution, autodetection, and API-key lookup.
//!
//! Split out of `provider/mod.rs` (dirge-4y4l): the pure
//! provider-resolution surface — turning a provider name/alias into a
//! concrete [`ProviderKind`] + [`ProviderInfo`], validating
//! custom/plugin endpoints, autodetecting from the environment, and
//! resolving the API key. No `rig` client/model types appear here; the
//! dispatch enums and agent-building wiring stay in their own modules.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::config::{Config, ProviderAuth, ProviderEntry};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProviderKind {
    OpenRouter,
    OpenAI,
    Anthropic,
    Gemini,
    DeepSeek,
    Glm,
    Ollama,
    OpenCode,
    Custom,
}

pub fn default_model_for(provider_name: &str) -> &'static str {
    // Per-provider sensible defaults. Without per-provider defaults
    // an unspecified `--model` against OpenAI/Anthropic/Gemini/Ollama
    // would pass `deepseek/deepseek-v4-flash` and the API would reject
    // with a confusing 404. Each provider gets a current-as-of-2026
    // first-class model id; OpenRouter keeps the multi-vendor prefix
    // form since that's what its API expects.
    match parse_provider(provider_name) {
        Some(ProviderKind::OpenAI) => "gpt-4o",
        Some(ProviderKind::Anthropic) => "claude-sonnet-4-6",
        Some(ProviderKind::Gemini) => "gemini-2.0-flash",
        Some(ProviderKind::DeepSeek) => "deepseek-v4-pro",
        Some(ProviderKind::Glm) => "glm-5.2",
        Some(ProviderKind::OpenCode) => "deepseek-v4-flash",
        Some(ProviderKind::Ollama) => "llama3",
        // OpenRouter + Custom + unknown — keep the historical default
        // since OpenRouter wants the `vendor/model` form.
        _ => "deepseek/deepseek-v4-flash",
    }
}

/// dirge-j3jd: default model for a provider ALIAS backed by a config/plugin
/// entry. A custom alias (e.g. `my-openai` with `provider_type = "openai"`)
/// is not a built-in name, so `default_model_for` on the bare alias would
/// miss `parse_provider` and fall back to the OpenRouter `vendor/model`
/// default — an invalid id for OpenAI/Anthropic/etc. Resolve the entry's
/// effective provider TYPE first.
pub fn default_model_for_entry(alias: &str, entry: &ProviderEntry) -> &'static str {
    default_model_for(&Config::provider_type_of(alias, entry))
}

/// dirge-j3jd: default model for a provider alias, resolving its entry from
/// `providers` first. Falls back to treating the alias as a built-in name
/// when no entry is declared.
pub fn default_model_for_alias(
    alias: &str,
    providers: &HashMap<String, ProviderEntry>,
) -> &'static str {
    match providers
        .get(alias)
        .or_else(|| providers.get(&alias.to_ascii_lowercase()))
    {
        Some(entry) => default_model_for_entry(alias, entry),
        None => default_model_for(alias),
    }
}

pub fn parse_provider(name: &str) -> Option<ProviderKind> {
    match name.to_lowercase().as_str() {
        "openrouter" => Some(ProviderKind::OpenRouter),
        "openai" => Some(ProviderKind::OpenAI),
        "anthropic" => Some(ProviderKind::Anthropic),
        "gemini" | "google" => Some(ProviderKind::Gemini),
        "deepseek" => Some(ProviderKind::DeepSeek),
        "glm" | "zhipu" => Some(ProviderKind::Glm),
        "opencode" => Some(ProviderKind::OpenCode),
        "ollama" => Some(ProviderKind::Ollama),
        "custom" => Some(ProviderKind::Custom),
        _ => None,
    }
}

pub struct ProviderInfo {
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub auth: Option<ProviderAuth>,
    /// Literal API key resolved from `entry.api_key` (with `${VAR}`
    /// already expanded). When present, takes precedence over both
    /// `api_key_env` and the standard env-var fallback chain.
    pub api_key_literal: Option<String>,
}

pub fn resolve_provider_info(
    name: &str,
    providers: &HashMap<String, ProviderEntry>,
) -> Option<ProviderInfo> {
    // Config-declared providers win on name collision — user intent
    // always trumps plugin defaults.
    // #2 fix: lowercase-fallback lookup so `--provider My-VLLM` finds
    // a `providers["my-vllm"]` config entry. parse_provider
    // (for built-ins) is already case-insensitive; matching the same
    // convention here removes a silent miss.
    let lower = name.to_ascii_lowercase();
    if let Some(entry) = providers.get(name).or_else(|| providers.get(&lower)) {
        let ptype = Config::provider_type_of(name, entry);
        let kind = parse_provider(&ptype)?;
        // Only enforce URL safety when the entry actually carries
        // a base_url. Built-in providers (e.g. `"deepseek": {}`)
        // legitimately have no base_url — they fall through to the
        // provider's default endpoint.
        // dirge-8sku: a CONFIG-declared entry is the user's own trusted
        // intent — aliasing a built-in name with a custom base_url (e.g.
        // `ollama`/`openai` pointed at a local proxy) is documented and
        // legitimate, so the built-in-name collision guard is NOT enforced
        // here. It exists only to stop an UNTRUSTED plugin from shadowing a
        // built-in to intercept credentials (enforced in the plugin branch
        // below). The URL-scheme (https / allow_insecure) check still runs.
        if let Some(url) = entry.base_url.as_deref()
            && let Err(err) = validate_custom_provider(
                name,
                url,
                entry.allow_insecure,
                /* enforce_builtin_collision */ false,
            )
        {
            tracing::error!(
                target: "dirge::provider",
                "{err}"
            );
            eprintln!("error: {err}");
            return None;
        }
        let api_key_literal = match entry.resolved_api_key() {
            Some(Ok(k)) => Some(k),
            Some(Err(missing)) => {
                tracing::error!(
                    target: "dirge::provider",
                    "provider '{name}' references env var ${{{missing}}} via api_key but it is unset",
                );
                eprintln!(
                    "error: provider '{name}' references env var ${{{missing}}} via api_key but it is unset"
                );
                None
            }
            None => None,
        };
        return Some(ProviderInfo {
            kind,
            base_url: entry.base_url.clone(),
            api_key_env: entry.api_key_env.clone(),
            auth: entry.auth,
            api_key_literal,
        });
    }
    // Then plugin-registered providers from `harness/register-provider`.
    // Installed once at startup after plugin load; never mutated again
    // in this process.
    if let Some(entry) = plugin_provider(name).or_else(|| plugin_provider(&lower)) {
        let ptype = Config::provider_type_of(name, &entry);
        let kind = parse_provider(&ptype)?;
        // dirge-8sku: plugin providers are UNTRUSTED — enforce the
        // built-in-name collision guard so a plugin can't register e.g.
        // "openai" to silently intercept the user's OpenAI credentials.
        if let Some(url) = entry.base_url.as_deref()
            && let Err(err) = validate_custom_provider(
                name,
                url,
                entry.allow_insecure,
                /* enforce_builtin_collision */ true,
            )
        {
            tracing::error!(
                target: "dirge::provider",
                "{err}"
            );
            eprintln!("error: {err}");
            return None;
        }
        let api_key_literal = match entry.resolved_api_key() {
            Some(Ok(k)) => Some(k),
            Some(Err(missing)) => {
                tracing::error!(
                    target: "dirge::provider",
                    "plugin provider '{name}' references env var ${{{missing}}} via api_key but it is unset",
                );
                eprintln!(
                    "error: plugin provider '{name}' references env var ${{{missing}}} via api_key but it is unset"
                );
                None
            }
            None => None,
        };
        return Some(ProviderInfo {
            kind,
            base_url: entry.base_url,
            api_key_env: entry.api_key_env,
            auth: entry.auth,
            api_key_literal,
        });
    }
    let kind = parse_provider(name)?;
    Some(ProviderInfo {
        kind,
        base_url: None,
        api_key_env: None,
        auth: None,
        api_key_literal: None,
    })
}

/// Built-in provider names — custom/plugin providers are rejected
/// if they collide with one of these. Protects against a malicious
/// plugin that registers "openai" to silently intercept credentials.
const BUILTIN_PROVIDER_NAMES: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "google",
    "deepseek",
    "glm",
    "zhipu",
    "ollama",
    "openrouter",
    "custom",
];

/// Validate a custom/plugin provider's configuration.
/// - Rejects names that collide with built-in providers.
/// - Rejects non-https base_url unless `allow_insecure: true`.
pub(crate) fn validate_custom_provider(
    name: &str,
    base_url: &str,
    allow_insecure: bool,
    enforce_builtin_collision: bool,
) -> Result<(), String> {
    // dirge-8sku: only UNTRUSTED (plugin) providers are blocked from
    // shadowing a built-in name; a user's own config may legitimately
    // alias one (e.g. `ollama` → openai backend + local base_url).
    if enforce_builtin_collision {
        let lower = name.to_ascii_lowercase();
        if BUILTIN_PROVIDER_NAMES
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&lower))
        {
            return Err(format!(
                "Custom provider '{}' collides with built-in provider name. \
                 Choose a different name.",
                name
            ));
        }
    }
    // URL scheme validation: only https:// is safe by default.
    // http:// sends plaintext over the network — every prompt,
    // file content, and tool result is exposed. Only allow when
    // the user explicitly opts in via `allow_insecure: true`,
    // which is appropriate for local-only proxies (ollama, vllm).
    if !allow_insecure && !base_url.starts_with("https://") {
        return Err(format!(
            "Custom provider '{}' has insecure base_url '{}'. \
             Set allow_insecure: true in config.json if this is a \
             local-only endpoint (e.g. ollama, vllm). All other \
             http:// URLs send your data in plaintext.",
            name, base_url
        ));
    }
    // PROV-1 stretch: when allow_insecure is set AND the base_url is
    // http://, also gate on host shape. Loopback / private-range
    // hosts (the legitimate ollama/vllm/lmstudio case) are silent;
    // a public-looking host with allow_insecure gets a LOUD stderr
    // warning every session so a misconfigured production setup
    // doesn't silently leak conversation content.
    if allow_insecure && base_url.starts_with("http://") && !looks_like_local_host(base_url) {
        eprintln!(
            "  ⚠️  WARNING: custom provider '{}' is using http:// over a NON-LOCAL host: {}\n  Every prompt, file content, and tool result is sent in plaintext.\n  This is allowed because allow_insecure: true is set in config.json,\n  but you should verify this is intentional — the typical allow_insecure\n  use case is loopback (127.0.0.1 / localhost) endpoints like ollama.",
            name, base_url,
        );
    }
    Ok(())
}

/// Quick check whether a base_url's host appears to be a loopback or
/// private-range address. Used by `validate_custom_provider` to
/// decide whether `allow_insecure: true` is benign (local ollama)
/// or alarming (somebody pointing at a public http endpoint). Not
/// a security boundary — `validate_custom_provider` already
/// rejected the dangerous case (http without allow_insecure) before
/// this function runs.
fn looks_like_local_host(base_url: &str) -> bool {
    let scheme_len = if base_url.len() >= 7 && base_url[..7].eq_ignore_ascii_case("http://") {
        7
    } else {
        return false;
    };
    let after = &base_url[scheme_len..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let host_and_port = &after[..end];
    let host: &str = if let Some(rest) = host_and_port.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        &rest[..end]
    } else {
        host_and_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_and_port)
    };
    let lower = host.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        };
    }
    // `.local` mDNS names are also commonly local-only.
    lower.ends_with(".local")
}

/// Process-global map of plugin-registered providers, populated once
/// after plugin load. Stored separately from `cfg.custom_providers`
/// so a `/reload` (future) can swap plugin providers without
/// disturbing the user's persistent config.
static PLUGIN_PROVIDERS: OnceLock<HashMap<String, ProviderEntry>> = OnceLock::new();

/// Install the plugin-registered provider map. Only the first call
/// wins (OnceLock semantics) — sufficient for current behavior where
/// plugins re-register every startup and never change at runtime.
/// Returns the installed-or-already-installed map size so callers
/// can log a confirmation.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn install_plugin_providers(map: HashMap<String, ProviderEntry>) -> usize {
    let size = map.len();
    // dirge-gsbf: don't silently swallow a second install. OnceLock::set
    // fails (returning the map) once already set — e.g. a plugin hot-reload
    // re-registering providers. Surface it instead of `let _ =`, and report
    // the size actually in effect (the first install's).
    if let Err(rejected) = PLUGIN_PROVIDERS.set(map) {
        let in_effect = PLUGIN_PROVIDERS.get().map(|m| m.len()).unwrap_or(0);
        tracing::warn!(
            target: "dirge::provider",
            attempted = rejected.len(),
            in_effect,
            "plugin providers already installed — ignoring re-registration (runtime hot-reload of providers is not supported)",
        );
        return in_effect;
    }
    size
}

fn plugin_provider(name: &str) -> Option<ProviderEntry> {
    PLUGIN_PROVIDERS.get().and_then(|m| m.get(name).cloned())
}

fn provider_env_var(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAI => "OPENAI_API_KEY",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        ProviderKind::DeepSeek => "DEEPSEEK_API_KEY",
        ProviderKind::Glm => "GLM_API_KEY",
        ProviderKind::OpenCode => "OPENCODE_API_KEY",
        ProviderKind::Ollama => "OLLAMA_API_KEY",
        ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
        ProviderKind::Custom => "CUSTOM_API_KEY",
    }
}

/// Auto-detect provider from environment variables when none is
/// explicitly configured. Returns the provider name string
/// (e.g. "deepseek") for the first matching `*_API_KEY` env var
/// with a non-empty value. Returns `None` if no known key is set.
///
/// Resolution order is fixed (see `PROVIDER_AUTODETECT_ORDER`).
/// When multiple keys are present, the FIRST in that list wins so
/// the behavior is deterministic — important for users who have
/// several keys in their shell environment.
pub fn auto_detect_provider() -> Option<&'static str> {
    auto_detect_provider_from(|name| std::env::var(name).ok())
}

/// Provider candidate list for autodetect. Listed in priority
/// order — first key with a non-empty value wins. Extracted as a
/// module item so tests reference the same source of truth and
/// adding a provider only touches one place.
pub(crate) const PROVIDER_AUTODETECT_ORDER: &[(&str, &str)] = &[
    ("DEEPSEEK_API_KEY", "deepseek"),
    ("OPENAI_API_KEY", "openai"),
    ("ANTHROPIC_API_KEY", "anthropic"),
    ("GEMINI_API_KEY", "gemini"),
    ("GLM_API_KEY", "glm"),
    // Zhipu's canonical env var name for the same provider. Listed
    // after GLM_API_KEY so users with both set get the dirge-
    // primary one; users with only ZHIPU_API_KEY still get glm.
    ("ZHIPU_API_KEY", "glm"),
    ("OPENCODE_API_KEY", "opencode"),
    ("OLLAMA_API_KEY", "ollama"),
    ("OPENROUTER_API_KEY", "openrouter"),
];

/// Pure helper that drives `auto_detect_provider` from a
/// caller-supplied env lookup. Production calls
/// `auto_detect_provider()` which passes `std::env::var`; tests
/// pass a closure backed by a HashMap so they don't mutate
/// process-wide env vars (which races under parallel `cargo test`).
pub(crate) fn auto_detect_provider_from<F: Fn(&str) -> Option<String>>(
    env: F,
) -> Option<&'static str> {
    for (env_var, provider_name) in PROVIDER_AUTODETECT_ORDER {
        if let Some(v) = env(env_var)
            && !v.is_empty()
        {
            return Some(provider_name);
        }
    }
    None
}

/// Provider implied by a stored `dirge auth` OAuth login. Consulted
/// after env-var autodetect and before the hard `openrouter` default so
/// that a user who ran `dirge auth openai` (or `dirge auth anthropic`) but
/// set no API-key env var and no `provider` in config launches against the
/// account they logged in to, instead of being asked for an OpenRouter key
/// (GH #617). Reads the local credential stores only — no network.
pub fn auth_detect_provider() -> Option<&'static str> {
    let openai = crate::auth::store::OpenAiAuthStore::default()
        .load_openai()
        .ok()
        .flatten()
        .is_some();
    let anthropic = std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok_and(|v| !v.is_empty())
        || crate::provider::anthropic_oauth::credentials_file_path().exists();
    auth_detect_provider_from(openai, anthropic)
}

/// Pure core of [`auth_detect_provider`]: pick a provider from which
/// OAuth logins are present. OpenAI wins over Anthropic when both exist —
/// arbitrary but stable, matching the env-autodetect order where openai
/// precedes anthropic.
pub(crate) fn auth_detect_provider_from(openai: bool, anthropic: bool) -> Option<&'static str> {
    if openai {
        Some("openai")
    } else if anthropic {
        Some("anthropic")
    } else {
        None
    }
}

/// Per-provider fallback env vars consulted AFTER the primary
/// (returned by `provider_env_var`) and after any explicit
/// `api_key_env_override`. Lets users with the upstream-canonical
/// env var name (e.g. ZHIPU_API_KEY for GLM/Zhipu) skip aliasing.
///
/// Empty for providers with no widely-used alternative; the slice
/// is iterated in order and the first non-empty value wins.
pub(crate) fn provider_env_var_fallbacks(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        // Zhipu's docs + their official SDKs uniformly use
        // ZHIPU_API_KEY. GLM_API_KEY is dirge's chosen primary
        // (matches the provider name), but accepting the
        // canonical form means users don't have to alias.
        ProviderKind::Glm => &["ZHIPU_API_KEY"],
        // dirge-ro8g: ANTHROPIC_OAUTH_TOKEN is an OAuth bearer
        // (`sk-ant-oat…`), NOT an x-api-key — it needs the Bearer
        // header + oauth beta + payload shaping that
        // AnthropicHttpClient adds, so sending it as an API key is
        // rejected. Its presence now implies ProviderAuth::Anthropic
        // (see client.rs `effective_auth`), routing it through
        // `resolve_anthropic_auth`. So it must NOT be an API-key
        // fallback here (was `&["ANTHROPIC_OAUTH_TOKEN"]`).
        ProviderKind::Anthropic => &[],
        // Google's generative-language SDK (and the official
        // gemini-cli) uses GOOGLE_GENERATIVE_AI_API_KEY. dirge's
        // primary GEMINI_API_KEY matches the provider name in the
        // /model command surface; accepting the Google-canonical
        // form means users don't have to alias.
        ProviderKind::Gemini => &["GOOGLE_GENERATIVE_AI_API_KEY", "GOOGLE_API_KEY"],
        _ => &[],
    }
}

pub(crate) fn resolve_api_key_from<F>(
    kind: ProviderKind,
    api_key_env_override: Option<&str>,
    cli_key: Option<&str>,
    env: F,
) -> anyhow::Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(key) = cli_key.filter(|k| !k.is_empty()) {
        // Audit C2: the `/proc/*/cmdline` warning now fires at the
        // call site in main.rs where we know which CLI source the
        // key came from. File-sourced and stdin-sourced keys end up
        // here too but those paths don't appear in argv, so no
        // warning is wanted.
        return Ok(key.to_string());
    }

    let env_var = api_key_env_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| provider_env_var(kind));

    if let Some(key) = env(env_var)
        && !key.is_empty()
    {
        return Ok(key);
    }

    // Provider-specific fallback env vars (e.g. ZHIPU_API_KEY
    // for GLM). Skip if the override was explicit — in that case
    // the user named the env var they want; don't second-guess.
    if api_key_env_override.is_none_or(|s| s.is_empty()) {
        for fallback in provider_env_var_fallbacks(kind) {
            if let Some(key) = env(fallback)
                && !key.is_empty()
            {
                return Ok(key);
            }
        }
    }

    if kind == ProviderKind::Ollama {
        return Ok(String::new());
    }

    if kind == ProviderKind::Custom {
        return Ok(String::new());
    }

    // OpenAI-compatible endpoints that need no key (a local ollama / vLLM /
    // LM Studio server) should use `provider_type: "custom"` (or "ollama"),
    // which are keyless — point the user there rather than just demanding a key.
    let keyless_hint = if kind == ProviderKind::OpenAI {
        " For a keyless OpenAI-compatible endpoint (e.g. a local ollama/vLLM server), set `provider_type` to \"custom\" (or \"ollama\") instead of \"openai\"."
    } else {
        ""
    };

    let fallbacks = provider_env_var_fallbacks(kind);
    if fallbacks.is_empty() {
        anyhow::bail!(
            "No API key found for {kind:?}. Set the {env_var} environment variable or pass --api-key.{keyless_hint}"
        )
    } else {
        anyhow::bail!(
            "No API key found for {kind:?}. Set {env_var} (or one of: {}) or pass --api-key.{keyless_hint}",
            fallbacks.join(", ")
        )
    }
}

#[cfg(test)]
mod ro8g_tests {
    use super::*;

    /// dirge-ro8g: ANTHROPIC_OAUTH_TOKEN is an OAuth bearer (needs the
    /// Bearer header + oauth beta + payload shaping AnthropicHttpClient
    /// adds), NOT an x-api-key. It must not appear in the API-key fallback
    /// list — its presence now routes through ProviderAuth::Anthropic.
    #[test]
    fn anthropic_oauth_token_is_not_an_api_key_fallback() {
        assert!(
            !provider_env_var_fallbacks(ProviderKind::Anthropic).contains(&"ANTHROPIC_OAUTH_TOKEN"),
            "ANTHROPIC_OAUTH_TOKEN must not be treated as an API key"
        );
    }
}
