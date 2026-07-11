use std::path::PathBuf;

use crate::auth::openai_oauth::normalize_optional_string;
use crate::auth::store::OpenAiOAuthCredential;
use crate::config::ProviderAuth;

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthHeaders {
    pub bearer_token: String,
    pub chatgpt_account_id: Option<String>,
    /// dirge-8gdv.4: true only when `bearer_token` is Dirge's own refreshable
    /// OpenAI OAuth access token (the ChatGPT path's middle branch). Env
    /// (`CODEX_ACCESS_TOKEN`) and legacy `codex login` file tokens set this
    /// false — Dirge can't refresh them (dirge-30nl). The Codex transport
    /// keys the refresh seam off this flag rather than comparing the bearer
    /// against a second, independently-loaded credential, which a token
    /// rotation between the two loads would break.
    pub chatgpt_bearer_is_dirge_oauth: bool,
}

// Hand-written so the live ChatGPT bearer token can never land in a log or
// panic message via `{:?}` — the derived Debug would print it verbatim.
impl std::fmt::Debug for ProviderAuthHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAuthHeaders")
            .field(
                "bearer_token",
                &if self.bearer_token.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("chatgpt_account_id", &self.chatgpt_account_id)
            .field(
                "chatgpt_bearer_is_dirge_oauth",
                &self.chatgpt_bearer_is_dirge_oauth,
            )
            .finish()
    }
}

pub fn resolve_auth_headers(auth: ProviderAuth) -> anyhow::Result<Option<ProviderAuthHeaders>> {
    match auth {
        ProviderAuth::ApiKey => Ok(None),
        ProviderAuth::ChatGpt => Ok(Some(resolve_chatgpt_auth()?)),
        ProviderAuth::Anthropic => Ok(Some(resolve_anthropic_auth()?)),
    }
}

fn resolve_chatgpt_auth() -> anyhow::Result<ProviderAuthHeaders> {
    resolve_chatgpt_auth_from(
        std::env::var("CODEX_ACCESS_TOKEN").ok(),
        std::env::var("CHATGPT_ACCOUNT_ID").ok(),
        codex_auth_file_path(),
    )
}

fn resolve_chatgpt_auth_from(
    codex_access_token: Option<String>,
    chatgpt_account_id: Option<String>,
    auth_file_path: PathBuf,
) -> anyhow::Result<ProviderAuthHeaders> {
    resolve_chatgpt_auth_from_with_dirge_oauth(
        codex_access_token,
        chatgpt_account_id,
        auth_file_path,
        crate::provider::client::load_fresh_openai_oauth,
    )
}

