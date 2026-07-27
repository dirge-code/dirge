//! Kimi Code (Moonshot) OAuth — RFC 8628 device-code flow against
//! `https://auth.kimi.com`, plus the `X-Msh-*` device identity headers the
//! managed service expects on both OAuth and API requests.
//!
//! Mirrors the injectable structure of `openai_device.rs` but speaks the
//! Kimi protocol shape (every endpoint is a form-encoded POST, there is no
//! browser/localhost-redirect variant) from the reference TypeScript client
//! (`packages/oauth/src` in MoonshotAI/kimi-code). Two protocol facts drive
//! the design:
//!   - access tokens live only 15 minutes, so refresh is mandatory and gets
//!     a small retry budget for transient 429/5xx/transport failures;
//!   - refresh rotates the refresh token, so the rotated bundle must always
//!     be persisted (handled by the store/caller, not here).

use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::openai_device::{HttpResponse, encode_form};

pub(crate) const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
pub(crate) const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub(crate) const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub(crate) const KIMI_PLATFORM: &str = "kimi_code_cli";

const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Refresh attempts for transient failures (429/5xx/transport); sleeps 1s
/// then 2s between attempts, mirroring the reference client's 3-attempt
/// exponential backoff.
const REFRESH_MAX_ATTEMPTS: u32 = 3;

pub(crate) type Result<T> = std::result::Result<T, KimiAuthError>;
type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>>;
type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum KimiAuthError {
    #[error("Kimi device authorization failed with status {status}")]
    DeviceAuthorizationStatus { status: u16 },
    #[error("Kimi device-code login timed out after 15 minutes")]
    TimedOut,
    #[error("Kimi device code expired before authorization completed; run `dirge auth kimi` again")]
    DeviceCodeExpired,
    #[error("Kimi device authorization was denied")]
    AccessDenied,
    #[error("Kimi device-code polling failed with status {status}")]
    PollStatus { status: u16 },
    #[error("Kimi OAuth token refresh failed with status {status}")]
    RefreshStatus { status: u16 },
    #[error("Kimi OAuth session is no longer authorized; run `dirge auth kimi` again")]
    Unauthorized,
    #[error("Kimi OAuth response was invalid: {0}")]
    InvalidResponse(String),
    #[error("Kimi OAuth transport failed: {0}")]
    Transport(String),
}

/// OAuth host for the device flow: `KIMI_CODE_OAUTH_HOST` wins, then
/// `KIMI_OAUTH_HOST`, then the production default.
pub(crate) fn oauth_host() -> String {
    oauth_host_from(|name| std::env::var(name).ok())
}

fn oauth_host_from(env: impl Fn(&str) -> Option<String>) -> String {
    for name in ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"] {
        if let Some(host) = env(name).filter(|host| !host.trim().is_empty()) {
            return host.trim_end_matches('/').to_string();
        }
    }
    DEFAULT_OAUTH_HOST.to_string()
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DeviceAuthorization {
    pub(crate) user_code: String,
    pub(crate) device_code: String,
    pub(crate) verification_uri: String,
    pub(crate) verification_uri_complete: String,
    pub(crate) interval: Duration,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `verification_uri_complete` embeds the user code in its query
        // string, so it is redacted alongside it (it is still printed to
        // stdout at login — the user needs it to authorize).
        f.debug_struct("DeviceAuthorization")
            .field("user_code", &"[REDACTED]")
            .field("device_code", &"[REDACTED]")
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &"[REDACTED]")
            .field("interval", &self.interval)
            .finish()
    }
}

/// A Kimi token bundle with the expiry already resolved to epoch
/// milliseconds (the reference client's `expires_at = now + expires_in`).
/// `expires_in` itself is not retained — only the resolved instant matters
/// to the store and the refresh seam.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TokenInfo {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at_epoch_ms: i64,
    pub(crate) scope: String,
    pub(crate) token_type: String,
}

impl fmt::Debug for TokenInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenInfo")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at_epoch_ms", &self.expires_at_epoch_ms)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .finish()
    }
}

/// One device-token poll outcome. `Pending` covers both
/// `authorization_pending` and `slow_down` (the reference client keeps the
/// server-provided interval for both); the terminal variants map to their
/// own errors in [`KimiDeviceAuthFlow::complete_device_login`].
#[derive(Debug)]
pub(crate) enum DevicePoll {
    Success(TokenInfo),
    Pending,
    Expired,
    Denied,
}

pub(crate) trait KimiDeviceAuthHttp: Clone + Send + Sync + 'static {
    fn post_form(&self, url: String, form: Vec<(String, String)>) -> HttpFuture<'_>;
}

