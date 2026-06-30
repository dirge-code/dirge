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

    let auth = info.auth.or(default_auth).unwrap_or(ProviderAuth::ApiKey);
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
        (ProviderAuth::ChatGpt, Some(headers)) => Some(headers),
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
            env,
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
        ProviderKind::DeepSeek => Some(
            std::env::var("DEEPSEEK_BASE_URL")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string()),
        ),
        ProviderKind::Glm => Some(
            std::env::var("GLM_BASE_URL")
                .unwrap_or_else(|_| "https://open.bigmodel.cn/api/coding/paas/v4".to_string()),
        ),
        ProviderKind::Custom => info
            .base_url
            .or_else(|| std::env::var("CUSTOM_BASE_URL").ok()),
        ProviderKind::OpenAI if uses_openai_oauth => Some(CHATGPT_CODEX_BASE_URL.to_string()),
        ProviderKind::OpenAI if is_chatgpt_auth => info
            .base_url
            .or_else(|| Some(CHATGPT_CODEX_BASE_URL.to_string())),
        _ => info.base_url,
    };

    // A Codex login token is higher-value than a per-provider API key, so it
    // must never leave over plaintext. `allow_insecure` is intentionally not
    // honored for either explicit ChatGPT auth or native Dirge OAuth fallback.
    if (is_chatgpt_auth || uses_openai_oauth)
        && let Some(url) = base_url.as_deref()
        && !url.starts_with("https://")
    {
        anyhow::bail!(
            "ChatGPT (Codex) auth requires an https base URL, but got `{url}`. The Codex login \
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
            let mut b = openai::Client::builder()
                .api_key(&key)
                .http_client(CodexHttpClient::default());
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
                let bearer = auth_headers
                    .as_ref()
                    .map(|h| h.bearer_token.clone())
                    .unwrap_or_else(|| key.clone());
                let mut b = anthropic::Client::builder()
                    .api_key(&key)
                    .http_client(super::anthropic_http::AnthropicHttpClient::new(bearer));
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
    fresh_openai_oauth_at(store.load_openai()?, current_epoch_ms(), |credential| {
        let refreshed = refresh_openai_credential(credential)?;
        store.save_openai(&refreshed)?;
        Ok(refreshed)
    })
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
    fn openai_oauth_fallback_maps_openai_default_model_to_codex_default() {
        let client = create_client_with("openai", None, &HashMap::new(), no_env, || {
            Ok(Some(oauth("oauth-token")))
        })
        .unwrap();

        let model = client.completion_model(crate::provider::default_model_for("openai"));

        match model {
            crate::provider::AnyModel::OpenAICodex(model) => {
                assert_eq!(model.model, "gpt-5.5");
            }
            _ => panic!("OAuth fallback must build a ChatGPT Codex model"),
        }
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
            },
        );
        let err = match result {
            Ok(_) => panic!("expected ChatGPT auth without account id to fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("no ChatGPT account id was found"));
    }
}
