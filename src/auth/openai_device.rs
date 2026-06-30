pub(crate) use super::openai_oauth::OAuthTokens;
use super::openai_oauth::OpenAiOAuthFlow;
use serde::Deserialize;
use serde::Deserializer;
use serde::de;
use serde_json::json;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_ISSUER: &str = "https://auth.openai.com";
pub(crate) const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) type Result<T> = std::result::Result<T, DeviceAuthError>;
type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>>;
type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DeviceAuthError {
    #[error(
        "OpenAI device-code auth is not enabled. Please enable device-code auth in ChatGPT Codex security settings, then run `dirge auth openai --device-code` again."
    )]
    DeviceAuthDisabled,
    #[error("OpenAI device-code auth timed out after 15 minutes")]
    TimedOut,
    #[error("OpenAI device-code request failed with status {status}")]
    UserCodeStatus { status: u16 },
    #[error("OpenAI device-code polling failed with status {status}")]
    PollStatus { status: u16 },
    #[error("OpenAI OAuth token exchange failed with status {status}")]
    TokenExchangeStatus { status: u16 },
    #[error("OpenAI device-code response was invalid: {0}")]
    InvalidResponse(String),
    #[error("OpenAI device-code transport failed: {0}")]
    Transport(String),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DeviceCode {
    pub(crate) verification_url: String,
    pub(crate) user_code: String,
    pub(crate) device_auth_id: String,
    pub(crate) interval: Duration,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("verification_url", &self.verification_url)
            .field("user_code", &"[REDACTED]")
            .field("device_auth_id", &"[REDACTED]")
            .field("interval", &self.interval)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthorizationCode {
    pub(crate) authorization_code: String,
    pub(crate) code_verifier: String,
}

impl fmt::Debug for AuthorizationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationCode")
            .field("authorization_code", &"[REDACTED]")
            .field("code_verifier", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body", &"[REDACTED]")
            .finish()
    }
}

pub(crate) trait DeviceAuthHttp: Clone + Send + Sync + 'static {
    fn post_json(&self, url: String, body: serde_json::Value) -> HttpFuture<'_>;

    fn post_form(&self, url: String, form: Vec<(String, String)>) -> HttpFuture<'_>;
}

pub(crate) trait DeviceAuthRuntime: Clone + Send + Sync + 'static {
    fn now(&self) -> Instant;

    fn sleep(&self, duration: Duration) -> SleepFuture<'_>;
}

fn device_auth_request_timeout() -> Duration {
    REQUEST_TIMEOUT
}

#[derive(Clone)]
pub(crate) struct ReqwestDeviceAuthHttp {
    client: reqwest::Client,
}