pub(crate) trait KimiDeviceAuthRuntime: Clone + Send + Sync + 'static {
    fn now(&self) -> Instant;

    /// Wall clock for resolving `expires_in` to an absolute expiry instant.
    /// Separate from `now()` (a monotonic `Instant` for timeouts) so tests
    /// can pin the persisted expiry without touching the real clock.
    fn now_epoch_ms(&self) -> i64;

    fn sleep(&self, duration: Duration) -> SleepFuture<'_>;
}

#[derive(Clone)]
pub(crate) struct ReqwestKimiDeviceAuthHttp {
    client: reqwest::Client,
    identity_headers: Vec<(String, String)>,
}

impl Default for ReqwestKimiDeviceAuthHttp {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            identity_headers: kimi_device_headers(),
        }
    }
}

impl ReqwestKimiDeviceAuthHttp {
    #[cfg(test)]
    fn with_identity_headers(identity_headers: Vec<(String, String)>) -> Self {
        Self {
            client: reqwest::Client::new(),
            identity_headers,
        }
    }

    async fn response(response: reqwest::Response) -> Result<HttpResponse> {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| KimiAuthError::Transport(err.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

impl KimiDeviceAuthHttp for ReqwestKimiDeviceAuthHttp {
    fn post_form(&self, url: String, form: Vec<(String, String)>) -> HttpFuture<'_> {
        Box::pin(async move {
            let body = encode_form(&form);
            let mut request = self
                .client
                .post(url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .header(reqwest::header::ACCEPT, "application/json")
                .timeout(REQUEST_TIMEOUT)
                .body(body);
            for (name, value) in &self.identity_headers {
                request = request.header(name, value);
            }
            let response = request
                .send()
                .await
                .map_err(|err| KimiAuthError::Transport(err.to_string()))?;
            Self::response(response).await
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TokioKimiDeviceAuthRuntime;

impl KimiDeviceAuthRuntime for TokioKimiDeviceAuthRuntime {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn now_epoch_ms(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
            Err(_) => 0,
        }
    }

    fn sleep(&self, duration: Duration) -> SleepFuture<'_> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Clone)]
pub(crate) struct KimiDeviceAuthFlow<H = ReqwestKimiDeviceAuthHttp, R = TokioKimiDeviceAuthRuntime> {
    oauth_host: String,
    client_id: String,
    http: H,
    runtime: R,
    timeout: Duration,
}

impl Default for KimiDeviceAuthFlow<ReqwestKimiDeviceAuthHttp, TokioKimiDeviceAuthRuntime> {
    fn default() -> Self {
        Self::with_parts(
            oauth_host(),
            KIMI_CLIENT_ID,
            ReqwestKimiDeviceAuthHttp::default(),
            TokioKimiDeviceAuthRuntime,
        )
    }
}

impl<H, R> KimiDeviceAuthFlow<H, R> {
    pub(crate) fn with_parts(
        oauth_host: impl Into<String>,
        client_id: impl Into<String>,
        http: H,
        runtime: R,
    ) -> Self {
        let oauth_host = oauth_host.into().trim_end_matches('/').to_string();
        Self {
            oauth_host,
            client_id: client_id.into(),
            http,
            runtime,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl<H, R> KimiDeviceAuthFlow<H, R>
where
    H: KimiDeviceAuthHttp,
    R: KimiDeviceAuthRuntime,
{
    pub(crate) async fn request_device_authorization(&self) -> Result<DeviceAuthorization> {
        let response = self
            .http
            .post_form(
                format!("{}/api/oauth/device_authorization", self.oauth_host),
                vec![("client_id".to_string(), self.client_id.clone())],
            )
            .await?;

        match response.status {
            200..=299 => parse_device_authorization(&response.body),
            status => Err(KimiAuthError::DeviceAuthorizationStatus { status }),
        }
    }

    /// A single device-token poll. The looping driver is
    /// [`Self::complete_device_login`]; keeping the single shot separate
    /// makes the pending/expired/denied mapping directly testable.
    pub(crate) async fn poll_device_token(&self, device_code: &str) -> Result<DevicePoll> {
        let response = self
            .http
            .post_form(
                format!("{}/api/oauth/token", self.oauth_host),
                vec![
                    ("client_id".to_string(), self.client_id.clone()),
                    ("device_code".to_string(), device_code.to_string()),
                    (
                        "grant_type".to_string(),
                        DEVICE_CODE_GRANT_TYPE.to_string(),
                    ),
                ],
            )
            .await?;

        match response.status {
            200..=299 => Ok(DevicePoll::Success(parse_token_info(
                &response.body,
                self.runtime.now_epoch_ms(),
            )?)),
            // HTTP ≥ 500 is a hard error per the reference client — no
            // graceful "pending" interpretation of a server failure.
            status if status >= 500 => Err(KimiAuthError::PollStatus { status }),
            status => {
                let error_code = json_str_field(&response.body, "error");
                match error_code.as_deref() {
                    Some("authorization_pending") | Some("slow_down") => Ok(DevicePoll::Pending),
                    Some("expired_token") => Ok(DevicePoll::Expired),
                    Some("access_denied") => Ok(DevicePoll::Denied),
                    _ => Err(KimiAuthError::PollStatus { status }),
                }
            }
        }
    }

    /// Poll at the server-provided interval until the user authorizes, the
    /// code expires / is denied, or the overall 15-minute timeout hits.
    pub(crate) async fn complete_device_login(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<TokenInfo> {
        let start = self.runtime.now();
        loop {
            match self.poll_device_token(&authorization.device_code).await? {
                DevicePoll::Success(token) => return Ok(token),
                DevicePoll::Pending => {
                    let elapsed = self.runtime.now().duration_since(start);
                    if elapsed >= self.timeout {
                        return Err(KimiAuthError::TimedOut);
                    }
                    let remaining = self.timeout.saturating_sub(elapsed);
                    self.runtime
                        .sleep(authorization.interval.min(remaining))
                        .await;
                }
                DevicePoll::Expired => return Err(KimiAuthError::DeviceCodeExpired),
                DevicePoll::Denied => return Err(KimiAuthError::AccessDenied),
            }
        }
    }

    /// Exchange a refresh token for a rotated token bundle.
    ///
    /// Transient failures (429, 5xx, transport errors) get
    /// [`REFRESH_MAX_ATTEMPTS`] attempts with 1s/2s backoff; 401/403 or an
    /// `invalid_grant` error body means the session is dead and maps to
    /// [`KimiAuthError::Unauthorized`] so callers can direct the user at
    /// `dirge auth kimi` instead of retrying forever.
    pub(crate) async fn refresh_access_token(&self, refresh_token: &str) -> Result<TokenInfo> {
        let url = format!("{}/api/oauth/token", self.oauth_host);
        let form = vec![
            ("client_id".to_string(), self.client_id.clone()),
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ];

        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.http.post_form(url.clone(), form.clone()).await;
            match result {
                Err(err) => {
                    if attempt < REFRESH_MAX_ATTEMPTS {
                        self.runtime.sleep(refresh_backoff(attempt)).await;
                        continue;
                    }
                    return Err(err);
                }
                Ok(response) => match response.status {
                    200..=299 => {
                        return parse_token_info(&response.body, self.runtime.now_epoch_ms());
                    }
                    401 | 403 => return Err(KimiAuthError::Unauthorized),
                    429 | 500..=599 => {
                        if attempt < REFRESH_MAX_ATTEMPTS {
                            self.runtime.sleep(refresh_backoff(attempt)).await;
                            continue;
                        }
                        return Err(KimiAuthError::RefreshStatus {
                            status: response.status,
                        });
                    }
                    status => {
                        if json_str_field(&response.body, "error").as_deref()
                            == Some("invalid_grant")
                        {
                            return Err(KimiAuthError::Unauthorized);
                        }
                        return Err(KimiAuthError::RefreshStatus { status });
                    }
                },
            }
        }
    }
}

fn refresh_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.saturating_sub(1))
}

/// Parse and validate the device-authorization response. `user_code`,
/// `device_code`, and `verification_uri_complete` are load-bearing —
/// accepting them empty would fail mysteriously later (the reference client
/// rejects the same three).
fn parse_device_authorization(body: &str) -> Result<DeviceAuthorization> {
    let value = parse_response(body)?;
    let user_code = required_str(&value, "user_code")?;
    let device_code = required_str(&value, "device_code")?;
    let verification_uri_complete = required_str(&value, "verification_uri_complete")?;
    let verification_uri = value
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let interval = match value.get("interval") {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    let interval = interval.filter(|interval| *interval > 0);
    Ok(DeviceAuthorization {
        user_code,
        device_code,
        verification_uri,
        verification_uri_complete,
        interval: Duration::from_secs(interval.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)),
    })
}

/// Parse and validate a token response (device-code grant or refresh grant
/// — same shape). `access_token`, `refresh_token`, and a positive
/// `expires_in` are required; `scope`/`token_type` default like the
/// reference client.
fn parse_token_info(body: &str, now_epoch_ms: i64) -> Result<TokenInfo> {
    let value = parse_response(body)?;
    let access_token = required_str(&value, "access_token")?;
    let refresh_token = required_str(&value, "refresh_token")?;
    let expires_in_seconds = match value.get("expires_in") {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
    .filter(|expires_in| *expires_in > 0)
    .ok_or_else(|| {
        KimiAuthError::InvalidResponse("missing or invalid expires_in".to_string())
    })?;
    let expires_in_ms = i64::try_from(expires_in_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
    Ok(TokenInfo {
        access_token,
        refresh_token,
        expires_at_epoch_ms: now_epoch_ms.saturating_add(expires_in_ms),
        scope: value
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        token_type: value
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_string(),
    })
}

fn required_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| KimiAuthError::InvalidResponse(format!("missing {field}")))
}

fn json_str_field(body: &str, field: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get(field).and_then(Value::as_str).map(str::to_string))
}

fn parse_response(body: &str) -> Result<Value> {
    serde_json::from_str(body).map_err(|err| {
        let reason = match err.classify() {
            serde_json::error::Category::Io => "I/O error while parsing JSON",
            serde_json::error::Category::Syntax => "invalid JSON syntax",
            serde_json::error::Category::Data => "unexpected JSON shape",
            serde_json::error::Category::Eof => "truncated JSON response",
        };
        KimiAuthError::InvalidResponse(reason.to_string())
    })
}

// ── Device identity headers (mirrors `identity.ts`) ────────────────────

fn kimi_device_id_path() -> PathBuf {
    crate::session::storage::dirs_path().join("kimi_device_id")
}

/// Stable per-machine device id, persisted at `path` (0600 file, 0700
/// parent dir) like the reference client's `device_id` file. Best-effort:
/// a persistence failure still returns the freshly minted in-memory id
/// for this process.
fn kimi_device_id_at(path: &Path) -> String {
    if let Ok(contents) = std::fs::read_to_string(path) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    persist_device_id(path, &id);
    id
}

fn persist_device_id(path: &Path, id: &str) {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
        && std::fs::create_dir_all(parent).is_ok()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    if crate::fs_atomic::atomic_write_sync(path, id.as_bytes()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// The `X-Msh-*` identity set plus a `dirge/<version>` User-Agent, sent on
/// OAuth requests (here) and on every managed-API request (`kimi_http.rs`).
pub(crate) fn kimi_device_headers() -> Vec<(String, String)> {
    kimi_device_headers_at(&kimi_device_id_path())
}

fn kimi_device_headers_at(device_id_path: &Path) -> Vec<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    vec![
        ("User-Agent".to_string(), format!("dirge/{version}")),
        ("X-Msh-Platform".to_string(), KIMI_PLATFORM.to_string()),
        ("X-Msh-Version".to_string(), version.to_string()),
        ("X-Msh-Device-Name".to_string(), ascii_header(&host_name())),
        ("X-Msh-Device-Model".to_string(), ascii_header(&device_model())),
        (
            "X-Msh-Os-Version".to_string(),
            ascii_header(std::env::consts::OS),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            kimi_device_id_at(device_id_path),
        ),
    ]
}

/// `std` exposes no hostname syscall; the shell env vars cover the
/// interactive cases and anything else falls back to "unknown" — the header
/// is telemetry, not an auth input.
fn host_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
}

fn device_model() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Headers must be printable ASCII; strip anything else and fall back to
/// "unknown" when nothing survives (mirrors `asciiHeader` in the reference
/// client).
fn ascii_header(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| ('\u{20}'..='\u{7E}').contains(c))
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
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

    impl KimiDeviceAuthHttp for FakeHttp {
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

    #[derive(Clone)]
    struct FakeRuntime {
        start: Instant,
        epoch_ms: i64,
        elapsed: Arc<Mutex<Duration>>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    impl FakeRuntime {
        fn new(epoch_ms: i64) -> Self {
            Self {
                start: Instant::now(),
                epoch_ms,
                elapsed: Arc::new(Mutex::new(Duration::ZERO)),
                sleeps: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl KimiDeviceAuthRuntime for FakeRuntime {
        fn now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }

        fn now_epoch_ms(&self) -> i64 {
            self.epoch_ms
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

    fn flow(http: FakeHttp, runtime: FakeRuntime) -> KimiDeviceAuthFlow<FakeHttp, FakeRuntime> {
        KimiDeviceAuthFlow::with_parts("https://auth.kimi.com", "client-test", http, runtime)
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dirge_kimi_device_{tag}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn device_id_path(&self) -> PathBuf {
            self.0.join("nested").join("kimi_device_id")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn oauth_host_env_precedence_and_default() {
        assert_eq!(oauth_host_from(|_| None), DEFAULT_OAUTH_HOST);
        assert_eq!(
            oauth_host_from(|name| (name == "KIMI_CODE_OAUTH_HOST")
                .then(|| "https://staging.kimi.com/".to_string())),
            "https://staging.kimi.com"
        );
        assert_eq!(
            oauth_host_from(|name| (name == "KIMI_OAUTH_HOST")
                .then(|| "https://legacy.kimi.com".to_string())),
            "https://legacy.kimi.com"
        );
        // Empty values are ignored.
        assert_eq!(
            oauth_host_from(|_| Some("  ".to_string())),
            DEFAULT_OAUTH_HOST
        );
    }

    #[test]
    fn debug_impls_redact_secret_values() {
        let authorization = DeviceAuthorization {
            user_code: "USER-CODE".to_string(),
            device_code: "DEVICE-CODE".to_string(),
            verification_uri: "https://kimi.com/device".to_string(),
            verification_uri_complete: "https://kimi.com/device?code=USER-CODE".to_string(),
            interval: Duration::from_secs(5),
        };
        let token = TokenInfo {
            access_token: "ACCESS-TOKEN".to_string(),
            refresh_token: "REFRESH-TOKEN".to_string(),
            expires_at_epoch_ms: 1_700_000_900_000,
            scope: "scope".to_string(),
            token_type: "Bearer".to_string(),
        };

        for debug in [
            format!("{authorization:?}"),
            format!("{token:?}"),
            format!("{:?}", DevicePoll::Success(token.clone())),
        ] {
            assert!(debug.contains("[REDACTED]"));
            for secret in ["USER-CODE", "DEVICE-CODE", "ACCESS-TOKEN", "REFRESH-TOKEN"] {
                assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
            }
        }
    }

    #[tokio::test]
    async fn device_authorization_posts_client_id_form_and_parses_response() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "user_code": "USER-CODE",
                "device_code": "DEVICE-CODE",
                "verification_uri": "https://kimi.com/device",
                "verification_uri_complete": "https://kimi.com/device?code=USER-CODE",
                "expires_in": 600,
                "interval": 3
            }),
        )]);

        let authorization = flow(http.clone(), FakeRuntime::new(0))
            .request_device_authorization()
            .await
            .unwrap();

        assert_eq!(authorization.user_code, "USER-CODE");
        assert_eq!(authorization.device_code, "DEVICE-CODE");
        assert_eq!(authorization.verification_uri, "https://kimi.com/device");
        assert_eq!(
            authorization.verification_uri_complete,
            "https://kimi.com/device?code=USER-CODE"
        );
        assert_eq!(authorization.interval, Duration::from_secs(3));
        assert_eq!(
            http.requests(),
            vec![RecordedRequest {
                url: "https://auth.kimi.com/api/oauth/device_authorization".to_string(),
                form: vec![("client_id".to_string(), "client-test".to_string())],
            }]
        );
    }

    #[tokio::test]
    async fn device_authorization_defaults_interval_and_rejects_missing_fields() {
        for body in [
            json!({
                "device_code": "DEVICE-CODE",
                "verification_uri_complete": "https://kimi.com/device?code=X"
            }),
            json!({
                "user_code": "USER-CODE",
                "verification_uri_complete": "https://kimi.com/device?code=X"
            }),
            json!({
                "user_code": "USER-CODE",
                "device_code": "DEVICE-CODE"
            }),
        ] {
            let http = FakeHttp::new([response(200, body)]);
            let err = flow(http, FakeRuntime::new(0))
                .request_device_authorization()
                .await
                .unwrap_err();
            assert!(matches!(err, KimiAuthError::InvalidResponse(_)));
        }

        // Missing interval falls back to the default; a zero interval is
        // clamped to it as well.
        for body in [
            json!({
                "user_code": "USER-CODE",
                "device_code": "DEVICE-CODE",
                "verification_uri_complete": "https://kimi.com/device?code=X"
            }),
            json!({
                "user_code": "USER-CODE",
                "device_code": "DEVICE-CODE",
                "verification_uri_complete": "https://kimi.com/device?code=X",
                "interval": 0
            }),
        ] {
            let http = FakeHttp::new([response(200, body)]);
            let authorization = flow(http, FakeRuntime::new(0))
                .request_device_authorization()
                .await
                .unwrap();
            assert_eq!(
                authorization.interval,
                Duration::from_secs(DEFAULT_POLL_INTERVAL_SECONDS)
            );
        }
    }

    #[tokio::test]
    async fn device_authorization_error_status_does_not_echo_body() {
        let http = FakeHttp::new([response(403, json!({"error": "ACCESS-TOKEN REFRESH-TOKEN"}))]);

        let err = flow(http, FakeRuntime::new(0))
            .request_device_authorization()
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(matches!(
            err,
            KimiAuthError::DeviceAuthorizationStatus { status: 403 }
        ));
        assert!(!message.contains("ACCESS-TOKEN"));
        assert!(!message.contains("REFRESH-TOKEN"));
    }

    #[tokio::test]
    async fn poll_maps_pending_slow_down_expired_denied_and_server_errors() {
        let cases: [(serde_json::Value, u16, &str); 6] = [
            (json!({"error": "authorization_pending"}), 400, "pending"),
            (json!({"error": "slow_down"}), 400, "pending"),
            (json!({"error": "expired_token"}), 400, "expired"),
            (json!({"error": "access_denied"}), 403, "denied"),
            (json!({"error": "something_else"}), 400, "poll-status"),
            (json!({"error": "authorization_pending"}), 500, "poll-status"),
        ];
        for (body, status, expected) in cases {
            let http = FakeHttp::new([response(status, body)]);
            let outcome = flow(http, FakeRuntime::new(0))
                .poll_device_token("DEVICE-CODE")
                .await;
            match expected {
                "pending" => assert!(matches!(outcome.unwrap(), DevicePoll::Pending)),
                "expired" => assert!(matches!(outcome.unwrap(), DevicePoll::Expired)),
                "denied" => assert!(matches!(outcome.unwrap(), DevicePoll::Denied)),
                _ => assert!(matches!(
                    outcome.unwrap_err(),
                    KimiAuthError::PollStatus { .. }
                )),
            }
        }
    }

    #[tokio::test]
    async fn poll_success_posts_device_code_grant_and_resolves_expiry() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "ACCESS-TOKEN",
                "refresh_token": "REFRESH-TOKEN",
                "expires_in": 900,
                "scope": "coding",
                "token_type": "Bearer"
            }),
        )]);
        let runtime = FakeRuntime::new(1_700_000_000_000);

        let poll = flow(http.clone(), runtime)
            .poll_device_token("DEVICE-CODE")
            .await
            .unwrap();

        let DevicePoll::Success(token) = poll else {
            panic!("expected success poll");
        };
        assert_eq!(token.access_token, "ACCESS-TOKEN");
        assert_eq!(token.refresh_token, "REFRESH-TOKEN");
        assert_eq!(token.expires_at_epoch_ms, 1_700_000_900_000);
        assert_eq!(token.scope, "coding");
        assert_eq!(token.token_type, "Bearer");
        assert_eq!(
            http.requests(),
            vec![RecordedRequest {
                url: "https://auth.kimi.com/api/oauth/token".to_string(),
                form: vec![
                    ("client_id".to_string(), "client-test".to_string()),
                    ("device_code".to_string(), "DEVICE-CODE".to_string()),
                    (
                        "grant_type".to_string(),
                        DEVICE_CODE_GRANT_TYPE.to_string()
                    ),
                ],
            }]
        );
    }

    #[tokio::test]
    async fn complete_login_sleeps_between_pending_polls_until_success() {
        let http = FakeHttp::new([
            response(400, json!({"error": "authorization_pending"})),
            response(
                200,
                json!({
                    "access_token": "ACCESS-TOKEN",
                    "refresh_token": "REFRESH-TOKEN",
                    "expires_in": 900
                }),
            ),
        ]);
        let runtime = FakeRuntime::new(1_700_000_000_000);
        let authorization = DeviceAuthorization {
            user_code: "USER-CODE".to_string(),
            device_code: "DEVICE-CODE".to_string(),
            verification_uri: String::new(),
            verification_uri_complete: "https://kimi.com/device?code=USER-CODE".to_string(),
            interval: Duration::from_secs(4),
        };

        let token = flow(http.clone(), runtime.clone())
            .complete_device_login(&authorization)
            .await
            .unwrap();

        assert_eq!(token.access_token, "ACCESS-TOKEN");
        assert_eq!(runtime.sleeps(), vec![Duration::from_secs(4)]);
        assert_eq!(http.requests().len(), 2);
    }

    #[tokio::test]
    async fn complete_login_maps_expired_and_denied_to_terminal_errors() {
        for (body, assert_fn) in [
            (
                json!({"error": "expired_token"}),
                KimiAuthError::DeviceCodeExpired,
            ),
            (json!({"error": "access_denied"}), KimiAuthError::AccessDenied),
        ] {
            let http = FakeHttp::new([response(400, body)]);
            let authorization = DeviceAuthorization {
                user_code: "USER-CODE".to_string(),
                device_code: "DEVICE-CODE".to_string(),
                verification_uri: String::new(),
                verification_uri_complete: "https://kimi.com/device?code=X".to_string(),
                interval: Duration::from_secs(5),
            };
            let err = flow(http, FakeRuntime::new(0))
                .complete_device_login(&authorization)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), assert_fn.to_string());
        }
    }

