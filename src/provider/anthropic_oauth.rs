use std::net::TcpListener;
use std::path::PathBuf;

use crate::auth::oauth_pkce;
use anyhow::Context;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53692;
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// Bound on each token-endpoint POST. Without it a dead TCP connection hangs
/// the refresh forever, wedging every caller waiting on the token mutex and
/// the cross-process auth-file lock (mirrors `openai_device::REQUEST_TIMEOUT`).
const TOKEN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AnthropicOAuthCredentials {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
}

impl std::fmt::Debug for AnthropicOAuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicOAuthCredentials")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

struct AuthorizeRequest {
    url: String,
    state: String,
    verifier: String,
}

fn authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", "true")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .finish()
    )
}

fn prepare_authorize_request() -> AuthorizeRequest {
    let verifier = oauth_pkce::verifier();
    let challenge = oauth_pkce::challenge(&verifier);
    // Independent CSRF state — must not be the verifier. Reusing the verifier
    // as `state` would embed the PKCE secret in the authorize URL (terminal,
    // browser history, URL-logging proxy) and echo it in the localhost
    // redirect, letting an observer of either complete the exchange.
    let state = oauth_pkce::state();
    let url = authorize_url(&challenge, &state);
    AuthorizeRequest {
        url,
        state,
        verifier,
    }
}

pub(crate) async fn login_and_persist() -> anyhow::Result<PathBuf> {
    let AuthorizeRequest {
        url: authorize_url,
        state,
        verifier,
    } = prepare_authorize_request();
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .with_context(|| format!("failed to bind OAuth callback port {CALLBACK_PORT}"))?;

    eprintln!("Open this URL to authenticate with Anthropic:\n\n{authorize_url}\n");
    eprintln!("Waiting for browser redirect on {REDIRECT_URI} ...");

    let (code, returned_state) = wait_for_callback(listener, &state)?;
    let credentials = exchange_authorization_code(&code, &returned_state, &verifier).await?;
    let path = persist_credentials(&credentials)?;
    Ok(path)
}

#[allow(dead_code)]
pub(crate) async fn refresh_token(
    refresh_token: &str,
) -> anyhow::Result<AnthropicOAuthCredentials> {
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = response.json().await?;
    Ok(credentials_from_token(token))
}

async fn exchange_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
) -> anyhow::Result<AnthropicOAuthCredentials> {
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        }))
        .send()
        .await?
        .error_for_status()?;
    let token: TokenResponse = response.json().await?;
    Ok(credentials_from_token(token))
}

fn credentials_from_token(token: TokenResponse) -> AnthropicOAuthCredentials {
    AnthropicOAuthCredentials {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: chrono::Utc::now().timestamp_millis() + token.expires_in * 1000 - 5 * 60 * 1000,
    }
}

pub(crate) fn persist_credentials(
    credentials: &AnthropicOAuthCredentials,
) -> anyhow::Result<PathBuf> {
    persist_credentials_to_path(credentials, credentials_file_path())
}

fn persist_credentials_to_path(
    credentials: &AnthropicOAuthCredentials,
    path: PathBuf,
) -> anyhow::Result<PathBuf> {
    // Read-modify-write: `~/.claude/.credentials.json` is shared with the
    // `claude` CLI, whose `claudeAiOauth` object carries fields dirge does not
    // own (e.g. `scopes`, `subscriptionType`) and which may sit alongside
    // sibling top-level keys. A full overwrite on every refresh dropped them
    // (dirge-wpk8). The caller already holds the cross-process file lock.
    let mut root: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let oauth = root
        .as_object_mut()
        .expect("root is defaulted to an object")
        .entry(String::from("claudeAiOauth"))
        .or_insert_with(|| serde_json::json!({}));
    if !oauth.is_object() {
        *oauth = serde_json::json!({});
    }
    let fresh = serde_json::to_value(credentials)?;
    if let (Some(fresh_map), Some(oauth_map)) = (fresh.as_object(), oauth.as_object_mut()) {
        for (key, value) in fresh_map {
            oauth_map.insert(key.clone(), value.clone());
        }
    }
    crate::auth::file_store::save_json_0600(&path, &root)?;
    Ok(path)
}

