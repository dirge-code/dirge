use super::openai_device::{
    DeviceAuthError, DeviceAuthHttp, HttpResponse, ReqwestDeviceAuthHttp, Result,
};
use serde::Deserialize;
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuthTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) expires_in: Option<u64>,
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("id_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiOAuthFlow<H> {
    issuer: String,
    client_id: String,
    http: H,
}

impl<H> OpenAiOAuthFlow<H> {
    pub(crate) fn new(issuer: impl Into<String>, client_id: impl Into<String>, http: H) -> Self {
        Self {
            issuer: issuer.into().trim_end_matches('/').to_string(),
            client_id: client_id.into(),
            http,
        }
    }
}

impl<H> OpenAiOAuthFlow<H>
where
    H: DeviceAuthHttp,
{
    pub(crate) async fn exchange_authorization_code(
        &self,
        code: String,
        redirect_uri: String,
        code_verifier: String,
    ) -> Result<OAuthTokens> {
        let response = self
            .http
            .post_form(
                format!("{}/oauth/token", self.issuer),
                vec![
                    ("grant_type".to_string(), "authorization_code".to_string()),
                    ("code".to_string(), code),
                    ("redirect_uri".to_string(), redirect_uri),
                    ("client_id".to_string(), self.client_id.clone()),
                    ("code_verifier".to_string(), code_verifier),
                ],
            )
            .await?;
        authorization_code_tokens(response)
    }

    pub(crate) async fn refresh_access_token(&self, refresh_token: &str) -> Result<OAuthTokens> {
        let response = self
            .http
            .post_form(
                format!("{}/oauth/token", self.issuer),
                vec![
                    ("grant_type".to_string(), "refresh_token".to_string()),
                    ("refresh_token".to_string(), refresh_token.to_string()),
                    ("client_id".to_string(), self.client_id.clone()),
                ],
            )
            .await?;
        refresh_tokens(response, refresh_token)
    }
}

pub(crate) async fn exchange_browser_authorization_code(
    issuer: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<OAuthTokens> {
    Ok(
        OpenAiOAuthFlow::new(issuer, client_id, ReqwestDeviceAuthHttp::default())
            .exchange_authorization_code(
                code.to_string(),
                redirect_uri.to_string(),
                verifier.to_string(),
            )
            .await?,
    )
}

fn authorization_code_tokens(response: HttpResponse) -> Result<OAuthTokens> {
    match response.status {
        200..=299 => {
            let body: TokenResponse = parse_response(&response.body)?;
            Ok(body.into_tokens())
        }
        status => Err(DeviceAuthError::TokenExchangeStatus { status }),
    }
}

fn refresh_tokens(response: HttpResponse, prior_refresh_token: &str) -> Result<OAuthTokens> {
    match response.status {
        200..=299 => {
            let body: RefreshTokenResponse = parse_response(&response.body)?;
            Ok(body.into_tokens(prior_refresh_token))
        }
        status => Err(DeviceAuthError::TokenExchangeStatus { status }),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(
        default,
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId",
        alias = "chatgpt_account",
        alias = "accountId"
    )]
    account_id: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(
        default,
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId",
        alias = "chatgpt_account",
        alias = "accountId"
    )]
    account_id: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_tokens(self) -> OAuthTokens {
        let account_id = normalize_optional_string(self.account_id)
            .or_else(|| account_id_from_access_token(&self.access_token));
        OAuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            id_token: self.id_token.unwrap_or_default(),
            account_id,
            expires_in: self.expires_in,
        }
    }
}

impl RefreshTokenResponse {
    fn into_tokens(self, prior_refresh_token: &str) -> OAuthTokens {
        let account_id = normalize_optional_string(self.account_id)
            .or_else(|| account_id_from_access_token(&self.access_token));
        OAuthTokens {
            access_token: self.access_token,
            refresh_token: normalize_optional_string(self.refresh_token)
                .unwrap_or_else(|| prior_refresh_token.to_string()),
            id_token: self.id_token.unwrap_or_default(),
            account_id,
            expires_in: self.expires_in,
        }
    }
}

