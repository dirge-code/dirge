//! Kimi Code managed-API transport.
//!
//! Modelled on `codex_http.rs` minus the Responses-body normalization (the
//! Kimi coding API serves plain OpenAI-compatible chat completions, so the
//! body passes through untouched). Two jobs:
//!   - rewrite `Authorization` from a refreshable bearer — Kimi access
//!     tokens live only 15 minutes, so a long session MUST renew mid-flight
//!     rather than die on a non-retryable 401 (same seam as dirge-30nl);
//!   - inject the `X-Msh-*` / `User-Agent` device identity headers the
//!     managed service expects on every API request (see
//!     `auth::kimi_device::kimi_device_headers`).

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

use crate::provider::auth::RefreshedAuth;

/// Re-resolves the Kimi OAuth bearer (and its expiry) when the frozen one
/// expires mid-session. Boxed so tests can inject a fake; the live seam
/// wraps `load_fresh_kimi_oauth`, which refreshes-and-persists.
pub(crate) type KimiRefreshFn = Arc<dyn Fn() -> anyhow::Result<RefreshedAuth> + Send + Sync>;

struct TokenState {
    bearer: String,
    /// `None` means "never refresh" — an API-key / env token with no
    /// refresh grant that Dirge doesn't manage.
    expires_at_ms: Option<i64>,
}

struct RefreshableToken {
    state: Mutex<TokenState>,
    refresher: KimiRefreshFn,
}

impl RefreshableToken {
    /// Current bearer, refreshing first if it has expired. A refresh failure
    /// keeps the stale token so the request fails exactly as it would
    /// without the seam rather than wedging the client; the next request
    /// retries. Refresh is rare (once per token lifetime) so doing it
    /// synchronously is acceptable.
    fn bearer(&self) -> String {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(expires_at) = state.expires_at_ms {
            let now = chrono::Utc::now().timestamp_millis();
            if crate::auth::file_store::epoch_ms_is_expired(expires_at, now) {
                match (self.refresher)() {
                    Ok(fresh) => {
                        state.bearer = fresh.bearer_token;
                        state.expires_at_ms = fresh.expires_at_ms;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "dirge::provider",
                            error = %e,
                            "Kimi OAuth token expired and refresh failed; sending the stale token",
                        );
                    }
                }
            }
        }
        state.bearer.clone()
    }
}

/// A never-called refresher for the static (non-Dirge-OAuth) path: with
/// `expires_at_ms: None` the refresh branch can never fire, but the field
/// still needs a value. Kept as one token code path for both constructors.
fn never_refresh() -> KimiRefreshFn {
    Arc::new(|| {
        anyhow::bail!("static Kimi credentials are not refreshable; this refresher is unreachable")
    })
}

#[derive(Clone)]
pub(crate) struct KimiHttpClient {
    inner: reqwest::Client,
    token: Option<Arc<RefreshableToken>>,
    identity_headers: Arc<http::HeaderMap>,
}

// `token` is `Option` only to satisfy the `HttpClientExt: Default` bound; a
// default instance never rewrites the Authorization header and injects no
// identity headers.
impl Default for KimiHttpClient {
    fn default() -> Self {
        Self {
            inner: reqwest::Client::new(),
            token: None,
            identity_headers: Arc::new(http::HeaderMap::new()),
        }
    }
}

// Redacts the token so it can't leak via `{:?}`.
impl std::fmt::Debug for KimiHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KimiHttpClient")
            .field("bearer_token", &"<redacted>")
            .finish()
    }
}

impl KimiHttpClient {
    /// The static credential path (API key / `KIMI_CODE_API_KEY` env
    /// token): identity headers are injected and the bearer is fixed — it
    /// is identical to the one rig sets from `api_key`, the rewrite just
    /// keeps a single token code path.
    pub(crate) fn new(bearer_token: String) -> Self {
        Self::with_identity_headers(
            Some(bearer_token),
            None,
            never_refresh(),
            default_identity_headers(),
        )
    }