pub(crate) fn credentials_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join(".credentials.json")
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> anyhow::Result<(String, String)> {
    oauth_pkce::wait_for_callback(
        listener,
        &oauth_pkce::CallbackOptions {
            success_body: "Anthropic authentication completed. You can close this window.",
            failure_body: "Anthropic authentication failed. You can close this window.",
            error_context: "OAuth",
            expected_state: Some(expected_state),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_request_uses_independent_state_and_hides_verifier() {
        let req = prepare_authorize_request();
        assert_ne!(
            req.state, req.verifier,
            "state must not be the PKCE verifier"
        );
        assert!(
            !req.url.contains(&req.verifier),
            "raw PKCE verifier must never appear in the authorize URL: {}",
            req.url
        );
        assert!(
            req.url.contains(&oauth_pkce::challenge(&req.verifier)),
            "the S256 challenge should be present in the authorize URL"
        );
        assert!(
            req.url.contains(&format!("state={}", req.state)),
            "the independent state should be carried in the authorize URL"
        );
    }

    #[test]
    fn pkce_challenge_uses_s256_url_safe_no_pad() {
        assert_eq!(
            oauth_pkce::challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn credentials_debug_redacts_tokens() {
        let creds = AnthropicOAuthCredentials {
            access_token: "sk-ant-oat-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            expires_at: 123,
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("sk-ant-oat-secret"));
        assert!(!rendered.contains("refresh-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn token_response_debug_redacts_tokens() {
        let token = TokenResponse {
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            expires_in: 3600,
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("refresh-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[cfg(unix)]
    #[test]
    fn credentials_persist_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let creds = AnthropicOAuthCredentials {
            access_token: "sk-ant-oat-test".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 123,
        };
        let dir =
            std::env::temp_dir().join(format!("dirge-anthropic-oauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");

        persist_credentials_to_path(&creds, path.clone()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn credentials_persist_round_trips_through_private_atomic_write() {
        let creds = AnthropicOAuthCredentials {
            access_token: "sk-ant-oat-roundtrip".to_string(),
            refresh_token: "refresh-roundtrip".to_string(),
            expires_at: 456,
        };
        let dir = std::env::temp_dir().join(format!(
            "dirge-anthropic-oauth-roundtrip-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let path = dir.join(".claude").join(".credentials.json");

        let returned = persist_credentials_to_path(&creds, path.clone()).unwrap();
        assert_eq!(returned, path);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value["claudeAiOauth"]["accessToken"],
            "sk-ant-oat-roundtrip"
        );
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "refresh-roundtrip");
        assert_eq!(value["claudeAiOauth"]["expiresAt"], 456);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_preserves_foreign_credential_fields() {
        let creds = AnthropicOAuthCredentials {
            access_token: "new".to_string(),
            refresh_token: "new2".to_string(),
            expires_at: 999,
        };
        let dir = std::env::temp_dir().join(format!(
            "dirge-anthropic-oauth-merge-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");

        // Pre-existing Claude Code credential carries dirge's three fields PLUS
        // fields dirge does not own (scopes, subscriptionType) and a sibling
        // top-level key someOtherTool. A dirge token refresh must update only
        // accessToken/refreshToken/expiresAt and leave the rest intact
        // (dirge-wpk8).
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old","expiresAt":1,"scopes":["a","b"],"subscriptionType":"pro"},"someOtherTool":{"k":"v"}}"#,
        )
        .unwrap();

        let returned = persist_credentials_to_path(&creds, path.clone()).unwrap();
        assert_eq!(returned, path);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // dirge's three fields are updated.
        assert_eq!(value["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "new2");
        assert_eq!(value["claudeAiOauth"]["expiresAt"], 999);

        // Foreign fields inside claudeAiOauth survive.
        assert_eq!(
            value["claudeAiOauth"]["scopes"],
            serde_json::json!(["a", "b"])
        );
        assert_eq!(value["claudeAiOauth"]["subscriptionType"], "pro");

        // Sibling top-level key survives.
        assert_eq!(value["someOtherTool"]["k"], "v");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn credentials_persist_in_claude_compatible_shape() {
        let creds = AnthropicOAuthCredentials {
            access_token: "sk-ant-oat-test".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: 123,
        };
        let value = serde_json::json!({ "claudeAiOauth": creds });
        assert_eq!(value["claudeAiOauth"]["accessToken"], "sk-ant-oat-test");
    }
}
