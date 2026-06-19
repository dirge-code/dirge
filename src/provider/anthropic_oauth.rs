use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::Context;
use base64::Engine;
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53692;
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

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

pub(crate) async fn login_and_persist() -> anyhow::Result<PathBuf> {
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .with_context(|| format!("failed to bind OAuth callback port {CALLBACK_PORT}"))?;

    let authorize_url = format!(
        "{AUTHORIZE_URL}?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", "true")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &verifier)
            .finish()
    );

    eprintln!("Open this URL to authenticate with Anthropic:\n\n{authorize_url}\n");
    eprintln!("Waiting for browser redirect on {REDIRECT_URI} ...");

    let (code, state) = wait_for_callback(listener)?;
    if state != verifier {
        anyhow::bail!("OAuth state mismatch");
    }
    let credentials = exchange_authorization_code(&code, &state, &verifier).await?;
    let path = persist_credentials(&credentials)?;
    Ok(path)
}

#[allow(dead_code)]
pub(crate) async fn refresh_token(
    refresh_token: &str,
) -> anyhow::Result<AnthropicOAuthCredentials> {
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
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
    let json = serde_json::json!({ "claudeAiOauth": credentials });
    crate::auth::file_store::save_json_0600(&path, &json)?;
    Ok(path)
}

pub(crate) fn credentials_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join(".credentials.json")
}

fn pkce_verifier() -> String {
    format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn wait_for_callback(listener: TcpListener) -> anyhow::Result<(String, String)> {
    let (mut stream, _) = listener.accept()?;
    let mut buf = [0_u8; 8192];
    let len = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..len]);
    let line = request
        .lines()
        .next()
        .context("empty OAuth callback request")?;
    let target = line
        .split_whitespace()
        .nth(1)
        .context("malformed OAuth callback request")?;
    let url = url::Url::parse(&format!("http://localhost{target}"))?;
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback missing code")?;
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback missing state")?;
    let body = "Anthropic authentication completed. You can close this window.";
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok((code, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_uses_s256_url_safe_no_pad() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
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
