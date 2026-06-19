use std::path::PathBuf;

use crate::config::ProviderAuth;

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthHeaders {
    pub bearer_token: String,
    pub chatgpt_account_id: Option<String>,
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
    if let Some(token) = codex_access_token
        && !token.trim().is_empty()
    {
        return Ok(ProviderAuthHeaders {
            bearer_token: token.trim().to_string(),
            chatgpt_account_id: chatgpt_account_id.filter(|v| !v.trim().is_empty()),
        });
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
    );

    Ok(ProviderAuthHeaders {
        bearer_token,
        chatgpt_account_id,
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

fn resolve_anthropic_auth() -> anyhow::Result<ProviderAuthHeaders> {
    resolve_anthropic_auth_from(
        std::env::var("ANTHROPIC_OAUTH_TOKEN").ok(),
        anthropic_credentials_file_path(),
    )
}

fn resolve_anthropic_auth_from(
    oauth_token: Option<String>,
    credentials_file_path: PathBuf,
) -> anyhow::Result<ProviderAuthHeaders> {
    if let Some(token) = oauth_token
        && !token.trim().is_empty()
    {
        return Ok(ProviderAuthHeaders {
            bearer_token: token.trim().to_string(),
            chatgpt_account_id: None,
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

    if anthropic_token_is_expired(&json)
        && let Some(refresh) = extract_string_by_keys(&json, &["refreshToken", "refresh_token"])
    {
        let refreshed = refresh_anthropic_token_sync(&refresh)?;
        crate::provider::anthropic_oauth::persist_credentials(&refreshed)?;
        bearer_token = refreshed.access_token;
    }

    Ok(ProviderAuthHeaders {
        bearer_token,
        chatgpt_account_id: None,
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