pub(crate) fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn account_id_from_access_token(access_token: &str) -> Option<String> {
    use base64::Engine;

    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let account_id = claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()?;
    normalize_optional_string(Some(account_id.to_string()))
}

fn parse_response<T>(body: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(body).map_err(|err| {
        let reason = match err.classify() {
            serde_json::error::Category::Io => "I/O error while parsing JSON",
            serde_json::error::Category::Syntax => "invalid JSON syntax",
            serde_json::error::Category::Data => "unexpected JSON shape",
            serde_json::error::Category::Eof => "truncated JSON response",
        };
        DeviceAuthError::InvalidResponse(reason.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedRequest {
        url: String,
        form: Vec<(String, String)>,
    }

    #[derive(Clone)]
    struct FakeHttp {
        responses: Arc<Mutex<VecDeque<Result<HttpResponse>>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl FakeHttp {
        fn new(responses: impl IntoIterator<Item = Result<HttpResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl DeviceAuthHttp for FakeHttp {
        fn post_json(
            &self,
            _url: String,
            _body: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + '_>> {
            unreachable!("OpenAI OAuth token flow only posts forms")
        }

        fn post_form(
            &self,
            url: String,
            form: Vec<(String, String)>,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + '_>> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .unwrap()
                    .push(RecordedRequest { url, form });
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("fake response queued")
            })
        }
    }

    fn response(status: u16, body: serde_json::Value) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
        })
    }

    fn flow(http: FakeHttp) -> OpenAiOAuthFlow<FakeHttp> {
        OpenAiOAuthFlow::new("https://auth.openai.com", "client-test", http)
    }

    fn access_token_with_account(account_id: &str) -> String {
        use base64::Engine;
        let encode = |value: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.signature",
            encode(&json!({"alg": "RS256", "typ": "JWT"})),
            encode(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id
                }
            }))
        )
    }

    #[tokio::test]
    async fn authorization_code_exchange_posts_form_and_accepts_missing_id_token() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "ACCESS-TOKEN",
                "refresh_token": "REFRESH-TOKEN",
                "chatgptAccountId": "acct-alias",
                "expires_in": 3600
            }),
        )]);

        let tokens = flow(http.clone())
            .exchange_authorization_code(
                "AUTH-CODE".to_string(),
                "http://localhost/callback".to_string(),
                "VERIFIER".to_string(),
            )
            .await
            .unwrap();

        assert_eq!(tokens.access_token, "ACCESS-TOKEN");
        assert_eq!(tokens.refresh_token, "REFRESH-TOKEN");
        assert_eq!(tokens.id_token, "");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-alias"));
        assert_eq!(tokens.expires_in, Some(3600));

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
        assert!(
            requests[0]
                .form
                .contains(&("grant_type".to_string(), "authorization_code".to_string()))
        );
        assert!(
            requests[0]
                .form
                .contains(&("code".to_string(), "AUTH-CODE".to_string()))
        );
        assert!(requests[0].form.contains(&(
            "redirect_uri".to_string(),
            "http://localhost/callback".to_string()
        )));
        assert!(
            requests[0]
                .form
                .contains(&("client_id".to_string(), "client-test".to_string()))
        );
        assert!(
            requests[0]
                .form
                .contains(&("code_verifier".to_string(), "VERIFIER".to_string()))
        );
    }

    #[tokio::test]
    async fn authorization_code_exchange_recovers_account_id_from_access_token_jwt() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": access_token_with_account("acct-from-jwt"),
                "refresh_token": "REFRESH-TOKEN",
                "expires_in": 3600
            }),
        )]);

        let tokens = flow(http)
            .exchange_authorization_code(
                "AUTH-CODE".to_string(),
                "http://localhost/callback".to_string(),
                "VERIFIER".to_string(),
            )
            .await
            .unwrap();

        assert_eq!(tokens.account_id.as_deref(), Some("acct-from-jwt"));
    }
}