fn resolve_chatgpt_auth_from_with_dirge_oauth(
    codex_access_token: Option<String>,
    chatgpt_account_id: Option<String>,
    auth_file_path: PathBuf,
    load_openai_oauth: impl FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
) -> anyhow::Result<ProviderAuthHeaders> {
    if let Some(token) = codex_access_token
        && !token.trim().is_empty()
    {
        return Ok(ProviderAuthHeaders {
            bearer_token: token.trim().to_string(),
            chatgpt_account_id: normalize_optional_string(chatgpt_account_id),
            // CODEX_ACCESS_TOKEN env override — not Dirge-refreshable.
            chatgpt_bearer_is_dirge_oauth: false,
        });
    }

    // `dirge auth openai` writes Dirge's own refreshable OAuth credential.
    // Honor it for explicit `auth: chatgpt` before falling back to legacy
    // `codex login` storage; otherwise a stale ~/.codex/auth.json can keep
    // winning even after the user has completed the newer Dirge login flow.
    //
    // dirge-cu44: a present-but-unusable Dirge credential (expired with no
    // refresh token, refresh failed, or unreadable store) surfaces as Err.
    // Don't propagate it — that would hard-error a session a valid legacy
    // codex file could still serve. Warn and fall through instead.
    match load_openai_oauth() {
        Ok(Some(credential)) => {
            return Ok(ProviderAuthHeaders {
                bearer_token: credential.access_token().to_string(),
                // dirge-l7xy: fall back to the CHATGPT_ACCOUNT_ID env value when
                // the credential carries no account id, mirroring the env-token
                // branch — otherwise setting the env var did nothing here.
                chatgpt_account_id: credential
                    .account_id()
                    .map(str::to_string)
                    .or_else(|| normalize_optional_string(chatgpt_account_id.clone())),
                // Dirge's own OAuth token — refreshable mid-session.
                chatgpt_bearer_is_dirge_oauth: true,
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                target: "dirge::provider",
                error = %e,
                "Dirge OpenAI OAuth credential present but unusable; falling back to legacy codex auth storage",
            );
        }
    }

    let raw = std::fs::read_to_string(&auth_file_path).map_err(|e| {
        anyhow::anyhow!(
            "ChatGPT auth requested, but CODEX_ACCESS_TOKEN is unset and Codex auth storage could not be read at {}: {e}. Run `codex login` or set CODEX_ACCESS_TOKEN.",
            auth_file_path.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "ChatGPT auth requested, but Codex auth storage at {} is not valid JSON: {e}",
            auth_file_path.display()
        )
    })?;

    let bearer_token = extract_string_by_keys(&json, &["access_token", "accessToken"])
        .ok_or_else(|| {
            anyhow::anyhow!(
                "ChatGPT auth requested, but no access token was found in {}. Run `codex login` again or set CODEX_ACCESS_TOKEN.",
                auth_file_path.display()
            )
        })?;
    let chatgpt_account_id = extract_string_by_keys(
        &json,
        &[
            "chatgpt_account_id",
            "chatgptAccountId",
            "chatgpt_account",
            "account_id",
            "accountId",
        ],
    )
    // dirge-l7xy: a legacy auth.json whose token lacks an account id used to
    // fail with "set CHATGPT_ACCOUNT_ID", but setting it did nothing because
    // this branch read only the file. Fall back to the env value.
    .or_else(|| normalize_optional_string(chatgpt_account_id));

    Ok(ProviderAuthHeaders {
        bearer_token,
        chatgpt_account_id,
        // Legacy `codex login` file token — not Dirge-refreshable.
        chatgpt_bearer_is_dirge_oauth: false,
    })
}

fn codex_auth_file_path() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home).join("auth.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("auth.json")
}

/// An Anthropic OAuth bearer resolved together with its expiry. The expiry
/// lets the transport know WHEN to refresh mid-session (dirge-956a); it is
/// `None` for the static `ANTHROPIC_OAUTH_TOKEN` env path, which has no
/// refresh token and so can never be renewed.
#[derive(Clone)]
pub(crate) struct RefreshedAuth {
    pub bearer_token: String,
    pub expires_at_ms: Option<i64>,
}

fn resolve_anthropic_auth() -> anyhow::Result<ProviderAuthHeaders> {
    resolve_anthropic_auth_from(
        std::env::var("ANTHROPIC_OAUTH_TOKEN").ok(),
        anthropic_credentials_file_path(),
    )
}

/// Thin wrapper that drops the expiry — kept so the resolver tests and any
/// bearer-only caller read unchanged.
fn resolve_anthropic_auth_from(
    oauth_token: Option<String>,
    credentials_file_path: PathBuf,
) -> anyhow::Result<ProviderAuthHeaders> {
    let resolved = resolve_anthropic_auth_with_expiry_from(oauth_token, credentials_file_path)?;
    Ok(ProviderAuthHeaders {
        bearer_token: resolved.bearer_token,
        chatgpt_account_id: None,
        // Anthropic path — the ChatGPT/Codex flag does not apply.
        chatgpt_bearer_is_dirge_oauth: false,
    })
}

