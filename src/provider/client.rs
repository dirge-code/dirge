//! Provider client construction.
//!
//! Contains `create_client` — the 8-backend dispatch that builds
//! rig clients (OpenAI, Anthropic, Gemini, DeepSeek, GLM, Ollama,
//! OpenRouter, Custom). Extracted from `provider/mod.rs` to keep
//! the provider module focused on type definitions + agent
//! construction.

use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use rig::http_client::HeaderMap;
use rig::providers::{anthropic, chatgpt, gemini, ollama, openai, openrouter};

use crate::auth::store::{OpenAiAuthStore, OpenAiOAuthCredential};
use crate::config::{ProviderAuth, ProviderEntry};

use super::auth::{ProviderAuthHeaders, resolve_auth_headers};
use super::codex_http::CodexHttpClient;
use super::{AnyClient, ProviderKind, resolve_api_key_from, resolve_provider_info};

const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CHATGPT_ORIGINATOR: &str = "dirge";

#[derive(Clone, PartialEq, Eq)]
enum ProviderCredential {
    ApiKey(String),
    ChatGptAuth(String),
    OpenAiOAuth {
        access_token: String,
        account_id: Option<String>,
    },
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.debug_tuple("ApiKey").field(&"[REDACTED]").finish(),
            Self::ChatGptAuth(_) => f.debug_tuple("ChatGptAuth").field(&"[REDACTED]").finish(),
            Self::OpenAiOAuth { account_id, .. } => f
                .debug_struct("OpenAiOAuth")
                .field("access_token", &"[REDACTED]")
                .field("account_id", account_id)
                .finish(),
        }
    }
}

impl ProviderCredential {
    fn into_secret(self) -> String {
        match self {
            Self::ApiKey(secret) | Self::ChatGptAuth(secret) => secret,
            Self::OpenAiOAuth { access_token, .. } => access_token,
        }
    }

    fn is_openai_oauth(&self) -> bool {
        matches!(self, Self::OpenAiOAuth { .. })
    }