    #[tokio::test]
    async fn pending_poll_times_out_without_real_sleeping() {
        let http = FakeHttp::new(
            std::iter::repeat_with(|| response(400, json!({"error": "authorization_pending"})))
                .take(4),
        );
        let runtime = FakeRuntime::new(0);
        let authorization = DeviceAuthorization {
            user_code: "USER-CODE".to_string(),
            device_code: "DEVICE-CODE".to_string(),
            verification_uri: String::new(),
            verification_uri_complete: "https://kimi.com/device?code=X".to_string(),
            interval: Duration::from_secs(300),
        };

        let err = flow(http, runtime.clone())
            .complete_device_login(&authorization)
            .await
            .unwrap_err();

        assert!(matches!(err, KimiAuthError::TimedOut));
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
    async fn refresh_posts_refresh_grant_and_parses_rotated_bundle() {
        let http = FakeHttp::new([response(
            200,
            json!({
                "access_token": "NEW-ACCESS",
                "refresh_token": "NEW-REFRESH",
                "expires_in": 900
            }),
        )]);
        let runtime = FakeRuntime::new(1_700_000_000_000);
        let flow = flow(http.clone(), runtime);

        let token = flow.refresh_access_token("OLD-REFRESH").await.unwrap();

        assert_eq!(token.access_token, "NEW-ACCESS");
        assert_eq!(token.refresh_token, "NEW-REFRESH");
        assert_eq!(token.expires_at_epoch_ms, 1_700_000_900_000);
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .form
            .contains(&("grant_type".to_string(), "refresh_token".to_string())));
        assert!(requests[0]
            .form
            .contains(&("refresh_token".to_string(), "OLD-REFRESH".to_string())));
        assert!(requests[0]
            .form
            .contains(&("client_id".to_string(), "client-test".to_string())));
    }