/// Resolve the Anthropic OAuth bearer AND its expiry, refreshing (and
/// persisting) a stale file credential along the way. Shared by the initial
/// client build and by the transport's mid-session refresh seam so both go
/// through the exact same read → refresh-if-expired → persist path.
pub(crate) fn resolve_anthropic_auth_with_expiry() -> anyhow::Result<RefreshedAuth> {
    resolve_anthropic_auth_with_expiry_from(
        std::env::var("ANTHROPIC_OAUTH_TOKEN").ok(),
        anthropic_credentials_file_path(),
    )
}

fn resolve_anthropic_auth_with_expiry_from(
    oauth_token: Option<String>,
    credentials_file_path: PathBuf,
) -> anyhow::Result<RefreshedAuth> {
    if let Some(token) = oauth_token
        && !token.trim().is_empty()
    {
        return Ok(RefreshedAuth {
            bearer_token: token.trim().to_string(),
            expires_at_ms: None,
        });
    }

    let raw = std::fs::read_to_string(&credentials_file_path).map_err(|e| {
        anyhow::anyhow!(
            "Anthropic OAuth requested, but ANTHROPIC_OAUTH_TOKEN is unset and Claude credentials could not be read at {}: {e}. Run `claude login` or set ANTHROPIC_OAUTH_TOKEN.",
            credentials_file_path.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "Anthropic OAuth requested, but Claude credentials at {} are not valid JSON: {e}",
            credentials_file_path.display()
        )
    })?;

    let mut bearer_token = extract_string_by_keys(&json, &["accessToken", "access_token"])
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic OAuth requested, but no access token was found in {}. Run `dirge auth anthropic` again or set ANTHROPIC_OAUTH_TOKEN.",
                credentials_file_path.display()
            )
        })?;
    let mut expires_at_ms = extract_i64_by_keys(&json, &["expiresAt", "expires_at", "expires"]);

    if anthropic_token_is_expired(&json)
        && extract_string_by_keys(&json, &["refreshToken", "refresh_token"]).is_some()
    {
        // The refresh token is single-use; two processes refreshing the same
        // stale credential would leave the loser persisting a dead token over
        // the winner's fresh one. Serialize load→refresh→save across processes
        // and re-read under the lock — a concurrent process may have already
        // refreshed while we blocked (dirge-m1o5).
        let _lock = crate::auth::file_lock::FileLock::acquire_for(&credentials_file_path);
        let current = std::fs::read_to_string(&credentials_file_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| json.clone());
        if anthropic_token_is_expired(&current) {
            if let Some(refresh) =
                extract_string_by_keys(&current, &["refreshToken", "refresh_token"])
            {
                let refreshed = refresh_anthropic_token_sync(&refresh)?;
                crate::provider::anthropic_oauth::persist_credentials(&refreshed)?;
                bearer_token = refreshed.access_token;
                expires_at_ms = Some(refreshed.expires_at);
            }
        } else {
            // Someone else refreshed while we waited; adopt their fresh values.
            if let Some(token) = extract_string_by_keys(&current, &["accessToken", "access_token"])
            {
                bearer_token = token;
            }
            expires_at_ms = extract_i64_by_keys(&current, &["expiresAt", "expires_at", "expires"]);
        }
    }

    Ok(RefreshedAuth {
        bearer_token,
        expires_at_ms,
    })
}

fn anthropic_credentials_file_path() -> PathBuf {
    crate::provider::anthropic_oauth::credentials_file_path()
}

fn anthropic_token_is_expired(value: &serde_json::Value) -> bool {
    let Some(expires_at) = extract_i64_by_keys(value, &["expiresAt", "expires_at", "expires"])
    else {
        return false;
    };
    crate::auth::file_store::epoch_ms_is_expired(expires_at, chrono::Utc::now().timestamp_millis())
}