    /// The live OAuth path: seed with the bearer + expiry resolved at
    /// build, plus a refresher that re-resolves (and persists) a fresh
    /// credential once the token expires mid-session.
    pub(crate) fn new_refreshable(
        bearer_token: String,
        expires_at_ms: Option<i64>,
        refresher: KimiRefreshFn,
    ) -> Self {
        Self::with_identity_headers(
            Some(bearer_token),
            expires_at_ms,
            refresher,
            default_identity_headers(),
        )
    }

    /// Testable core — identity headers injected so tests don't touch the
    /// on-disk device id.
    #[cfg(test)]
    fn with_identity_headers_for_test(
        bearer_token: String,
        expires_at_ms: Option<i64>,
        refresher: KimiRefreshFn,
        identity_headers: http::HeaderMap,
    ) -> Self {
        Self::with_identity_headers(
            Some(bearer_token),
            expires_at_ms,
            refresher,
            identity_headers,
        )
    }

    fn with_identity_headers(
        bearer_token: Option<String>,
        expires_at_ms: Option<i64>,
        refresher: KimiRefreshFn,
        identity_headers: http::HeaderMap,
    ) -> Self {
        Self {
            inner: reqwest::Client::new(),
            token: bearer_token.map(|bearer| {
                Arc::new(RefreshableToken {
                    state: Mutex::new(TokenState {
                        bearer,
                        expires_at_ms,
                    }),
                    refresher,
                })
            }),
            identity_headers: Arc::new(identity_headers),
        }
    }

    /// Rewrite the Authorization header from the (possibly refreshed)
    /// bearer and inject the device identity headers. The body passes
    /// through unchanged — unlike the Codex transport there is no
    /// dialect-specific normalization to apply.
    fn normalized_request<T>(&self, req: Request<T>) -> http_client::Result<Request<Bytes>>
    where
        T: Into<Bytes>,
    {
        let (mut parts, body) = req.into_parts();
        // Overwrite the build-time bearer with a freshly resolved one; this
        // is where a mid-session refresh fires if the token has expired.
        // Absent a token the header is left as rig set it from the static
        // api_key.
        if let Some(token) = &self.token
            && let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {}", token.bearer()))
        {
            parts.headers.insert(http::header::AUTHORIZATION, value);
        }
        for (name, value) in self.identity_headers.iter() {
            parts.headers.insert(name, value.clone());
        }

        let mut builder = Request::builder()
            .method(parts.method)
            .uri(parts.uri)
            .version(parts.version);
        if let Some(headers) = builder.headers_mut() {
            *headers = parts.headers;
        }
        builder
            .body(body.into())
            .map_err(http_client::Error::Protocol)
    }
}