    fn openai_oauth_account_id(&self) -> Option<&str> {
        match self {
            Self::OpenAiOAuth { account_id, .. } => account_id.as_deref(),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<AnyClient> {
    create_client_with(
        provider_name,
        api_key,
        providers,
        |name| std::env::var(name).ok(),
        load_fresh_openai_oauth,
    )
}

pub(crate) fn create_client_with_auth(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
) -> anyhow::Result<AnyClient> {
    create_client_with_resolved_auth(
        provider_name,
        api_key,
        providers,
        default_auth,
        None,
        |name| std::env::var(name).ok(),
        load_fresh_openai_oauth,
    )
}

pub(crate) fn create_openai_api_key_fallback_client(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<Option<AnyClient>> {
    create_openai_api_key_fallback_client_with_env(provider_name, api_key, providers, |name| {
        std::env::var(name).ok()
    })
}

fn create_openai_api_key_fallback_client_with_env<F>(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
    env: F,
) -> anyhow::Result<Option<AnyClient>>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(info) = resolve_provider_info(provider_name, providers) else {
        return Ok(None);
    };
    if !provider_name.eq_ignore_ascii_case("openai")
        || info.kind != ProviderKind::OpenAI
        || info.base_url.is_some()
    {
        return Ok(None);
    }

    let key = if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        key.to_string()
    } else if let Some(key) = info.api_key_literal.filter(|key| !key.is_empty()) {
        key
    } else {
        match resolve_api_key_from(info.kind, info.api_key_env.as_deref(), None, env) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        }
    };
    let client = openai::CompletionsClient::builder().api_key(&key).build()?;
    Ok(Some(AnyClient::OpenAI(client)))
}

/// dirge-ro8g: pick the effective auth mode. An EXPLICIT choice (config
/// `auth:` or a top-level default) always wins. Otherwise the default is
/// `ApiKey`, EXCEPT for the anthropic provider when an OAuth login is
/// present (`anthropic_oauth_present`) — then it's `Anthropic`, so the
/// stored `dirge auth anthropic` login / `ANTHROPIC_OAUTH_TOKEN` is used
/// via `resolve_anthropic_auth` instead of being ignored or mis-sent as an
/// x-api-key.
fn effective_auth(
    explicit: Option<ProviderAuth>,
    kind: ProviderKind,
    anthropic_oauth_present: bool,
) -> ProviderAuth {
    match explicit {
        Some(auth) => auth,
        None if kind == ProviderKind::Anthropic && anthropic_oauth_present => {
            ProviderAuth::Anthropic
        }
        None => ProviderAuth::ApiKey,
    }
}

/// dirge-pkh1: resolve a provider's base URL with ONE precedence for every
/// provider — config (`config_url`, already scheme-validated upstream in
/// `resolve.rs`) > provider env var > hard default.
///
/// Previously DeepSeek/GLM used `env > default` and IGNORED config, Custom
/// used `config > env`, and everyone else was config-only — so a user who
/// set `providers.deepseek.base_url` to a proxy was silently ignored. And
/// the env-var URLs skipped `resolve.rs`'s https check entirely, so an
/// http:// value in `DEEPSEEK_BASE_URL` / `GLM_BASE_URL` / `CUSTOM_BASE_URL`
/// sent the API key in plaintext. Env-sourced URLs are https-checked here
/// (they carry no `allow_insecure`); config URLs already passed their check,
/// and the built-in defaults are https by construction.
///
/// `env` is injected so precedence is unit-testable without touching the
/// process environment. OpenAI-OAuth / ChatGPT base URLs force the Codex
/// endpoint in the caller and never come through here.
fn resolve_provider_base_url(
    kind: ProviderKind,
    config_url: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<String>> {
    if let Some(cfg) = config_url {
        return Ok(Some(cfg)); // config wins — the user's explicit, validated intent
    }
    let (env_var, default): (Option<&str>, Option<&str>) = match kind {
        ProviderKind::DeepSeek => (
            Some("DEEPSEEK_BASE_URL"),
            Some("https://api.deepseek.com/v1"),
        ),
        ProviderKind::Glm => (
            Some("GLM_BASE_URL"),
            Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        ),
        ProviderKind::Custom => (Some("CUSTOM_BASE_URL"), None),
        _ => (None, None),
    };
    if let Some(var) = env_var
        && let Some(url) = env(var).filter(|u| !u.is_empty())
    {
        if !url.starts_with("https://") {
            anyhow::bail!(
                "${var} is `{url}`, but only https:// base URLs are accepted from the \
                 environment (an http:// endpoint sends the API key in plaintext). Use \
                 https://, or configure a `custom` provider with `allow_insecure: true` \
                 for a local-only http endpoint."
            );
        }
        return Ok(Some(url));
    }
    Ok(default.map(str::to_string))
}

#[cfg(test)]
fn create_client_with<F, G>(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
    env: F,
    load_openai_oauth: G,
) -> anyhow::Result<AnyClient>
where
    F: Fn(&str) -> Option<String>,
    G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
{
    create_client_with_resolved_auth(
        provider_name,
        api_key,
        providers,
        None,
        None,
        env,
        load_openai_oauth,
    )
}

fn create_client_with_resolved_auth<F, G>(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    resolved_auth_headers: Option<ProviderAuthHeaders>,
    env: F,
    load_openai_oauth: G,
) -> anyhow::Result<AnyClient>
where
    F: Fn(&str) -> Option<String>,
    G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
{
    let info = resolve_provider_info(provider_name, providers).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown provider: {}. Supported providers: openrouter, openai, anthropic, gemini, deepseek, glm, ollama, custom",
            provider_name
        )
    })?;

    // dirge-ro8g: for the anthropic provider, a present OAuth login — a
    // stored `dirge auth anthropic` creds file OR an exported
    // ANTHROPIC_OAUTH_TOKEN — implies OAuth auth when the user chose none
    // explicitly. Without this the ApiKey default never consults the login
    // and bails "No API key found for Anthropic", and the sk-ant-oat bearer
    // would otherwise be mis-sent as an x-api-key.
    let anthropic_oauth_present = info.kind == ProviderKind::Anthropic
        && (env("ANTHROPIC_OAUTH_TOKEN").is_some_and(|v| !v.trim().is_empty())
            || super::anthropic_oauth::credentials_file_path().exists());
    let auth = effective_auth(
        info.auth.or(default_auth),
        info.kind,
        anthropic_oauth_present,
    );
    // A top-level `auth: chatgpt` applies to every provider. Refuse non-OpenAI
    // early so a Codex bearer token is never sent to another provider.
    if auth == ProviderAuth::ChatGpt && info.kind != ProviderKind::OpenAI {
        anyhow::bail!(
            "ChatGPT (Codex) auth is only supported for the `openai` provider, not `{provider_name}`. \
             Set `auth: chatgpt` only on your openai provider (or use an API key for `{provider_name}`)."
        );
    }
    if auth == ProviderAuth::Anthropic && info.kind != ProviderKind::Anthropic {
        anyhow::bail!(
            "Anthropic OAuth is only supported for the `anthropic` provider, not `{provider_name}`. \
             Set `auth: anthropic` only on your anthropic provider (or use an API key for `{provider_name}`)."
        );
    }
    let auth_headers = match (auth, resolved_auth_headers) {
        (ProviderAuth::ChatGpt | ProviderAuth::Anthropic, Some(headers)) => Some(headers),
        _ => resolve_auth_headers(auth)?,
    };
    let is_chatgpt_auth = auth == ProviderAuth::ChatGpt;

    let credential = if let Some(headers) = auth_headers.as_ref() {
        ProviderCredential::ChatGptAuth(headers.bearer_token.clone())
    } else {
        // Canonical OpenAI prefers stored Dirge OAuth/Codex subscription auth
        // before API-key billing. API keys remain the fallback when no fresh
        // stored OAuth credential exists; non-canonical OpenAI-compatible
        // aliases and custom base URLs never receive native OAuth tokens.
        let allow_openai_oauth =
            provider_name.eq_ignore_ascii_case("openai") && info.base_url.as_deref().is_none();
        resolve_provider_credential(
            allow_openai_oauth,
            info.kind,
            info.api_key_literal.as_deref(),
            info.api_key_env.as_deref(),
            api_key,
            // Borrow so `env` stays available for the base-URL resolver below
            // (`&F` implements `Fn` when `F: Fn`).
            &env,
            load_openai_oauth,
        )?
    };
    let uses_openai_oauth = credential.is_openai_oauth();

    if is_chatgpt_auth {
        let has_account_id = auth_headers
            .as_ref()
            .and_then(|headers| headers.chatgpt_account_id.as_deref())
            .map(str::trim)
            .is_some_and(|account_id| !account_id.is_empty());
        if !has_account_id {
            anyhow::bail!(
                "ChatGPT auth requested, but no ChatGPT account id was found. Set CHATGPT_ACCOUNT_ID or run `codex login` so auth.json contains a chatgpt_account_id/account_id."
            );
        }
    }

    let openai_oauth_account_id = credential.openai_oauth_account_id().map(str::to_string);
    let key = credential.into_secret();
    let base_url = match info.kind {
        // OAuth / ChatGPT force the Codex endpoint (config may still point a
        // ChatGPT client at a proxy); everyone else goes through the one
        // shared config > env > default resolver (dirge-pkh1).
        ProviderKind::OpenAI if uses_openai_oauth => Some(CHATGPT_CODEX_BASE_URL.to_string()),
        ProviderKind::OpenAI if is_chatgpt_auth => info
            .base_url
            .clone()
            .or_else(|| Some(CHATGPT_CODEX_BASE_URL.to_string())),
        _ => resolve_provider_base_url(info.kind, info.base_url.clone(), &env)?,
    };

    // An OAuth login token — Codex/ChatGPT bearer, native Dirge OAuth, or an
    // Anthropic `sk-ant-oat` bearer — is higher-value than a per-provider API
    // key, so it must never leave over plaintext. `allow_insecure` is
    // intentionally not honored for any of them.
    let uses_anthropic_oauth = auth == ProviderAuth::Anthropic;
    if (is_chatgpt_auth || uses_openai_oauth || uses_anthropic_oauth)
        && let Some(url) = base_url.as_deref()
        && !url.starts_with("https://")
    {
        anyhow::bail!(
            "OAuth login auth requires an https base URL, but got `{url}`. The OAuth bearer \
             token is too sensitive to send over http:// — `allow_insecure` is ignored here."
        );
    }

    match info.kind {
        ProviderKind::OpenAI if uses_openai_oauth => {
            let b = chatgpt::Client::builder()
                .api_key(chatgpt::ChatGPTAuth::AccessToken {
                    access_token: key,
                    account_id: openai_oauth_account_id,
                })
                .originator(CHATGPT_ORIGINATOR)
                .base_url(CHATGPT_CODEX_BASE_URL);
            Ok(AnyClient::OpenAICodex(b.build()?))
        }
        ProviderKind::OpenAI if is_chatgpt_auth => {
            // `key` is the ChatGPT bearer from `auth_headers` (the
            // ChatGptAuth credential branch above), so the same headers
            // carry its provenance (dirge-8gdv.4).
            let bearer_is_dirge_oauth = auth_headers
                .as_ref()
                .is_some_and(|h| h.chatgpt_bearer_is_dirge_oauth);
            let mut b = openai::Client::builder()
                .api_key(&key)
                .http_client(codex_http_client_for(&key, bearer_is_dirge_oauth));
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            if let Some(headers) = chatgpt_http_headers(auth_headers.as_ref()) {
                b = b.http_headers(headers);
            }
            Ok(AnyClient::ChatGptOpenAI(b.build()?))
        }
        ProviderKind::OpenAI => {
            let mut b = openai::CompletionsClient::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::OpenAI(b.build()?))
        }
        ProviderKind::Anthropic => {
            if auth == ProviderAuth::Anthropic {
                // dirge-956a: hand the transport the bearer + its expiry and a
                // refresher, so a long session that crosses token expiry
                // re-resolves (and persists) a fresh credential instead of
                // dying on a non-retryable 401. Fall back to a static,
                // non-refreshing bearer if the expiry can't be read.
                let http = match super::auth::resolve_anthropic_auth_with_expiry() {
                    Ok(resolved) => {
                        let refresher: super::anthropic_http::RefreshFn =
                            std::sync::Arc::new(super::auth::resolve_anthropic_auth_with_expiry);
                        super::anthropic_http::AnthropicHttpClient::new_refreshable(
                            resolved.bearer_token,
                            resolved.expires_at_ms,
                            refresher,
                        )
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "dirge::provider",
                            error = %e,
                            "could not resolve Anthropic OAuth expiry; mid-session token refresh disabled",
                        );
                        let bearer = auth_headers
                            .as_ref()
                            .map(|h| h.bearer_token.clone())
                            .unwrap_or_else(|| key.clone());
                        super::anthropic_http::AnthropicHttpClient::new(bearer)
                    }
                };
                let mut b = anthropic::Client::builder().api_key(&key).http_client(http);
                if let Some(base_url) = &base_url {
                    b = b.base_url(base_url);
                }
                Ok(AnyClient::AnthropicOauth(b.build()?))
            } else {
                let mut b = anthropic::Client::builder().api_key(&key);
                if let Some(base_url) = &base_url {
                    b = b.base_url(base_url);
                }
                Ok(AnyClient::Anthropic(b.build()?))
            }
        }
        ProviderKind::Gemini => {
            let mut b = gemini::Client::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::Gemini(b.build()?))
        }
        ProviderKind::DeepSeek => {
            let b = openai::CompletionsClient::builder()
                .api_key(&key)
                .base_url(base_url.as_deref().unwrap_or("https://api.deepseek.com/v1"));
            Ok(AnyClient::DeepSeek(b.build()?))
        }
        ProviderKind::Glm => {
            let b = openai::CompletionsClient::builder().api_key(&key).base_url(
                base_url
                    .as_deref()
                    .unwrap_or("https://open.bigmodel.cn/api/coding/paas/v4"),
            );
            Ok(AnyClient::Glm(b.build()?))
        }
        ProviderKind::OpenCode => {
            let b = openai::CompletionsClient::builder()
                .api_key(&key)
                .base_url(base_url.as_deref().unwrap_or("https://opencode.ai/zen/v1"));
            Ok(AnyClient::OpenCode(b.build()?))
        }
        ProviderKind::Ollama => {
            let key: ollama::OllamaApiKey = key.as_str().into();
            let mut b = ollama::Client::builder().api_key(key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::Ollama(b.build()?))
        }
        ProviderKind::OpenRouter => {
            let mut b = openrouter::Client::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::OpenRouter(b.build()?))
        }
        ProviderKind::Custom => {
            let base_url = base_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "CUSTOM_BASE_URL environment variable must be set for custom provider"
                )
            })?;
            let b = openai::CompletionsClient::builder()
                .api_key(&key)
                .base_url(&base_url);
            Ok(AnyClient::Custom(b.build()?))
        }
    }
}