fn refresh_anthropic_token_sync(
    refresh_token: &str,
) -> anyhow::Result<crate::provider::anthropic_oauth::AnthropicOAuthCredentials> {
    let refresh_token = refresh_token.to_string();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(crate::provider::anthropic_oauth::refresh_token(
            &refresh_token,
        ))
    })
    .join()
    .map_err(|panic| anyhow::anyhow!("Anthropic OAuth refresh thread panicked: {panic:?}"))?
}

fn extract_i64_by_keys(value: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    if let serde_json::Value::Object(map) = value {
        for key in keys {
            if let Some(n) = map.get(*key).and_then(serde_json::Value::as_i64) {
                return Some(n);
            }
        }
        for child in map.values() {
            if let Some(n) = extract_i64_by_keys(child, keys) {
                return Some(n);
            }
        }
    } else if let serde_json::Value::Array(items) = value {
        for child in items {
            if let Some(n) = extract_i64_by_keys(child, keys) {
                return Some(n);
            }
        }
    }
    None
}

fn extract_string_by_keys(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    if let serde_json::Value::Object(map) = value {
        for key in keys {
            if let Some(s) = map
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(s.to_string());
            }
        }
        for child in map.values() {
            if let Some(s) = extract_string_by_keys(child, keys) {
                return Some(s);
            }
        }
    } else if let serde_json::Value::Array(items) = value {
        for child in items {
            if let Some(s) = extract_string_by_keys(child, keys) {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_codex_access_token_and_account_id() {
        let value = serde_json::json!({
            "chatgpt_auth_tokens": {
                "access_token": "token-123",
                "refresh_token": "must-not-win"
            },
            "chatgpt_account_id": "acct-456"
        });

        assert_eq!(
            extract_string_by_keys(&value, &["access_token", "accessToken"]).as_deref(),
            Some("token-123")
        );
        assert_eq!(
            extract_string_by_keys(&value, &["chatgpt_account_id"]).as_deref(),
            Some("acct-456")
        );
    }

    #[test]
    fn debug_redacts_bearer_token() {
        let headers = ProviderAuthHeaders {
            bearer_token: "super-secret-token".to_string(),
            chatgpt_account_id: Some("acct-1".to_string()),
            chatgpt_bearer_is_dirge_oauth: true,
        };
        let rendered = format!("{headers:?}");
        assert!(
            !rendered.contains("super-secret-token"),
            "bearer token must not appear in Debug output: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn codex_access_token_env_wins() {
        let headers = resolve_chatgpt_auth_from(
            Some(" env-token ".to_string()),
            Some("acct-env".to_string()),
            PathBuf::from("/should/not/be/read"),
        )
        .unwrap();

        assert_eq!(headers.bearer_token, "env-token");
        assert_eq!(headers.chatgpt_account_id.as_deref(), Some("acct-env"));
        // Env override is not Dirge-refreshable (dirge-8gdv.4).
        assert!(!headers.chatgpt_bearer_is_dirge_oauth);
    }

    #[test]
    fn dirge_openai_oauth_wins_over_codex_auth_file() {
        let dir = std::env::temp_dir().join(format!("dirge-chatgpt-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{ "tokens": { "access_token": "stale-codex-token", "account_id": "acct-codex" } }"#,
        )
        .unwrap();

        let headers = resolve_chatgpt_auth_from_with_dirge_oauth(None, None, path, || {
            Ok(Some(OpenAiOAuthCredential::new(
                "fresh-dirge-token",
                "refresh-token",
                None,
                Some("acct-dirge".to_string()),
                i64::MAX,
            )))
        })
        .unwrap();

        assert_eq!(headers.bearer_token, "fresh-dirge-token");
        assert_eq!(headers.chatgpt_account_id.as_deref(), Some("acct-dirge"));
        // Dirge OAuth token is refreshable (dirge-8gdv.4).
        assert!(headers.chatgpt_bearer_is_dirge_oauth);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unrefreshable_dirge_oauth_falls_back_to_codex_auth_file() {
        // dirge-cu44: an expired Dirge OAuth credential whose refresh fails
        // surfaces as an Err from the loader. That must NOT block a valid
        // legacy codex auth file — otherwise a stale-but-broken Dirge login
        // hard-errors a session that `codex login` could have served.
        let dir = std::env::temp_dir().join(format!(
            "dirge-chatgpt-auth-fallback-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{ "tokens": { "access_token": "codex-token", "account_id": "acct-codex" } }"#,
        )
        .unwrap();

        let headers = resolve_chatgpt_auth_from_with_dirge_oauth(None, None, path, || {
            anyhow::bail!("Stored OpenAI OAuth credential is expired and could not be refreshed")
        })
        .unwrap();

        assert_eq!(headers.bearer_token, "codex-token");
        assert_eq!(headers.chatgpt_account_id.as_deref(), Some("acct-codex"));
        // Legacy codex-file fallback is not Dirge-refreshable (dirge-8gdv.4).
        assert!(!headers.chatgpt_bearer_is_dirge_oauth);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// dirge-l7xy: a legacy auth.json whose token lacks an account id used to
    /// fail with "set CHATGPT_ACCOUNT_ID", but the file branch read only the
    /// file — so setting the env var didn't help. It's now a fallback.
    #[test]
    fn env_account_id_fills_in_for_accountless_codex_file() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-chatgpt-auth-envfallback-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        // No account_id anywhere in the file.
        std::fs::write(&path, r#"{ "tokens": { "access_token": "codex-token" } }"#).unwrap();

        let headers = resolve_chatgpt_auth_from_with_dirge_oauth(
            None,
            Some("acct-env".to_string()),
            path,
            || Ok(None),
        )
        .unwrap();

        assert_eq!(headers.bearer_token, "codex-token");
        assert_eq!(headers.chatgpt_account_id.as_deref(), Some("acct-env"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// dirge-l7xy: same fallback on the Dirge-OAuth branch — a credential with
    /// no account id picks up CHATGPT_ACCOUNT_ID rather than sending none.
    #[test]
    fn env_account_id_fills_in_for_accountless_dirge_oauth() {
        let headers = resolve_chatgpt_auth_from_with_dirge_oauth(
            None,
            Some("acct-env".to_string()),
            PathBuf::from("/should/not/be/read"),
            || {
                Ok(Some(OpenAiOAuthCredential::new(
                    "dirge-token",
                    "refresh-token",
                    None,
                    None, // credential carries no account id
                    i64::MAX,
                )))
            },
        )
        .unwrap();

        assert_eq!(headers.bearer_token, "dirge-token");
        assert_eq!(headers.chatgpt_account_id.as_deref(), Some("acct-env"));
        assert!(headers.chatgpt_bearer_is_dirge_oauth);
    }

    #[test]
    fn anthropic_access_token_env_wins() {
        let headers = resolve_anthropic_auth_from(
            Some(" oat-env-token ".to_string()),
            PathBuf::from("/should/not/be/read"),
        )
        .unwrap();

        assert_eq!(headers.bearer_token, "oat-env-token");
        assert_eq!(headers.chatgpt_account_id, None);
    }

    #[test]
    fn anthropic_refresh_sync_does_not_panic_on_current_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let result =
                std::panic::catch_unwind(|| refresh_anthropic_token_sync("invalid-refresh-token"));
            assert!(
                result.is_ok(),
                "refresh entrypoint must not panic on current_thread runtime"
            );
        });
    }

    #[test]
    fn anthropic_reads_credentials_file_access_token() {
        let dir = std::env::temp_dir().join(format!("dirge-anthropic-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(
            &path,
            r#"{ "claudeAiOauth": { "accessToken": "sk-ant-oat-file", "refreshToken": "no" } }"#,
        )
        .unwrap();

        let headers = resolve_anthropic_auth_from(None, path.clone()).unwrap();
        assert_eq!(headers.bearer_token, "sk-ant-oat-file");

        std::fs::remove_dir_all(&dir).ok();
    }
}
