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

use std::sync::Arc;

use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

/// Re-resolves the Kimi OAuth bearer (and its expiry) when the frozen one
/// expires mid-session. Boxed so tests can inject a fake; the live seam
/// wraps `load_fresh_kimi_oauth`, which refreshes-and-persists.
pub(crate) use crate::provider::refreshable_token::RefreshFn as KimiRefreshFn;
use crate::provider::refreshable_token::{self, RefreshableToken};

/// dirge-iki5: renew shortly BEFORE the token dies. A 15-minute Kimi
/// access token crosses the expiry boundary every 15 minutes of an active
/// session, and a request that passes the freshness check with
/// milliseconds to spare can still land after expiry — a non-retryable
/// 401 that fails the whole turn.
const REFRESH_MARGIN_MS: i64 = crate::auth::store::KIMI_REFRESH_MARGIN_MS;

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
                Arc::new(RefreshableToken::renewable(
                    bearer,
                    expires_at_ms,
                    refresher,
                    REFRESH_MARGIN_MS,
                    "kimi",
                ))
            }),
            identity_headers: Arc::new(identity_headers),
        }
    }

    /// Rewrite the Authorization header from the (possibly refreshed)
    /// bearer and inject the device identity headers. The body passes
    /// through unchanged — unlike the Codex transport there is no
    /// dialect-specific normalization to apply.
    /// dirge-bz0a: the bearer is resolved by the CALLER, in async context,
    /// because a mid-session renewal blocks (see `refreshable_token`). This
    /// stays sync and pure so it can run inline once the bearer is in hand.
    fn normalized_request(
        req: Request<Bytes>,
        bearer: Option<String>,
        identity_headers: &http::HeaderMap,
    ) -> http_client::Result<Request<Bytes>> {
        let (mut parts, body) = req.into_parts();
        // Overwrite the build-time bearer with the freshly resolved one.
        // Absent a token the header is left as rig set it from the static
        // api_key.
        if let Some(bearer) = bearer
            && let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {bearer}"))
        {
            parts.headers.insert(http::header::AUTHORIZATION, value);
        }
        for (name, value) in identity_headers.iter() {
            parts.headers.insert(name, value.clone());
        }

        let mut builder = Request::builder()
            .method(parts.method)
            .uri(parts.uri)
            .version(parts.version);
        if let Some(headers) = builder.headers_mut() {
            *headers = parts.headers;
        }
        builder.body(body).map_err(http_client::Error::Protocol)
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
        let token = self.token.clone();
        let identity = self.identity_headers.clone();
        // `T` is not `'static` and the returned future is; the body
        // conversion is the only step that needs `T`, so it happens here.
        let req: Request<Bytes> = req.map(Into::into);
        async move {
            let bearer = refreshable_token::bearer(token).await;
            let req = Self::normalized_request(req, bearer, &identity)?;
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
        let token = self.token.clone();
        let identity = self.identity_headers.clone();
        // `T` is not `'static` and the returned future is; the body
        // conversion is the only step that needs `T`, so it happens here.
        let req: Request<Bytes> = req.map(Into::into);
        async move {
            let bearer = refreshable_token::bearer(token).await;
            let req = Self::normalized_request(req, bearer, &identity)?;
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
        let token = self.token.clone();
        let identity = self.identity_headers.clone();
        async move {
            let bearer = refreshable_token::bearer(token).await;
            match Self::normalized_request(req, bearer, &identity) {
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
    use crate::provider::auth::RefreshedAuth;

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

    /// dirge-iki5, and the WIRING of it rather than the mechanism: the
    /// shared token can carry a margin, but only this transport asks for
    /// one. A 15-minute Kimi access token crosses the expiry boundary
    /// every 15 minutes of an active session, and a request that passes
    /// the freshness check with milliseconds to spare still lands after
    /// expiry — a non-retryable 401 that fails the whole turn.
    ///
    /// So a token that is still technically alive, but inside the
    /// margin, must renew. Set the margin to zero and this token is used
    /// as-is, which is the shape of the bug.
    #[tokio::test]
    async fn a_kimi_token_inside_the_refresh_margin_renews_before_it_dies() {
        // Compile-time, and load-bearing: `nearly` below is derived from
        // the margin, so at zero it would land exactly on `now`, read as
        // expired, renew, and the assertion would pass having tested
        // nothing. Zeroing kimi's margin must break the build, not go
        // quiet.
        const {
            assert!(
                REFRESH_MARGIN_MS > 0,
                "kimi runs a refresh margin; without one a 15-minute token 401s every 15 minutes"
            )
        };
        let refresher: KimiRefreshFn = Arc::new(|| {
            Ok(RefreshedAuth {
                bearer_token: "FRESH".to_string(),
                expires_at_ms: Some(i64::MAX),
            })
        });
        // Alive, but only for half the margin.
        let nearly = chrono::Utc::now().timestamp_millis() + REFRESH_MARGIN_MS / 2;
        let client = client("NEARLY-DEAD", Some(nearly), refresher);
        assert_eq!(
            authorization(&request(&client, Some("Bearer NEARLY-DEAD")).await).as_deref(),
            Some("Bearer FRESH"),
        );
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

    /// Drive one request through the real path: resolve the client's
    /// bearer the way `send` does (async since dirge-bz0a, so a renewal
    /// cannot block the runtime) and normalize with it.
    async fn request(client: &KimiHttpClient, preexisting: Option<&str>) -> http::HeaderMap {
        let mut builder = Request::builder()
            .method("POST")
            .uri("https://api.kimi.com/coding/v1/chat/completions");
        if let Some(bearer) = preexisting {
            builder = builder.header(http::header::AUTHORIZATION, bearer);
        }
        let req = builder.body(Bytes::from("{}")).unwrap();
        let bearer = refreshable_token::bearer(client.token.clone()).await;
        KimiHttpClient::normalized_request(req, bearer, &client.identity_headers)
            .expect("normalization must succeed")
            .headers()
            .clone()
    }

    fn authorization(headers: &http::HeaderMap) -> Option<String> {
        headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn refreshable_client_overwrites_authorization_with_refreshed_bearer() {
        let refresher: KimiRefreshFn = Arc::new(|| {
            Ok(RefreshedAuth {
                bearer_token: "FRESH".to_string(),
                expires_at_ms: Some(i64::MAX),
            })
        });
        // expiry in the past -> the refresher fires on the first request.
        let client = client("STALE", Some(0), refresher);

        assert_eq!(
            authorization(&request(&client, Some("Bearer STALE")).await).as_deref(),
            Some("Bearer FRESH")
        );
    }

    #[tokio::test]
    async fn refreshable_client_keeps_fresh_bearer_without_refreshing() {
        let refresher: KimiRefreshFn = Arc::new(|| panic!("must not refresh a fresh token"));
        let client = client("CURRENT", Some(i64::MAX), refresher);

        assert_eq!(
            authorization(&request(&client, Some("Bearer CURRENT")).await).as_deref(),
            Some("Bearer CURRENT")
        );
    }

    #[tokio::test]
    async fn refresh_failure_falls_back_to_the_stale_bearer() {
        let refresher: KimiRefreshFn = Arc::new(|| anyhow::bail!("network down"));
        let client = client("STALE", Some(0), refresher);

        // Fail-open: the request still carries the old bearer rather than
        // dropping Authorization.
        assert_eq!(
            authorization(&request(&client, Some("Bearer STALE")).await).as_deref(),
            Some("Bearer STALE")
        );
    }

    #[tokio::test]
    async fn static_client_bearer_is_never_refreshed() {
        let client = client("API-KEY", None, never_refresh());

        assert_eq!(
            authorization(&request(&client, Some("Bearer API-KEY")).await).as_deref(),
            Some("Bearer API-KEY")
        );
    }

    #[tokio::test]
    async fn identity_headers_are_injected_on_every_request() {
        let client = client("TOKEN", Some(i64::MAX), never_refresh());
        let headers = request(&client, None).await;

        assert_eq!(headers.get("x-msh-platform").unwrap(), "kimi_code_cli");
        assert_eq!(headers.get("x-msh-device-id").unwrap(), "device-1");
        assert_eq!(
            headers.get(http::header::USER_AGENT).unwrap(),
            "dirge/0.0.0-test"
        );
    }

    #[tokio::test]
    async fn default_client_leaves_authorization_and_headers_untouched() {
        let client = KimiHttpClient::default();
        let headers = request(&client, Some("Bearer PREEXISTING")).await;

        assert_eq!(
            authorization(&headers).as_deref(),
            Some("Bearer PREEXISTING")
        );
        assert!(headers.get("x-msh-platform").is_none());
    }

    #[tokio::test]
    async fn debug_redacts_bearer_token() {
        let client = client("SUPER-SECRET", Some(i64::MAX), never_refresh());
        let rendered = format!("{client:?}");

        assert!(!rendered.contains("SUPER-SECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