#[cfg(test)]
fn create_client_with_chatgpt_auth_headers(
    provider_name: &str,
    providers: &HashMap<String, ProviderEntry>,
    headers: ProviderAuthHeaders,
) -> anyhow::Result<AnyClient> {
    create_client_with_resolved_auth(
        provider_name,
        None,
        providers,
        Some(ProviderAuth::ChatGpt),
        Some(headers),
        |name| std::env::var(name).ok(),
        load_fresh_openai_oauth,
    )
}

#[cfg(test)]
fn create_client_with_anthropic_auth_headers(
    provider_name: &str,
    providers: &HashMap<String, ProviderEntry>,
    headers: ProviderAuthHeaders,
) -> anyhow::Result<AnyClient> {
    create_client_with_resolved_auth(
        provider_name,
        None,
        providers,
        Some(ProviderAuth::Anthropic),
        Some(headers),
        |name| std::env::var(name).ok(),
        load_fresh_openai_oauth,
    )
}

fn chatgpt_http_headers(auth_headers: Option<&ProviderAuthHeaders>) -> Option<HeaderMap> {
    let account_id = auth_headers?
        .chatgpt_account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let mut headers = HeaderMap::new();
    let name = http::HeaderName::from_static("chatgpt-account-id");
    let value = http::HeaderValue::from_str(account_id).ok()?;
    headers.insert(name, value);
    Some(headers)
}

fn resolve_provider_credential<F, G>(
    allow_openai_oauth: bool,
    kind: ProviderKind,
    api_key_literal: Option<&str>,
    api_key_env: Option<&str>,
    cli_key: Option<&str>,
    env: F,
    load_openai_oauth: G,
) -> anyhow::Result<ProviderCredential>
where
    F: Fn(&str) -> Option<String>,
    G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
{
    let mut openai_oauth_error = None;
    if kind == ProviderKind::OpenAI && allow_openai_oauth {
        match load_openai_oauth() {
            Ok(Some(credential)) => {
                return Ok(ProviderCredential::OpenAiOAuth {
                    access_token: credential.access_token().to_string(),
                    account_id: credential.account_id().map(str::to_string),
                });
            }
            Ok(None) => {}
            Err(err) => openai_oauth_error = Some(err),
        }
    }
    if let Some(key) = cli_key.filter(|k| !k.is_empty()) {
        return Ok(ProviderCredential::ApiKey(key.to_string()));
    }
    if let Some(key) = api_key_literal.filter(|k| !k.is_empty()) {
        return Ok(ProviderCredential::ApiKey(key.to_string()));
    }

    resolve_api_key_from(kind, api_key_env, None, env)
        .map(ProviderCredential::ApiKey)
        .map_err(|err| {
            if let Some(openai_oauth_error) = openai_oauth_error {
                return openai_oauth_error;
            }
            if kind == ProviderKind::OpenAI && allow_openai_oauth {
                anyhow::anyhow!(
                    "{err} You can also run `dirge auth openai` to use a stored OpenAI OAuth login."
                )
            } else {
                err
            }
        })
}