/// The production identity header set, minting/reading the persisted device
/// id. Invalid header values (non-ASCII) are dropped rather than failing
/// client construction — the headers are telemetry, not auth.
fn default_identity_headers() -> http::HeaderMap {
    let mut map = http::HeaderMap::new();
    for (name, value) in crate::auth::kimi_device::kimi_device_headers() {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(&value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

impl HttpClientExt for KimiHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes>,
        T: Send,
        U: From<Bytes>,
        U: Send + 'static,
    {
        let inner = self.inner.clone();
        let req = self.normalized_request(req);
        async move {
            let req = req?;
            inner.send(req).await
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let inner = self.inner.clone();
        let req = self.normalized_request(req);
        async move {
            let req = req?;
            inner.send_streaming(req).await
        }
    }
}

/// Same reasoning as the Codex client: the inner is a plain
/// `reqwest::Client`, so delegate to the header-preserving path instead of
/// letting rig's status check drop the `HeaderMap`.
impl super::compressing_http::StreamingWithHeaders for KimiHttpClient {
    fn send_streaming_with_headers(
        &self,
        req: http::Request<Bytes>,
    ) -> impl Future<Output = super::compressing_http::StreamingSend> + Send {
        use super::compressing_http::StreamingSend;
        let inner = self.inner.clone();
        let req = self.normalized_request(req);
        async move {
            match req {
                Ok(req) => inner.send_streaming_with_headers(req).await,
                Err(e) => StreamingSend {
                    result: Err(e),
                    headers: None,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_headers() -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        map.insert(
            http::HeaderName::from_static("x-msh-platform"),
            http::HeaderValue::from_static("kimi_code_cli"),
        );
        map.insert(
            http::HeaderName::from_static("x-msh-device-id"),
            http::HeaderValue::from_static("device-1"),
        );
        map.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("dirge/0.0.0-test"),
        );
        map
    }

    fn client(
        bearer: &str,
        expires_at_ms: Option<i64>,
        refresher: KimiRefreshFn,
    ) -> KimiHttpClient {
        KimiHttpClient::with_identity_headers_for_test(
            bearer.to_string(),
            expires_at_ms,
            refresher,
            identity_headers(),
        )
    }

    fn request(client: &KimiHttpClient, preexisting: Option<&str>) -> http::HeaderMap {
        let mut builder = Request::builder()
            .method("POST")
            .uri("https://api.kimi.com/coding/v1/chat/completions");
        if let Some(bearer) = preexisting {
            builder = builder.header(http::header::AUTHORIZATION, bearer);
        }
        let req = builder.body(Bytes::from("{}")).unwrap();
        client.normalized_request(req).unwrap().headers().clone()
    }

    fn authorization(headers: &http::HeaderMap) -> Option<String> {
        headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn refreshable_client_overwrites_authorization_with_refreshed_bearer() {
        let refresher: KimiRefreshFn = Arc::new(|| {
            Ok(RefreshedAuth {
                bearer_token: "FRESH".to_string(),
                expires_at_ms: Some(i64::MAX),
            })
        });
        // expiry in the past -> the refresher fires on the first request.
        let client = client("STALE", Some(0), refresher);

        assert_eq!(
            authorization(&request(&client, Some("Bearer STALE"))).as_deref(),
            Some("Bearer FRESH")
        );
    }

    #[test]
    fn refreshable_client_keeps_fresh_bearer_without_refreshing() {
        let refresher: KimiRefreshFn = Arc::new(|| panic!("must not refresh a fresh token"));
        let client = client("CURRENT", Some(i64::MAX), refresher);

        assert_eq!(
            authorization(&request(&client, Some("Bearer CURRENT"))).as_deref(),
            Some("Bearer CURRENT")
        );
    }

    #[test]
    fn refresh_failure_falls_back_to_the_stale_bearer() {
        let refresher: KimiRefreshFn = Arc::new(|| anyhow::bail!("network down"));
        let client = client("STALE", Some(0), refresher);

        // Fail-open: the request still carries the old bearer rather than
        // dropping Authorization.
        assert_eq!(
            authorization(&request(&client, Some("Bearer STALE"))).as_deref(),
            Some("Bearer STALE")
        );
    }

    #[test]
    fn static_client_bearer_is_never_refreshed() {
        let client = client("API-KEY", None, never_refresh());

        assert_eq!(
            authorization(&request(&client, Some("Bearer API-KEY"))).as_deref(),
            Some("Bearer API-KEY")
        );
    }

    #[test]
    fn identity_headers_are_injected_on_every_request() {
        let client = client("TOKEN", Some(i64::MAX), never_refresh());
        let headers = request(&client, None);

        assert_eq!(headers.get("x-msh-platform").unwrap(), "kimi_code_cli");
        assert_eq!(headers.get("x-msh-device-id").unwrap(), "device-1");
        assert_eq!(
            headers.get(http::header::USER_AGENT).unwrap(),
            "dirge/0.0.0-test"
        );
    }

    #[test]
    fn default_client_leaves_authorization_and_headers_untouched() {
        let client = KimiHttpClient::default();
        let headers = request(&client, Some("Bearer PREEXISTING"));

        assert_eq!(
            authorization(&headers).as_deref(),
            Some("Bearer PREEXISTING")
        );
        assert!(headers.get("x-msh-platform").is_none());
    }

    #[test]
    fn debug_redacts_bearer_token() {
        let client = client("SUPER-SECRET", Some(i64::MAX), never_refresh());
        let rendered = format!("{client:?}");

        assert!(!rendered.contains("SUPER-SECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