    #[tokio::test]
    async fn refresh_retries_transient_failures_with_backoff() {
        let http = FakeHttp::new([
            response(429, json!({"error": "rate_limited"})),
            response(503, json!({"error": "unavailable"})),
            response(
                200,
                json!({
                    "access_token": "NEW-ACCESS",
                    "refresh_token": "NEW-REFRESH",
                    "expires_in": 900
                }),
            ),
        ]);
        let runtime = FakeRuntime::new(1_700_000_000_000);

        let token = flow(http, runtime.clone())
            .refresh_access_token("OLD-REFRESH")
            .await
            .unwrap();

        assert_eq!(token.access_token, "NEW-ACCESS");
        assert_eq!(
            runtime.sleeps(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[tokio::test]
    async fn refresh_retries_transport_errors_then_surfaces_them() {
        let http = FakeHttp::new([
            Err(KimiAuthError::Transport("connection reset".to_string())),
            Err(KimiAuthError::Transport("connection reset".to_string())),
            Err(KimiAuthError::Transport("connection reset".to_string())),
        ]);
        let runtime = FakeRuntime::new(0);

        let err = flow(http, runtime.clone())
            .refresh_access_token("OLD-REFRESH")
            .await
            .unwrap_err();

        assert!(matches!(err, KimiAuthError::Transport(_)));
        assert_eq!(
            runtime.sleeps(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[tokio::test]
    async fn refresh_exhausts_retryable_statuses_without_echoing_body() {
        let http = FakeHttp::new([
            response(429, json!({"error": "OLD-REFRESH"})),
            response(500, json!({"error": "OLD-REFRESH"})),
            response(502, json!({"error": "OLD-REFRESH"})),
        ]);
        let runtime = FakeRuntime::new(0);

        let err = flow(http, runtime)
            .refresh_access_token("OLD-REFRESH")
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(matches!(
            err,
            KimiAuthError::RefreshStatus { status: 502 }
        ));
        assert!(!message.contains("OLD-REFRESH"));
    }

    #[tokio::test]
    async fn refresh_maps_unauthorized_statuses_and_invalid_grant() {
        for (status, body) in [
            (401, json!({"error": "unauthorized"})),
            (403, json!({"error": "forbidden"})),
            (400, json!({"error": "invalid_grant"})),
        ] {
            let http = FakeHttp::new([response(status, body.clone())]);
            let err = flow(http, FakeRuntime::new(0))
                .refresh_access_token("OLD-REFRESH")
                .await
                .unwrap_err();
            assert!(
                matches!(err, KimiAuthError::Unauthorized),
                "status {status} with {body} must map to Unauthorized, got {err}"
            );
        }
    }

    #[tokio::test]
    async fn token_response_requires_access_refresh_and_expires_in() {
        for body in [
            json!({"refresh_token": "R", "expires_in": 900}),
            json!({"access_token": "A", "expires_in": 900}),
            json!({"access_token": "A", "refresh_token": "R"}),
            json!({"access_token": "A", "refresh_token": "R", "expires_in": 0}),
            json!({"access_token": "A", "refresh_token": "R", "expires_in": -5}),
        ] {
            let http = FakeHttp::new([response(200, body)]);
            let err = flow(http, FakeRuntime::new(0))
                .refresh_access_token("OLD-REFRESH")
                .await
                .unwrap_err();
            assert!(matches!(err, KimiAuthError::InvalidResponse(_)));
        }

        // scope / token_type default like the reference client.
        let http = FakeHttp::new([response(
            200,
            json!({"access_token": "A", "refresh_token": "R", "expires_in": 900}),
        )]);
        let token = flow(http, FakeRuntime::new(0))
            .refresh_access_token("OLD-REFRESH")
            .await
            .unwrap();
        assert_eq!(token.scope, "");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn device_id_is_minted_persisted_and_reused() {
        let dir = TestDir::new("mint");
        let path = dir.device_id_path();

        let first = kimi_device_id_at(&path);
        let second = kimi_device_id_at(&path);

        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        uuid::Uuid::parse_str(&first).expect("device id is a UUID");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
    }

    #[test]
    fn device_id_ignores_empty_file_and_remints() {
        let dir = TestDir::new("remint");
        let path = dir.device_id_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "   \n").unwrap();

        let id = kimi_device_id_at(&path);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), id);
        uuid::Uuid::parse_str(&id).expect("device id is a UUID");
    }

    #[test]
    fn device_headers_carry_the_full_identity_set() {
        let dir = TestDir::new("headers");
        let headers = kimi_device_headers_at(&dir.device_id_path());
        let map: std::collections::HashMap<_, _> = headers.into_iter().collect();

        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            map.get("User-Agent").map(String::as_str),
            Some(format!("dirge/{version}").as_str())
        );
        assert_eq!(
            map.get("X-Msh-Platform").map(String::as_str),
            Some(KIMI_PLATFORM)
        );
        assert_eq!(
            map.get("X-Msh-Version").map(String::as_str),
            Some(version)
        );
        for name in [
            "X-Msh-Device-Name",
            "X-Msh-Device-Model",
            "X-Msh-Os-Version",
        ] {
            assert!(
                map.get(name).is_some_and(|value| !value.is_empty()),
                "{name} must be present"
            );
        }
        let device_id = map.get("X-Msh-Device-Id").unwrap();
        uuid::Uuid::parse_str(device_id).expect("device id header is a UUID");
    }

    #[test]
    fn ascii_header_strips_non_printable_and_falls_back() {
        assert_eq!(ascii_header("hello world"), "hello world");
        assert_eq!(ascii_header("  spaced  "), "spaced");
        assert_eq!(ascii_header("emoji-\u{1F600}-host"), "emoji--host");
        assert_eq!(ascii_header(""), "unknown");
        assert_eq!(ascii_header("\u{2603}"), "unknown");
    }

    #[tokio::test]
    async fn reqwest_http_attaches_identity_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                use tokio::io::AsyncReadExt;
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            use tokio::io::AsyncWriteExt;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8_lossy(&buffer).to_string()
        });

        let http = ReqwestKimiDeviceAuthHttp::with_identity_headers(vec![
            ("X-Msh-Platform".to_string(), KIMI_PLATFORM.to_string()),
            ("X-Msh-Device-Id".to_string(), "device-1".to_string()),
            ("User-Agent".to_string(), "dirge/0.0.0-test".to_string()),
        ]);
        http.post_form(
            format!("http://{address}/api/oauth/token"),
            vec![("client_id".to_string(), "client-test".to_string())],
        )
        .await
        .unwrap();
        let request = server.await.unwrap();

        assert!(request.contains("x-msh-platform: kimi_code_cli"));
        assert!(request.contains("x-msh-device-id: device-1"));
        assert!(request.contains("user-agent: dirge/0.0.0-test"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("client_id=client-test"));
    }
}