pub(crate) fn load_fresh_openai_oauth() -> anyhow::Result<Option<OpenAiOAuthCredential>> {
    let store = OpenAiAuthStore::default();
    load_fresh_openai_oauth_locked(&store, current_epoch_ms, |credential| {
        let refreshed = refresh_openai_credential(credential)?;
        store.save_openai(&refreshed)?;
        Ok(refreshed)
    })
}

/// Build the Codex transport for the ChatGPT/Codex OAuth path.
///
/// `is_dirge_oauth` is the provenance of `bearer`, resolved once upstream in
/// `resolve_chatgpt_auth` (dirge-8gdv.4): true only when the bearer is Dirge's
/// own refreshable OAuth token, false for `CODEX_ACCESS_TOKEN` env and legacy
/// `codex login` file tokens (which Dirge cannot refresh — dirge-30nl).
///
/// The refresh seam is keyed off that flag rather than re-comparing `bearer`
/// against a freshly loaded credential. The old comparison had a TOCTOU: if
/// the stored token rotated between header resolution and here, the second
/// load returned a different token, the comparison failed, and the session
/// froze the now-stale `bearer` — 401s all session with a fresh token on disk.
fn codex_http_client_for(bearer: &str, is_dirge_oauth: bool) -> CodexHttpClient {
    codex_http_client_from(
        bearer,
        is_dirge_oauth,
        load_fresh_openai_oauth().ok().flatten(),
    )
}

/// Testable core of [`codex_http_client_for`] — the `loaded` credential is
/// injected so the rotation and absent-credential cases can be exercised
/// without touching the on-disk store.
fn codex_http_client_from(
    bearer: &str,
    is_dirge_oauth: bool,
    loaded: Option<OpenAiOAuthCredential>,
) -> CodexHttpClient {
    if !is_dirge_oauth {
        return CodexHttpClient::default();
    }
    // Prefer the freshly loaded credential's token over the passed `bearer`:
    // if a rotation happened between resolution and now, `loaded` holds the
    // current token while `bearer` is stale. Fall back to `bearer` only when
    // the credential is momentarily unavailable, so the seam still refreshes
    // on the next expiry rather than degrading to a frozen default.
    let (seed_token, expires_at) = match loaded {
        Some(credential) => (
            credential.access_token().to_string(),
            Some(credential.expires_at_epoch_ms()),
        ),
        None => (bearer.to_string(), None),
    };
    let refresher: super::codex_http::CodexRefreshFn = std::sync::Arc::new(|| {
        let credential = load_fresh_openai_oauth()?.ok_or_else(|| {
            anyhow::anyhow!("stored OpenAI OAuth credential is no longer available")
        })?;
        Ok(super::auth::RefreshedAuth {
            bearer_token: credential.access_token().to_string(),
            expires_at_ms: Some(credential.expires_at_epoch_ms()),
        })
    });
    CodexHttpClient::new_refreshable(seed_token, expires_at, refresher)
}

/// Load a fresh OpenAI OAuth credential, refreshing under a cross-process lock.
///
/// OpenAI rotates the refresh token on every use, so two Dirge processes that
/// both refresh the same stale credential would have the loser persist a
/// now-dead refresh token over the winner's fresh one — the next expiry then
/// fails and forces a re-login. Take an advisory lock on the auth file around
/// load→refresh→save and re-check freshness after acquiring it, so a process
/// that lost the race adopts the winner's result instead of refreshing again
/// (dirge-m1o5). The fast path (fresh or absent credential) skips the lock.
fn load_fresh_openai_oauth_locked(
    store: &OpenAiAuthStore,
    now: impl Fn() -> i64,
    refresh_and_save: impl FnOnce(&OpenAiOAuthCredential) -> anyhow::Result<OpenAiOAuthCredential>,
) -> anyhow::Result<Option<OpenAiOAuthCredential>> {
    match store.load_openai()? {
        Some(credential) if credential.is_fresh_at(now()) => return Ok(Some(credential)),
        None => return Ok(None),
        _ => {}
    }
    let _lock = crate::auth::file_lock::FileLock::acquire_for(store.path());
    fresh_openai_oauth_at(store.load_openai()?, now(), refresh_and_save)
}

fn fresh_openai_oauth_at(
    credential: Option<OpenAiOAuthCredential>,
    epoch_ms: i64,
    refresh: impl FnOnce(&OpenAiOAuthCredential) -> anyhow::Result<OpenAiOAuthCredential>,
) -> anyhow::Result<Option<OpenAiOAuthCredential>> {
    let Some(credential) = credential else {
        return Ok(None);
    };
    if credential.is_fresh_at(epoch_ms) {
        return Ok(Some(credential));
    }
    if credential.refresh_token().trim().is_empty() {
        anyhow::bail!(
            "Stored OpenAI OAuth credential is expired and has no refresh token; run `dirge auth openai` again or set OPENAI_API_KEY."
        );
    }
    let refreshed = refresh(&credential).map_err(|_err| {
        anyhow::anyhow!(
            "Stored OpenAI OAuth credential is expired and could not be refreshed; run `dirge auth openai` again or set OPENAI_API_KEY."
        )
    })?;
    Ok(Some(refreshed))
}