impl Default for ReqwestDeviceAuthHttp {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl ReqwestDeviceAuthHttp {
    async fn response(response: reqwest::Response) -> Result<HttpResponse> {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| DeviceAuthError::Transport(err.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

impl DeviceAuthHttp for ReqwestDeviceAuthHttp {
    fn post_json(&self, url: String, body: serde_json::Value) -> HttpFuture<'_> {
        Box::pin(async move {
            let response = self
                .client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(device_auth_request_timeout())
                .json(&body)
                .send()
                .await
                .map_err(|err| DeviceAuthError::Transport(err.to_string()))?;
            Self::response(response).await
        })
    }

    fn post_form(&self, url: String, form: Vec<(String, String)>) -> HttpFuture<'_> {
        Box::pin(async move {
            let body = encode_form(&form);
            let response = self
                .client
                .post(url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .timeout(device_auth_request_timeout())
                .body(body)
                .send()
                .await
                .map_err(|err| DeviceAuthError::Transport(err.to_string()))?;
            Self::response(response).await
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TokioDeviceAuthRuntime;

impl DeviceAuthRuntime for TokioDeviceAuthRuntime {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> SleepFuture<'_> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiDeviceAuthFlow<H = ReqwestDeviceAuthHttp, R = TokioDeviceAuthRuntime> {
    issuer: String,
    client_id: String,
    http: H,
    runtime: R,
    timeout: Duration,
}

impl Default for OpenAiDeviceAuthFlow<ReqwestDeviceAuthHttp, TokioDeviceAuthRuntime> {
    fn default() -> Self {
        Self::with_parts(
            DEFAULT_ISSUER,
            DEFAULT_CLIENT_ID,
            ReqwestDeviceAuthHttp::default(),
            TokioDeviceAuthRuntime,
        )
    }
}

impl<H, R> OpenAiDeviceAuthFlow<H, R> {
    pub(crate) fn with_parts(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        http: H,
        runtime: R,
    ) -> Self {
        let issuer = issuer.into().trim_end_matches('/').to_string();
        Self {
            issuer,
            client_id: client_id.into(),
            http,
            runtime,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl<H, R> OpenAiDeviceAuthFlow<H, R>
where
    H: DeviceAuthHttp,
    R: DeviceAuthRuntime,
{
    pub(crate) async fn request_device_code(&self) -> Result<DeviceCode> {
        let response = self
            .http
            .post_json(
                format!("{}/api/accounts/deviceauth/usercode", self.issuer),
                json!({ "client_id": self.client_id }),
            )
            .await?;

        match response.status {
            200..=299 => {
                let body: UserCodeResponse = parse_response(&response.body)?;
                Ok(DeviceCode {
                    verification_url: format!("{}/codex/device", self.issuer),
                    user_code: body.user_code,
                    device_auth_id: body.device_auth_id,
                    interval: Duration::from_secs(body.interval_seconds),
                })
            }
            404 => Err(DeviceAuthError::DeviceAuthDisabled),
            status => Err(DeviceAuthError::UserCodeStatus { status }),
        }
    }

    pub(crate) async fn complete_device_code_login(
        &self,
        device_code: DeviceCode,
    ) -> Result<OAuthTokens> {
        let authorization_code = self.poll_for_authorization_code(&device_code).await?;
        self.exchange_authorization_code(authorization_code).await
    }

    async fn poll_for_authorization_code(
        &self,
        device_code: &DeviceCode,
    ) -> Result<AuthorizationCode> {
        let start = self.runtime.now();
        loop {
            let response = self
                .http
                .post_json(
                    format!("{}/api/accounts/deviceauth/token", self.issuer),
                    json!({
                        "device_auth_id": device_code.device_auth_id,
                        "user_code": device_code.user_code,
                    }),
                )
                .await?;

            match response.status {
                200..=299 => {
                    let body: AuthorizationCodeResponse = parse_response(&response.body)?;
                    return Ok(AuthorizationCode {
                        authorization_code: body.authorization_code,
                        code_verifier: body.code_verifier,
                    });
                }
                403 | 404 => {
                    let elapsed = self.runtime.now().duration_since(start);
                    if elapsed >= self.timeout {
                        return Err(DeviceAuthError::TimedOut);
                    }
                    let remaining = self.timeout.saturating_sub(elapsed);
                    self.runtime
                        .sleep(device_code.interval.min(remaining))
                        .await;
                }
                status => return Err(DeviceAuthError::PollStatus { status }),
            }
        }
    }

    pub(crate) async fn exchange_authorization_code(
        &self,
        code: AuthorizationCode,
    ) -> Result<OAuthTokens> {
        OpenAiOAuthFlow::new(
            self.issuer.clone(),
            self.client_id.clone(),
            self.http.clone(),
        )
        .exchange_authorization_code(
            code.authorization_code,
            format!("{}/deviceauth/callback", self.issuer),
            code.code_verifier,
        )
        .await
    }

    /// Exchange a refresh token for a fresh access token at `{issuer}/oauth/token`.
    ///
    /// OpenAI's refresh response may omit `refresh_token`/`id_token`; in that
    /// case the prior refresh token is retained so future refreshes still work.
    pub(crate) async fn refresh_access_token(&self, refresh_token: &str) -> Result<OAuthTokens> {
        OpenAiOAuthFlow::new(
            self.issuer.clone(),
            self.client_id.clone(),
            self.http.clone(),
        )
        .refresh_access_token(refresh_token)
        .await
    }
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(
        default = "default_poll_interval_seconds",
        deserialize_with = "deserialize_interval_seconds",
        rename = "interval"
    )]
    interval_seconds: u64,
}

#[derive(Deserialize)]
struct AuthorizationCodeResponse {
    authorization_code: String,
    #[serde(rename = "code_challenge")]
    _code_challenge: String,
    code_verifier: String,
}

fn default_poll_interval_seconds() -> u64 {
    DEFAULT_POLL_INTERVAL_SECONDS
}

fn deserialize_interval_seconds<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let interval = match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| de::Error::custom("interval must be an unsigned integer")),
        serde_json::Value::String(value) => value
            .trim()
            .parse::<u64>()
            .map_err(|err| de::Error::custom(format!("invalid interval: {err}"))),
        serde_json::Value::Null => Ok(DEFAULT_POLL_INTERVAL_SECONDS),
        _ => Err(de::Error::custom("interval must be a string or number")),
    }?;
    Ok(if interval == 0 {
        DEFAULT_POLL_INTERVAL_SECONDS
    } else {
        interval
    })
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

fn encode_form(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", form_escape(key), form_escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_escape(value: &str) -> String {
    let mut escaped = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                escaped.push(byte as char);
            }
            b' ' => escaped.push('+'),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RecordedBody {
        Json(serde_json::Value),
        Form(Vec<(String, String)>),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedRequest {
        url: String,
        body: RecordedBody,
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
            url: String,
            body: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + '_>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(RecordedRequest {
                    url,
                    body: RecordedBody::Json(body),
                });
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("fake response queued")
            })
        }

        fn post_form(
            &self,
            url: String,
            form: Vec<(String, String)>,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + '_>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(RecordedRequest {
                    url,
                    body: RecordedBody::Form(form),
                });
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("fake response queued")
            })
        }
    }

    #[derive(Clone)]
    struct FakeRuntime {
        start: Instant,
        elapsed: Arc<Mutex<Duration>>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Arc::new(Mutex::new(Duration::ZERO)),
                sleeps: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl DeviceAuthRuntime for FakeRuntime {
        fn now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.sleeps.lock().unwrap().push(duration);
                *self.elapsed.lock().unwrap() += duration;
            })
        }
    }

    fn response(status: u16, body: serde_json::Value) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
        })
    }

    fn flow(http: FakeHttp, runtime: FakeRuntime) -> OpenAiDeviceAuthFlow<FakeHttp, FakeRuntime> {
        OpenAiDeviceAuthFlow::with_parts("https://auth.openai.com", "client-test", http, runtime)
    }

    #[test]
    fn device_auth_requests_have_a_per_request_timeout_budget() {
        let timeout = device_auth_request_timeout();

        assert!(timeout > Duration::ZERO);
        assert!(timeout < DEFAULT_TIMEOUT);
        assert_eq!(timeout, Duration::from_secs(30));
    }

    #[test]
    fn protocol_debug_impls_redact_secret_values() {
        let device_code = DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: "USER-CODE".to_string(),
            device_auth_id: "DEVICE-AUTH-ID".to_string(),
            interval: Duration::from_secs(5),
        };
        let authorization_code = AuthorizationCode {
            authorization_code: "AUTH-CODE".to_string(),
            code_verifier: "CODE-VERIFIER".to_string(),
        };
        let tokens = OAuthTokens {
            access_token: "ACCESS-TOKEN".to_string(),
            refresh_token: "REFRESH-TOKEN".to_string(),
            id_token: "ID-TOKEN".to_string(),
            account_id: None,
            expires_in: Some(3600),
        };
        let response = HttpResponse {
            status: 200,
            body: "ACCESS-TOKEN REFRESH-TOKEN ID-TOKEN AUTH-CODE USER-CODE DEVICE-AUTH-ID CODE-VERIFIER".to_string(),
        };

        for debug in [
            format!("{device_code:?}"),
            format!("{authorization_code:?}"),
            format!("{tokens:?}"),
            format!("{response:?}"),
        ] {
            assert!(debug.contains("[REDACTED]"));
            for secret in [
                "USER-CODE",
                "DEVICE-AUTH-ID",
                "AUTH-CODE",
                "CODE-VERIFIER",
                "ACCESS-TOKEN",
                "REFRESH-TOKEN",
                "ID-TOKEN",
            ] {
                assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
            }
        }
    }

    #[tokio::test]
    async fn requests_user_code_with_default_openai_shape() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "device_auth_id": "device-auth-id",
                "user_code": "USER-CODE",
                "interval": "7"
            }),
        )]);
        let runtime = FakeRuntime::new();

        let device_code = flow(http.clone(), runtime)
            .request_device_code()
            .await
            .unwrap();

        assert_eq!(
            device_code.verification_url,
            "https://auth.openai.com/codex/device"
        );
        assert_eq!(device_code.user_code, "USER-CODE");
        assert_eq!(device_code.interval, Duration::from_secs(7));
        assert_eq!(
            http.requests(),
            vec![RecordedRequest {
                url: "https://auth.openai.com/api/accounts/deviceauth/usercode".to_string(),
                body: RecordedBody::Json(json!({ "client_id": "client-test" })),
            }]
        );
    }

    #[tokio::test]
    async fn accepts_usercode_alias_and_numeric_interval() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "device_auth_id": "device-auth-id",
                "usercode": "ALIAS-CODE",
                "interval": 3
            }),
        )]);
        let runtime = FakeRuntime::new();

        let device_code = flow(http, runtime).request_device_code().await.unwrap();

        assert_eq!(device_code.user_code, "ALIAS-CODE");
        assert_eq!(device_code.interval, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn zero_poll_interval_is_clamped_to_default_interval() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "device_auth_id": "device-auth-id",
                "user_code": "USER-CODE",
                "interval": 0
            }),
        )]);
        let runtime = FakeRuntime::new();

        let device_code = flow(http, runtime).request_device_code().await.unwrap();

        assert_eq!(
            device_code.interval,
            Duration::from_secs(DEFAULT_POLL_INTERVAL_SECONDS)
        );
    }

    #[tokio::test]
    async fn disabled_user_code_endpoint_is_actionable() {
        let http = FakeHttp::new([response(404, json!({"error": "not found"}))]);
        let runtime = FakeRuntime::new();

        let err = flow(http, runtime).request_device_code().await.unwrap_err();

        assert!(matches!(err, DeviceAuthError::DeviceAuthDisabled));
        assert!(err.to_string().contains("enable device-code auth"));
        assert!(err.to_string().contains("ChatGPT Codex security settings"));
    }

    #[tokio::test]
    async fn pending_poll_sleeps_then_exchanges_authorization_code_for_tokens() {
        let http = FakeHttp::new([
            response(403, json!({"status": "pending"})),
            response(
                200,
                json!({
                    "authorization_code": "AUTH-CODE",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }),
            ),
            response(
                200,
                json!({
                    "access_token": "ACCESS-TOKEN",
                    "refresh_token": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "chatgpt_account_id": "acct-device",
                    "expires_in": 3600
                }),
            ),
        ]);
        let runtime = FakeRuntime::new();
        let device_code = DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: "USER-CODE".to_string(),
            device_auth_id: "device-auth-id".to_string(),
            interval: Duration::from_secs(4),
        };

        let tokens = flow(http.clone(), runtime.clone())
            .complete_device_code_login(device_code)
            .await
            .unwrap();

        assert_eq!(tokens.access_token, "ACCESS-TOKEN");
        assert_eq!(tokens.refresh_token, "REFRESH-TOKEN");
        assert_eq!(tokens.id_token, "ID-TOKEN");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-device"));
        assert_eq!(tokens.expires_in, Some(3600));
        assert_eq!(runtime.sleeps(), vec![Duration::from_secs(4)]);
        assert_eq!(
            http.requests(),
            vec![
                RecordedRequest {
                    url: "https://auth.openai.com/api/accounts/deviceauth/token".to_string(),
                    body: RecordedBody::Json(json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "USER-CODE"
                    })),
                },
                RecordedRequest {
                    url: "https://auth.openai.com/api/accounts/deviceauth/token".to_string(),
                    body: RecordedBody::Json(json!({
                        "device_auth_id": "device-auth-id",
                        "user_code": "USER-CODE"
                    })),
                },
                RecordedRequest {
                    url: "https://auth.openai.com/oauth/token".to_string(),
                    body: RecordedBody::Form(vec![
                        ("grant_type".to_string(), "authorization_code".to_string()),
                        ("code".to_string(), "AUTH-CODE".to_string()),
                        (
                            "redirect_uri".to_string(),
                            "https://auth.openai.com/deviceauth/callback".to_string(),
                        ),
                        ("client_id".to_string(), "client-test".to_string()),
                        ("code_verifier".to_string(), "verifier".to_string()),
                    ]),
                },
            ]
        );
    }

    #[tokio::test]
    async fn token_exchange_recovers_account_id_from_access_token_jwt() {
        use base64::Engine;

        // Access token JWT (header.payload.signature) whose payload carries the
        // account id only inside the OpenAI auth claim, like the real response.
        let payload = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-from-jwt"
            }
        });
        let encode = |value: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        let access_token = format!(
            "{}.{}.{}",
            encode(&json!({"alg": "RS256", "typ": "JWT"})),
            encode(&payload),
            "signature"
        );

        let http = FakeHttp::new([
            response(
                200,
                json!({
                    "authorization_code": "AUTH-CODE",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }),
            ),
            response(
                200,
                json!({
                    "access_token": access_token,
                    "refresh_token": "REFRESH-TOKEN",
                    "id_token": "ID-TOKEN",
                    "expires_in": 3600
                }),
            ),
        ]);
        let device_code = DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: "USER-CODE".to_string(),
            device_auth_id: "device-auth-id".to_string(),
            interval: Duration::from_secs(5),
        };

        let tokens = flow(http, FakeRuntime::new())
            .complete_device_code_login(device_code)
            .await
            .unwrap();

        assert_eq!(tokens.account_id.as_deref(), Some("acct-from-jwt"));
    }

    #[tokio::test]
    async fn pending_poll_times_out_without_real_sleeping() {
        let http = FakeHttp::new(
            std::iter::repeat_with(|| response(404, json!({"pending": true}))).take(4),
        );
        let runtime = FakeRuntime::new();
        let device_code = DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: "USER-CODE".to_string(),
            device_auth_id: "device-auth-id".to_string(),
            interval: Duration::from_secs(300),
        };

        let err = flow(http, runtime.clone())
            .complete_device_code_login(device_code)
            .await
            .unwrap_err();

        assert!(matches!(err, DeviceAuthError::TimedOut));
        assert_eq!(
            runtime.sleeps(),
            vec![
                Duration::from_secs(300),
                Duration::from_secs(300),
                Duration::from_secs(300),
            ]
        );
    }

    #[tokio::test]
    async fn malformed_json_is_reported_without_echoing_body() {
        let http = FakeHttp::new([Ok(HttpResponse {
            status: 200,
            body: "not-json ACCESS-TOKEN REFRESH-TOKEN ID-TOKEN USER-CODE".to_string(),
        })]);
        let runtime = FakeRuntime::new();

        let err = flow(http, runtime).request_device_code().await.unwrap_err();
        let message = err.to_string();

        assert!(matches!(err, DeviceAuthError::InvalidResponse(_)));
        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
        assert!(!message.contains("ID-TOKEN"));
        assert!(!message.contains("USER-CODE"));
    }

    #[tokio::test]
    async fn typed_deserialization_errors_do_not_echo_secret_values() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "ACCESS-TOKEN",
                "refresh_token": "REFRESH-TOKEN",
                "id_token": "ID-TOKEN",
                "expires_in": "ACCESS-TOKEN REFRESH-TOKEN ID-TOKEN AUTH-CODE USER-CODE DEVICE-AUTH-ID"
            }),
        )]);
        let runtime = FakeRuntime::new();
        let code = AuthorizationCode {
            authorization_code: "AUTH-CODE".to_string(),
            code_verifier: "verifier".to_string(),
        };

        let err = flow(http, runtime)
            .exchange_authorization_code(code)
            .await
            .unwrap_err();
        let message = err.to_string();
        let debug = format!("{err:?}");

        assert!(matches!(err, DeviceAuthError::InvalidResponse(_)));
        for secret in [
            "ACCESS-TOKEN",
            "REFRESH-TOKEN",
            "ID-TOKEN",
            "AUTH-CODE",
            "USER-CODE",
            "DEVICE-AUTH-ID",
        ] {
            assert!(!message.contains(secret), "Display leaked {secret}");
            assert!(!debug.contains(secret), "Debug leaked {secret}");
        }
    }

    #[tokio::test]
    async fn token_exchange_error_does_not_echo_secret_body() {
        let http = FakeHttp::new([response(
            400,
            json!({
                "error": "ACCESS-TOKEN REFRESH-TOKEN ID-TOKEN AUTH-CODE USER-CODE"
            }),
        )]);
        let runtime = FakeRuntime::new();
        let code = AuthorizationCode {
            authorization_code: "AUTH-CODE".to_string(),
            code_verifier: "verifier".to_string(),
        };

        let err = flow(http, runtime)
            .exchange_authorization_code(code)
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(matches!(
            err,
            DeviceAuthError::TokenExchangeStatus { status: 400 }
        ));
        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
        assert!(!message.contains("ID-TOKEN"));
        assert!(!message.contains("AUTH-CODE"));
        assert!(!message.contains("USER-CODE"));
    }

    #[tokio::test]
    async fn refresh_access_token_posts_refresh_grant() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "NEW-ACCESS",
                "refresh_token": "NEW-REFRESH",
                "id_token": "NEW-ID",
                "expires_in": 3600
            }),
        )]);
        let runtime = FakeRuntime::new();
        let flow = flow(http.clone(), runtime);

        let tokens = flow
            .refresh_access_token("OLD-REFRESH")
            .await
            .expect("refresh succeeds");

        assert_eq!(tokens.access_token, "NEW-ACCESS");
        assert_eq!(tokens.refresh_token, "NEW-REFRESH");
        assert_eq!(tokens.expires_in, Some(3600));

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
        let RecordedBody::Form(form) = &requests[0].body else {
            panic!("expected form body");
        };
        assert!(form.contains(&("grant_type".to_string(), "refresh_token".to_string())));
        assert!(form.contains(&("refresh_token".to_string(), "OLD-REFRESH".to_string())));
        assert!(form.contains(&("client_id".to_string(), "client-test".to_string())));
    }

    #[tokio::test]
    async fn refresh_access_token_keeps_prior_refresh_token_when_omitted() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "NEW-ACCESS",
                "id_token": "NEW-ID",
                "expires_in": 3600
            }),
        )]);
        let runtime = FakeRuntime::new();

        let tokens = flow(http, runtime)
            .refresh_access_token("OLD-REFRESH")
            .await
            .expect("refresh succeeds");

        assert_eq!(tokens.access_token, "NEW-ACCESS");
        assert_eq!(tokens.refresh_token, "OLD-REFRESH");
    }

    #[tokio::test]
    async fn refresh_access_token_error_does_not_echo_secret_body() {
        let http = FakeHttp::new([response(400, json!({ "error": "OLD-REFRESH NEW-ACCESS" }))]);
        let runtime = FakeRuntime::new();

        let err = flow(http, runtime)
            .refresh_access_token("OLD-REFRESH")
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(matches!(
            err,
            DeviceAuthError::TokenExchangeStatus { status: 400 }
        ));
        assert!(!message.contains("OLD-REFRESH"));
        assert!(!message.contains("NEW-ACCESS"));
    }

    #[tokio::test]
    async fn transport_error_is_classified_without_protocol_secrets() {
        let http = FakeHttp::new([Err(DeviceAuthError::Transport(
            "network unavailable".to_string(),
        ))]);
        let runtime = FakeRuntime::new();

        let err = flow(http, runtime).request_device_code().await.unwrap_err();

        assert!(matches!(err, DeviceAuthError::Transport(_)));
        assert_eq!(
            err.to_string(),
            "OpenAI device-code transport failed: network unavailable"
        );
    }
}