/// Exchange the credential's refresh token for a fresh access token.
///
/// Runs the async refresh on a dedicated thread+runtime so it works whether or
/// not the caller is already inside a Tokio runtime (mirrors the Anthropic
/// refresh bridge in `provider/auth.rs`).
fn refresh_openai_credential(
    credential: &OpenAiOAuthCredential,
) -> anyhow::Result<OpenAiOAuthCredential> {
    let refresh_token = credential.refresh_token().to_string();
    let prior_id_token = credential.id_token().map(str::to_string);
    let prior_account_id = credential.account_id().map(str::to_string);
    let tokens = std::thread::spawn(
        move || -> anyhow::Result<crate::auth::openai_oauth::OAuthTokens> {
            let runtime = tokio::runtime::Runtime::new()?;
            let flow = crate::auth::openai_oauth::OpenAiOAuthFlow::new(
                crate::auth::openai_device::DEFAULT_ISSUER,
                crate::auth::openai_device::DEFAULT_CLIENT_ID,
                crate::auth::openai_device::ReqwestDeviceAuthHttp::default(),
            );
            Ok(runtime.block_on(flow.refresh_access_token(&refresh_token))?)
        },
    )
    .join()
    .map_err(|panic| anyhow::anyhow!("OpenAI OAuth refresh thread panicked: {panic:?}"))??;

    let mut tokens = tokens;
    if tokens.id_token.trim().is_empty()
        && let Some(prior) = prior_id_token
    {
        tokens.id_token = prior;
    }
    if tokens.account_id.is_none() {
        tokens.account_id = prior_account_id;
    }
    Ok(crate::auth::command::oauth_tokens_to_credential(
        tokens,
        current_epoch_ms(),
    ))
}

fn current_epoch_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::OpenAiOAuthCredential;
    use crate::config::{ProviderAuth, ProviderEntry};
    use std::cell::Cell;
    use std::collections::HashMap;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // ── dirge-ro8g: anthropic-OAuth presence implies Anthropic auth ──

    #[test]
    fn anthropic_oauth_presence_implies_anthropic_auth() {
        use crate::config::ProviderAuth;
        // No explicit auth + anthropic + OAuth present → Anthropic.
        assert_eq!(
            effective_auth(None, ProviderKind::Anthropic, true),
            ProviderAuth::Anthropic
        );
        // No OAuth present → the ApiKey default.
        assert_eq!(
            effective_auth(None, ProviderKind::Anthropic, false),
            ProviderAuth::ApiKey
        );
        // An EXPLICIT api_key choice is honored even with OAuth present.
        assert_eq!(
            effective_auth(Some(ProviderAuth::ApiKey), ProviderKind::Anthropic, true),
            ProviderAuth::ApiKey
        );
        // A non-anthropic provider never gets Anthropic auth implied.
        assert_eq!(
            effective_auth(None, ProviderKind::DeepSeek, true),
            ProviderAuth::ApiKey
        );
    }

    // ── dirge-pkh1: base-URL resolution precedence + scheme validation ──

    #[test]
    fn base_url_config_wins_over_env_and_default() {
        // DeepSeek/GLM used to IGNORE config base_url; now it wins.
        let got = resolve_provider_base_url(
            ProviderKind::DeepSeek,
            Some("https://proxy.internal/v1".to_string()),
            |_| Some("https://env.example/v1".to_string()),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("https://proxy.internal/v1"));
    }

    #[test]
    fn base_url_env_wins_over_default_when_no_config() {
        let got = resolve_provider_base_url(ProviderKind::Glm, None, |v| {
            (v == "GLM_BASE_URL").then(|| "https://glm.proxy/v4".to_string())
        })
        .unwrap();
        assert_eq!(got.as_deref(), Some("https://glm.proxy/v4"));
    }

    #[test]
    fn base_url_falls_back_to_default() {
        assert_eq!(
            resolve_provider_base_url(ProviderKind::DeepSeek, None, no_env)
                .unwrap()
                .as_deref(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(
            resolve_provider_base_url(ProviderKind::Glm, None, no_env)
                .unwrap()
                .as_deref(),
            Some("https://open.bigmodel.cn/api/coding/paas/v4")
        );
    }

    #[test]
    fn base_url_env_http_is_rejected() {
        let err = resolve_provider_base_url(ProviderKind::DeepSeek, None, |_| {
            Some("http://api.deepseek.com/v1".to_string())
        })
        .unwrap_err();
        assert!(err.to_string().contains("https://"), "{err}");
    }

    #[test]
    fn base_url_custom_config_over_env_no_default() {
        let cfg = resolve_provider_base_url(
            ProviderKind::Custom,
            Some("https://cfg.example".to_string()),
            |_| Some("https://env.example".to_string()),
        )
        .unwrap();
        assert_eq!(cfg.as_deref(), Some("https://cfg.example"));
        let env_only = resolve_provider_base_url(ProviderKind::Custom, None, |v| {
            (v == "CUSTOM_BASE_URL").then(|| "https://env.example".to_string())
        })
        .unwrap();
        assert_eq!(env_only.as_deref(), Some("https://env.example"));
        assert_eq!(
            resolve_provider_base_url(ProviderKind::Custom, None, no_env).unwrap(),
            None
        );
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_client_oauth_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn auth_path(&self) -> std::path::PathBuf {
            self.0.join("auth.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn oauth(access_token: &str) -> OpenAiOAuthCredential {
        oauth_with_account(access_token, None)
    }

    fn oauth_with_account(access_token: &str, account_id: Option<&str>) -> OpenAiOAuthCredential {
        OpenAiOAuthCredential::new(
            access_token,
            "REFRESH-TOKEN",
            Some("ID-TOKEN".to_string()),
            account_id.map(str::to_string),
            i64::MAX,
        )
    }

    fn test_chatgpt_headers() -> ProviderAuthHeaders {
        ProviderAuthHeaders {
            bearer_token: "test-token".to_string(),
            chatgpt_account_id: Some("acct-test".to_string()),
            chatgpt_bearer_is_dirge_oauth: false,
        }
    }

    fn test_anthropic_headers() -> ProviderAuthHeaders {
        ProviderAuthHeaders {
            bearer_token: "sk-ant-oat-test".to_string(),
            chatgpt_account_id: None,
            chatgpt_bearer_is_dirge_oauth: false,
        }
    }

    #[test]
    fn api_key_billing_fallback_client_builds_only_for_canonical_openai() {
        let client = create_openai_api_key_fallback_client_with_env(
            "openai",
            None,
            &HashMap::new(),
            |name| (name == "OPENAI_API_KEY").then(|| "env-key".to_string()),
        )
        .unwrap()
        .unwrap();

        let AnyClient::OpenAI(_) = client else {
            panic!("API-key billing fallback must use the OpenAI API client");
        };
    }

    #[test]
    fn api_key_billing_fallback_skips_openai_base_url_and_aliases() {
        let configured_openai = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                base_url: Some("https://proxy.example.com/v1".to_string()),
                ..Default::default()
            },
        )]);
        assert!(
            create_openai_api_key_fallback_client_with_env(
                "openai",
                Some("api-key"),
                &configured_openai,
                no_env,
            )
            .unwrap()
            .is_none()
        );

        let alias = HashMap::from([(
            "local-vllm".to_string(),
            ProviderEntry {
                provider_type: Some("openai".to_string()),
                base_url: Some("http://localhost:11434/v1".to_string()),
                allow_insecure: true,
                multimodal: None,
                ..Default::default()
            },
        )]);
        assert!(
            create_openai_api_key_fallback_client_with_env(
                "local-vllm",
                Some("api-key"),
                &alias,
                no_env,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn openai_oauth_wins_over_cli_key_as_subscription_default() {
        let loaded = Cell::new(false);

        let credential = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            Some("cli-key"),
            no_env,
            || {
                loaded.set(true);
                Ok(Some(oauth("oauth-token")))
            },
        )
        .unwrap();

        let ProviderCredential::OpenAiOAuth {
            access_token: token,
            ..
        } = credential
        else {
            panic!("stored OpenAI OAuth must win over CLI API key billing fallback");
        };
        assert_eq!(token, "oauth-token");
        assert!(
            loaded.get(),
            "OAuth-first OpenAI auth must read the Dirge auth store before API-key fallback"
        );
    }

    #[test]
    fn openai_oauth_wins_over_default_env_key_as_subscription_default() {
        let loaded = Cell::new(false);

        let credential = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            None,
            |name| (name == "OPENAI_API_KEY").then(|| "env-key".to_string()),
            || {
                loaded.set(true);
                Ok(Some(oauth("oauth-token")))
            },
        )
        .unwrap();

        let ProviderCredential::OpenAiOAuth {
            access_token: token,
            ..
        } = credential
        else {
            panic!("stored OpenAI OAuth must win over OPENAI_API_KEY billing fallback");
        };
        assert_eq!(token, "oauth-token");
        assert!(
            loaded.get(),
            "OAuth-first OpenAI auth must read the Dirge auth store before OPENAI_API_KEY fallback"
        );
    }

    #[test]
    fn openai_api_key_is_used_when_subscription_oauth_is_absent() {
        let loaded = Cell::new(false);

        let credential = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            None,
            |name| (name == "OPENAI_API_KEY").then(|| "env-key".to_string()),
            || {
                loaded.set(true);
                Ok(None)
            },
        )
        .unwrap();

        let ProviderCredential::ApiKey(token) = credential else {
            panic!("OPENAI_API_KEY remains the fallback when no stored OAuth credential exists");
        };
        assert_eq!(token, "env-key");
        assert!(
            loaded.get(),
            "OAuth-first OpenAI auth must check for a stored login before API-key fallback"
        );
    }

    #[test]
    fn expired_openai_oauth_does_not_block_api_key_fallback() {
        let credential = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            None,
            |name| (name == "OPENAI_API_KEY").then(|| "env-key".to_string()),
            || {
                fresh_openai_oauth_at(Some(oauth("ACCESS-TOKEN")), i64::MAX, |_| {
                    anyhow::bail!("refresh unavailable in test")
                })
            },
        )
        .unwrap();

        let ProviderCredential::ApiKey(token) = credential else {
            panic!("OPENAI_API_KEY must remain fallback when stored OAuth is expired");
        };
        assert_eq!(token, "env-key");
    }

    #[test]
    fn openai_oauth_credential_carries_account_id_for_codex_requests() {
        let credential = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            Some("cli-api-key"),
            no_env,
            || {
                Ok(Some(oauth_with_account(
                    "oauth-token",
                    Some("acct-provider"),
                )))
            },
        )
        .unwrap();

        let ProviderCredential::OpenAiOAuth {
            access_token,
            account_id,
        } = credential
        else {
            panic!("stored OpenAI OAuth must be selected before API-key billing fallback");
        };
        assert_eq!(access_token, "oauth-token");
        assert_eq!(account_id.as_deref(), Some("acct-provider"));
    }

    #[test]
    fn openai_oauth_fallback_builds_chatgpt_codex_client() {
        let client = create_client_with("openai", None, &HashMap::new(), no_env, || {
            Ok(Some(oauth("oauth-token")))
        })
        .unwrap();

        match client {
            AnyClient::OpenAICodex(client) => {
                assert_eq!(client.base_url(), CHATGPT_CODEX_BASE_URL);
            }
            _ => panic!("OAuth fallback must use the ChatGPT Codex client"),
        }
    }

    #[test]
    fn configured_openai_base_url_does_not_fallback_to_oauth() {
        let loaded = Cell::new(false);
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                provider_type: Some("openai".to_string()),
                base_url: Some("https://proxy.invalid/v1".to_string()),
                ..Default::default()
            },
        )]);

        let result = create_client_with("openai", None, &providers, no_env, || {
            loaded.set(true);
            Ok(Some(oauth("oauth-token")))
        });
        let err = match result {
            Ok(_) => panic!("configured OpenAI base_url must not use OAuth fallback"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("OPENAI_API_KEY"), "unexpected error: {err}");
        assert!(
            !loaded.get(),
            "configured OpenAI base_url must not read the Dirge OAuth store"
        );
    }

    #[test]
    fn openai_oauth_fallback_builds_a_codex_client_and_passes_the_name_through() {
        let client = create_client_with("openai", None, &HashMap::new(), no_env, || {
            Ok(Some(oauth("oauth-token")))
        })
        .unwrap();

        // completion_model no longer remaps (dirge-ovjk) — the Codex default
        // is resolved upstream by resolve_model_name. Here the OAuth fallback
        // still builds a Codex *client*; the name passes through verbatim.
        let model = client.completion_model("gpt-5.5");

        match model {
            crate::provider::AnyModel::OpenAICodex(model) => {
                assert_eq!(model.model, "gpt-5.5");
            }
            _ => panic!("OAuth fallback must build a ChatGPT Codex model"),
        }
    }

    #[test]
    fn resolve_model_name_ties_provenance_to_a_real_codex_client() {
        use crate::provider::resolve_model_name;

        // A Dirge OAuth fallback builds a Codex client.
        let codex = create_client_with("openai", None, &HashMap::new(), no_env, || {
            Ok(Some(oauth("oauth-token")))
        })
        .unwrap();
        assert!(codex.is_codex());
        // Defaulted OpenAI id -> Codex default; explicit gpt-4o preserved
        // (dirge-ovjk); a non-default name is untouched.
        assert_eq!(resolve_model_name(&codex, "gpt-4o", false), "gpt-5.5");
        assert_eq!(resolve_model_name(&codex, "gpt-4o", true), "gpt-4o");
        assert_eq!(resolve_model_name(&codex, "o3", false), "o3");

        // A plain API-key OpenAI client is not Codex — never remapped.
        let plain = create_client_with("openai", Some("sk-test"), &HashMap::new(), no_env, || {
            Ok(None)
        })
        .unwrap();
        assert!(!plain.is_codex());
        assert_eq!(resolve_model_name(&plain, "gpt-4o", false), "gpt-4o");
    }

    #[test]
    fn oauth_fallback_is_openai_only() {
        let loaded = Cell::new(false);

        let err = resolve_provider_credential(
            false,
            ProviderKind::Anthropic,
            None,
            None,
            None,
            no_env,
            || {
                loaded.set(true);
                Ok(Some(oauth("oauth-token")))
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("ANTHROPIC_API_KEY"));
        assert!(
            !loaded.get(),
            "non-OpenAI providers must not read OpenAI auth"
        );
    }

    #[test]
    fn openai_compatible_alias_does_not_fallback_to_oauth() {
        let loaded = Cell::new(false);
        let providers = HashMap::from([(
            "local-vllm".to_string(),
            ProviderEntry {
                provider_type: Some("openai".to_string()),
                base_url: Some("http://localhost:11434/v1".to_string()),
                allow_insecure: true,
                multimodal: None,
                ..Default::default()
            },
        )]);

        let result = create_client_with("local-vllm", None, &providers, no_env, || {
            loaded.set(true);
            Ok(Some(oauth("oauth-token")))
        });
        let err = match result {
            Ok(_) => panic!("OpenAI-compatible custom alias must not use OAuth fallback"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("OPENAI_API_KEY"), "unexpected error: {err}");
        assert!(
            !loaded.get(),
            "OpenAI-compatible custom aliases must not read the Dirge OAuth store"
        );
    }

    #[test]
    fn missing_openai_oauth_fallback_points_to_login_command() {
        let err = resolve_provider_credential(
            true,
            ProviderKind::OpenAI,
            None,
            None,
            None,
            no_env,
            || Ok(None),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("OPENAI_API_KEY"));
        assert!(err.contains("dirge auth openai"));
    }

    #[test]
    fn expired_openai_oauth_error_is_actionable_and_redacted() {
        let err = fresh_openai_oauth_at(Some(oauth("ACCESS-TOKEN")), i64::MAX, |_| {
            anyhow::bail!("refresh unavailable in test")
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("dirge auth openai"));
        for secret in ["ACCESS-TOKEN", "REFRESH-TOKEN", "ID-TOKEN"] {
            assert!(!err.contains(secret), "expired-token error leaked {secret}");
        }
    }

    #[test]
    fn expired_openai_oauth_is_refreshed_when_refresh_succeeds() {
        let refreshed = OpenAiOAuthCredential::new(
            "NEW-ACCESS",
            "NEW-REFRESH",
            Some("NEW-ID".to_string()),
            Some("acct".to_string()),
            i64::MAX,
        );
        let result = fresh_openai_oauth_at(Some(oauth("OLD-ACCESS")), i64::MAX, |cred| {
            assert_eq!(cred.refresh_token(), "REFRESH-TOKEN");
            Ok(refreshed.clone())
        })
        .unwrap()
        .expect("refreshed credential returned");

        assert_eq!(result.access_token(), "NEW-ACCESS");
    }

    #[test]
    fn fresh_openai_oauth_does_not_refresh() {
        let result = fresh_openai_oauth_at(Some(oauth("ACCESS-TOKEN")), 0, |_| {
            panic!("must not refresh a fresh credential")
        })
        .unwrap()
        .expect("fresh credential returned");

        assert_eq!(result.access_token(), "ACCESS-TOKEN");
    }

    #[test]
    fn codex_transport_keys_refresh_off_provenance_not_token_equality() {
        // Env / legacy-file bearer (is_dirge_oauth = false) stays frozen even
        // when a Dirge OAuth credential happens to be present -> no override.
        let credential = oauth("DIRGE-OAUTH-ACCESS");
        assert!(
            !codex_http_client_from("CODEX-ENV-TOKEN", false, Some(credential.clone()))
                .is_refreshable()
        );

        // Dirge OAuth bearer that still matches the loaded credential ->
        // refresh seam attached.
        assert!(
            codex_http_client_from("DIRGE-OAUTH-ACCESS", true, Some(credential.clone()))
                .is_refreshable()
        );

        // dirge-8gdv.4: the token rotated between resolution and here, so the
        // passed bearer no longer equals the loaded credential. The old
        // equality check would freeze the stale bearer; provenance keying
        // still attaches the seam (seeded from the fresh credential).
        let rotated = oauth("ROTATED-FRESH-ACCESS");
        assert!(
            codex_http_client_from("DIRGE-OAUTH-ACCESS-STALE", true, Some(rotated))
                .is_refreshable()
        );

        // Provenance says Dirge OAuth but the credential is momentarily gone:
        // still attach the seam (seeded from the passed bearer) so a later
        // expiry can refresh, rather than degrading to a frozen default.
        assert!(codex_http_client_from("DIRGE-OAUTH-ACCESS", true, None).is_refreshable());
    }

    #[test]
    fn locked_load_takes_fast_path_for_fresh_credential() {
        let dir = TestDir::new("fast");
        let store = OpenAiAuthStore::at(dir.auth_path());
        store
            .save_openai(&OpenAiOAuthCredential::new(
                "ACCESS",
                "R0",
                None,
                None,
                i64::MAX,
            ))
            .unwrap();

        let result = load_fresh_openai_oauth_locked(
            &store,
            || 0,
            |_| panic!("a fresh credential must not be refreshed"),
        )
        .unwrap()
        .expect("fresh credential returned");

        assert_eq!(result.access_token(), "ACCESS");
    }

    #[test]
    fn concurrent_locked_refresh_rotates_token_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = TestDir::new("m1o5");
        let path = dir.auth_path();
        // Seed an expired credential whose single-use refresh token is "R0".
        OpenAiAuthStore::at(path.clone())
            .save_openai(&OpenAiOAuthCredential::new(
                "OLD-ACCESS",
                "R0",
                None,
                None,
                0,
            ))
            .unwrap();

        let refreshes = Arc::new(AtomicUsize::new(0));
        let now = || 1_900_000_000_000_i64;

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                let refreshes = refreshes.clone();
                std::thread::spawn(move || {
                    let store = OpenAiAuthStore::at(path);
                    load_fresh_openai_oauth_locked(&store, now, |cred| {
                        // Single-use: the token being rotated is still the
                        // original — no one clobbered it back to "R0".
                        assert_eq!(cred.refresh_token(), "R0");
                        refreshes.fetch_add(1, Ordering::SeqCst);
                        let refreshed =
                            OpenAiOAuthCredential::new("NEW-ACCESS", "R1", None, None, i64::MAX);
                        store.save_openai(&refreshed)?;
                        Ok(refreshed)
                    })
                    .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            1,
            "the stale credential must be refreshed exactly once across processes"
        );
        for result in &results {
            let cred = result.as_ref().expect("a fresh credential");
            assert_eq!(cred.access_token(), "NEW-ACCESS");
        }
        // The winner's rotated refresh token survives; it isn't overwritten.
        let stored = OpenAiAuthStore::at(path).load_openai().unwrap().unwrap();
        assert_eq!(stored.refresh_token(), "R1");
    }

    #[test]
    fn provider_credential_debug_redacts_selected_secrets() {
        let oauth_debug = format!(
            "{:?}",
            ProviderCredential::OpenAiOAuth {
                access_token: "ACCESS-TOKEN".to_string(),
                account_id: Some("acct-debug".to_string()),
            }
        );
        let chatgpt_debug = format!(
            "{:?}",
            ProviderCredential::ChatGptAuth("CHATGPT-TOKEN".to_string())
        );
        let api_key_debug = format!("{:?}", ProviderCredential::ApiKey("API-KEY".to_string()));

        assert!(!oauth_debug.contains("ACCESS-TOKEN"));
        assert!(!chatgpt_debug.contains("CHATGPT-TOKEN"));
        assert!(!api_key_debug.contains("API-KEY"));
        assert!(oauth_debug.contains("[REDACTED]"));
        assert!(chatgpt_debug.contains("[REDACTED]"));
        assert!(api_key_debug.contains("[REDACTED]"));
    }

    #[test]
    fn top_level_auth_can_default_provider_entry_auth() {
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                model: Some("gpt-5.5".to_string()),
                ..Default::default()
            },
        )]);
        let info = resolve_provider_info("openai", &providers).unwrap();

        assert_eq!(
            info.auth.or(Some(ProviderAuth::ChatGpt)),
            Some(ProviderAuth::ChatGpt)
        );
    }

    #[test]
    fn provider_auth_overrides_top_level_default() {
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                auth: Some(ProviderAuth::ApiKey),
                ..Default::default()
            },
        )]);
        let info = resolve_provider_info("openai", &providers).unwrap();

        assert_eq!(
            info.auth.or(Some(ProviderAuth::ChatGpt)),
            Some(ProviderAuth::ApiKey)
        );
    }

    #[test]
    fn api_key_openai_uses_chat_completions_client() {
        let providers = HashMap::new();

        let client = create_client_with("openai", Some("test-api-key"), &providers, no_env, || {
            Ok(None)
        })
        .unwrap();

        let crate::provider::AnyClient::OpenAI(_) = client else {
            panic!("expected API-key OpenAI to use Chat Completions client");
        };
    }

    #[test]
    fn chatgpt_auth_rejected_for_non_openai_provider() {
        let providers = HashMap::new();
        let msg = match create_client_with_chatgpt_auth_headers(
            "anthropic",
            &providers,
            test_chatgpt_headers(),
        ) {
            Ok(_) => panic!("chatgpt auth on a non-openai provider must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("only supported for the `openai` provider"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("anthropic"),
            "error should name the provider: {msg}"
        );
    }

    #[test]
    fn chatgpt_auth_refuses_insecure_base_url_even_with_allow_insecure() {
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                base_url: Some("http://proxy.local/openai".to_string()),
                allow_insecure: true,
                multimodal: None,
                ..Default::default()
            },
        )]);
        let msg = match create_client_with_chatgpt_auth_headers(
            "openai",
            &providers,
            test_chatgpt_headers(),
        ) {
            Ok(_) => panic!("http base url must be refused under chatgpt auth"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("https base URL"), "unexpected error: {msg}");
    }

    #[test]
    fn anthropic_oauth_refuses_insecure_base_url_even_with_allow_insecure() {
        let providers = HashMap::from([(
            "anthropic".to_string(),
            ProviderEntry {
                base_url: Some("http://proxy.local/anthropic".to_string()),
                allow_insecure: true,
                ..Default::default()
            },
        )]);
        let msg = match create_client_with_anthropic_auth_headers(
            "anthropic",
            &providers,
            test_anthropic_headers(),
        ) {
            Ok(_) => panic!("http base url must be refused under anthropic oauth"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("https base URL"), "unexpected error: {msg}");
    }

    #[test]
    fn anthropic_oauth_allows_https_base_url() {
        let providers = HashMap::from([(
            "anthropic".to_string(),
            ProviderEntry {
                base_url: Some("https://proxy.example.com/anthropic".to_string()),
                ..Default::default()
            },
        )]);
        let client = create_client_with_anthropic_auth_headers(
            "anthropic",
            &providers,
            test_anthropic_headers(),
        )
        .unwrap();
        assert!(matches!(client, AnyClient::AnthropicOauth(_)));
    }

    #[test]
    fn chatgpt_auth_openai_uses_codex_backend_by_default() {
        let providers = HashMap::new();

        let client =
            create_client_with_chatgpt_auth_headers("openai", &providers, test_chatgpt_headers())
                .unwrap();

        let crate::provider::AnyClient::ChatGptOpenAI(client) = client else {
            panic!("expected ChatGPT OpenAI client");
        };
        assert_eq!(client.base_url(), CHATGPT_CODEX_BASE_URL);
    }

    #[test]
    fn chatgpt_auth_openai_preserves_explicit_base_url() {
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                base_url: Some("https://proxy.example.com/openai".to_string()),
                ..Default::default()
            },
        )]);

        let client =
            create_client_with_chatgpt_auth_headers("openai", &providers, test_chatgpt_headers())
                .unwrap();

        let crate::provider::AnyClient::ChatGptOpenAI(client) = client else {
            panic!("expected ChatGPT OpenAI client");
        };
        assert_eq!(client.base_url(), "https://proxy.example.com/openai");
    }

    #[test]
    fn chatgpt_auth_requires_account_id() {
        let providers = HashMap::new();

        let result = create_client_with_chatgpt_auth_headers(
            "openai",
            &providers,
            ProviderAuthHeaders {
                bearer_token: "test-token".to_string(),
                chatgpt_account_id: None,
                chatgpt_bearer_is_dirge_oauth: false,
            },
        );
        let err = match result {
            Ok(_) => panic!("expected ChatGPT auth without account id to fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("no ChatGPT account id was found"));
    }
}
