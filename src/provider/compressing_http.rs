use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

/// Render a request URI for logging with its query string removed. Some
/// providers (notably Gemini, whose rig client builds `…?key=<API_KEY>`) carry
/// the API key in the query, so the raw URI must never reach the logs. Keeps
/// scheme://authority/path — enough to debug routing.
fn log_safe_uri(uri: &str) -> String {
    uri.split('?').next().unwrap_or(uri).to_string()
}

/// Key a rate-limit throttle by host (GH #718). Rate limits are enforced
/// per account at the provider, so every path on a host shares one window —
/// `openrouter.ai` and `api.anthropic.com` must stay independent, but
/// `/chat/completions` and `/completions` on the same host must not.
///
/// Falls back to the query-stripped URI when there is no authority (the
/// shape used by some test and proxy configurations), which is still a
/// stable key even if it over-partitions.
fn endpoint_key(uri: &http::Uri) -> String {
    uri.authority()
        .map(|a| a.host().to_string())
        .unwrap_or_else(|| log_safe_uri(&uri.to_string()))
}

/// Build the error returned in place of a request we declined to send.
fn suppressed(wait: std::time::Duration, scope: Option<&str>) -> http_client::Error {
    http_client::Error::InvalidStatusCodeWithMessage(
        http::StatusCode::TOO_MANY_REQUESTS,
        super::rate_limit_gate::suppressed_error_message(wait, scope),
    )
}

/// Wraps an inner HTTP client and optionally compresses request bodies before
/// delegating — fail-open: any compression error passes the original body
/// through unchanged, so a compression bug can never break a request.
///
/// The `enabled` field gates compression at runtime; set to `false` for a
/// pass-through. Use `DIRGE_COMPRESSION=0` to disable via env.
#[derive(Clone)]
pub(crate) struct CompressingHttpClient<Inner> {
    inner: Inner,
    enabled: bool,
    provider: crate::llmtrim::ir::ProviderKind,
    config: std::sync::Arc<crate::llmtrim::config::DenseConfig>,
}

impl<Inner: Default> Default for CompressingHttpClient<Inner> {
    fn default() -> Self {
        Self {
            inner: Inner::default(),
            enabled: true,
            provider: crate::llmtrim::ir::ProviderKind::OpenAi,
            config: std::sync::Arc::new(crate::compression::dirge_default_config()),
        }
    }
}

impl<Inner: std::fmt::Debug> std::fmt::Debug for CompressingHttpClient<Inner> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompressingHttpClient")
            .field("inner", &self.inner)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl<Inner> CompressingHttpClient<Inner> {
    /// Construct a compressing HTTP client wrapper. Runtime compression is
    /// controlled by the `enabled` field; set to `false` for a pass-through.
    pub fn new(
        inner: Inner,
        provider: crate::llmtrim::ir::ProviderKind,
        config: std::sync::Arc<crate::llmtrim::config::DenseConfig>,
        enabled: bool,
    ) -> Self {
        Self {
            inner,
            enabled,
            provider,
            config,
        }
    }
}

impl<Inner> CompressingHttpClient<Inner> {
    /// Try to compress the body. On any failure, return the original bytes
    /// unchanged — this is the fail-open guard.
    fn maybe_compress(&self, body: Bytes) -> Bytes {
        if self.enabled {
            let body_str = match std::str::from_utf8(&body) {
                Ok(s) => s,
                Err(_) => return body,
            };
            match crate::compression::rewrite_with(body_str, self.provider, &self.config) {
                Ok(compressed) => {
                    tracing::debug!(
                        target: "dirge::compression",
                        before = body.len(),
                        after = compressed.len(),
                        "compressed request body"
                    );
                    return Bytes::from(compressed);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::compression",
                        error = %e,
                        "compression failed; sending original body"
                    );
                }
            }
        }
        body
    }

    fn normalized_request<T>(&self, req: Request<T>) -> http_client::Result<Request<Bytes>>
    where
        T: Into<Bytes>,
    {
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let body = self.maybe_compress(body);
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

impl<Inner> HttpClientExt for CompressingHttpClient<Inner>
where
    Inner: HttpClientExt + Clone + Send + Sync + 'static,
{
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
            let method = req.method().to_string();
            let uri = log_safe_uri(&req.uri().to_string());
            let endpoint = endpoint_key(req.uri());
            // GH #718: the provider already told us this window is empty.
            // Sending anyway is a guaranteed 429 that still counts against
            // the quota — which is how the reporter's daily allowance was
            // consumed by retries alone.
            if let Some((wait, scope)) = super::rate_limit_gate::remaining(&endpoint) {
                tracing::debug!(
                    method = %method,
                    uri = %uri,
                    wait_secs = wait.as_secs(),
                    "request suppressed — provider rate limit still in effect"
                );
                return Err(suppressed(wait, scope.as_deref()));
            }
            let result = inner.send(req).await;
            match &result {
                Ok(resp) => {
                    // Unlike the streaming path, a non-2xx can arrive here
                    // as a real `Response` — headers intact. That is the
                    // only place providers which report their limits ONLY
                    // in headers (Anthropic, OpenAI, Groq) are visible to
                    // us at all, since rig's error conversion drops them.
                    if resp.status() == http::StatusCode::TOO_MANY_REQUESTS {
                        super::rate_limit_gate::note_from_headers(&endpoint, resp.headers());
                    } else if resp.status().is_success() {
                        super::rate_limit_gate::clear(&endpoint);
                    }
                    tracing::debug!(
                        method = %method,
                        uri = %uri,
                        status = resp.status().as_u16(),
                        "HTTP response received"
                    );
                }
                Err(e) => {
                    super::rate_limit_gate::note_from_error(&endpoint, &e.to_string());
                    tracing::debug!(
                        method = %method,
                        uri = %uri,
                        error = %e,
                        "sending HTTP request"
                    );
                }
            }
            result
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
            let method = req.method().to_string();
            let uri = log_safe_uri(&req.uri().to_string());
            let endpoint = endpoint_key(req.uri());
            if let Some((wait, scope)) = super::rate_limit_gate::remaining(&endpoint) {
                tracing::debug!(
                    method = %method,
                    uri = %uri,
                    wait_secs = wait.as_secs(),
                    "streaming request suppressed — provider rate limit still in effect"
                );
                return Err(suppressed(wait, scope.as_deref()));
            }
            let result = inner.send_streaming(req).await;
            match &result {
                Ok(_) => {
                    super::rate_limit_gate::clear(&endpoint);
                    tracing::debug!(
                        method = %method,
                        uri = %uri,
                        "sending HTTP streaming request"
                    );
                }
                Err(e) => {
                    // rig flattens a non-2xx into status + body text here,
                    // so the body is all we get. OpenRouter nests its
                    // `X-RateLimit-*` headers inside that body, which is
                    // what makes the #718 case recoverable at all.
                    super::rate_limit_gate::note_from_error(&endpoint, &e.to_string());
                    tracing::debug!(
                        method = %method,
                        uri = %uri,
                        error = %e,
                        "sending HTTP streaming request"
                    );
                }
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::log_safe_uri;

    #[test]
    fn log_safe_uri_strips_the_query_string() {
        // Gemini carries the API key in `?key=…` — it must not survive into logs.
        assert_eq!(
            log_safe_uri(
                "https://generativelanguage.googleapis.com/v1beta/models/x:generateContent?alt=sse&key=SECRET"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/x:generateContent"
        );
    }

    #[test]
    fn log_safe_uri_leaves_query_less_urls_untouched() {
        assert_eq!(
            log_safe_uri("https://api.cerebras.ai/v1/chat/completions"),
            "https://api.cerebras.ai/v1/chat/completions"
        );
    }

    // ---- GH #718: rate-limit gate wiring ----

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn endpoint_key_is_the_host_so_all_paths_share_one_window() {
        let a: http::Uri = "https://openrouter.ai/api/v1/chat/completions"
            .parse()
            .unwrap();
        let b: http::Uri = "https://openrouter.ai/api/v1/completions".parse().unwrap();
        let other: http::Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        assert_eq!(endpoint_key(&a), "openrouter.ai");
        assert_eq!(endpoint_key(&a), endpoint_key(&b));
        assert_ne!(endpoint_key(&a), endpoint_key(&other));
    }

    /// Inner client that records how many requests reached it and always
    /// fails with a canned error.
    #[derive(Clone)]
    struct MockClient {
        calls: Arc<AtomicUsize>,
        error: Arc<String>,
    }

    impl MockClient {
        fn new(error: &str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                error: Arc::new(error.to_string()),
            }
        }
    }

    impl HttpClientExt for MockClient {
        fn send<T, U>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
        where
            T: Into<Bytes> + Send,
            U: From<Bytes> + Send + 'static,
        {
            let calls = self.calls.clone();
            let error = self.error.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(http_client::Error::InvalidStatusCodeWithMessage(
                    http::StatusCode::TOO_MANY_REQUESTS,
                    error.to_string(),
                ))
            }
        }

        // Unused by these tests; the signature must match the trait, so
        // `async fn` isn't an option here.
        #[allow(clippy::manual_async_fn)]
        fn send_multipart<U>(
            &self,
            _req: Request<MultipartForm>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
        where
            U: From<Bytes> + Send + 'static,
        {
            async move {
                Err(http_client::Error::InvalidStatusCode(
                    http::StatusCode::NOT_IMPLEMENTED,
                ))
            }
        }

        fn send_streaming<T>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
        where
            T: Into<Bytes> + Send,
        {
            let calls = self.calls.clone();
            let error = self.error.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(http_client::Error::InvalidStatusCodeWithMessage(
                    http::StatusCode::TOO_MANY_REQUESTS,
                    error.to_string(),
                ))
            }
        }
    }

    fn client(inner: MockClient) -> CompressingHttpClient<MockClient> {
        CompressingHttpClient::new(
            inner,
            crate::llmtrim::ir::ProviderKind::OpenAi,
            std::sync::Arc::new(crate::compression::dirge_default_config()),
            // Compression is irrelevant here and would only add noise.
            false,
        )
    }

    fn request_to(host: &str) -> Request<Bytes> {
        Request::builder()
            .method("POST")
            .uri(format!("https://{host}/v1/chat/completions"))
            .body(Bytes::from_static(b"{}"))
            .unwrap()
    }

    /// The reporter's per-day 429 must latch the gate, and the NEXT
    /// request to that host must never reach the network. This is the
    /// behaviour that stops a retry storm from eating the daily quota.
    #[tokio::test]
    async fn a_definitive_429_latches_and_the_next_request_is_never_sent() {
        let host = "gate-test-latch.invalid";
        let reset = (chrono::Utc::now() + chrono::Duration::hours(14)).timestamp_millis();
        let body = format!(
            r#"{{"error":{{"message":"Rate limit exceeded: free-models-per-day.","code":429,"metadata":{{"headers":{{"X-RateLimit-Limit":"50","X-RateLimit-Remaining":"0","X-RateLimit-Reset":"{reset}"}}}}}}}}"#
        );
        let inner = MockClient::new(&body);
        let calls = inner.calls.clone();
        let c = client(inner);

        // First request reaches the provider and comes back 429.
        let first = c.send_streaming(request_to(host)).await;
        assert!(first.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second request is refused locally — the counter must not move.
        // `StreamingResponse` is not `Debug`, so unwrap the error by hand.
        let err = match c.send_streaming(request_to(host)).await {
            Err(e) => e,
            Ok(_) => panic!("the second request must be suppressed"),
        };
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the suppressed request must never reach the inner client",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("did not send this request"),
            "suppressed error should say so: {msg}",
        );
        // ...and it must still classify as a usage cap so the run stops
        // cleanly rather than burning its retry budget.
        assert_eq!(
            crate::agent::recovery::classify_error(&msg),
            crate::agent::recovery::ErrorKind::UsageCap,
        );

        super::super::rate_limit_gate::clear(host);
    }

    /// A 429 with no reset information must NOT latch — we were told
    /// nothing definitive, so the ordinary retry path keeps ownership and
    /// subsequent requests still go out.
    #[tokio::test]
    async fn a_bare_429_does_not_suppress_later_requests() {
        let host = "gate-test-bare.invalid";
        let inner = MockClient::new("Too Many Requests");
        let calls = inner.calls.clone();
        let c = client(inner);

        let _ = c.send_streaming(request_to(host)).await;
        let _ = c.send_streaming(request_to(host)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "without a definitive signal both requests should be attempted",
        );
    }

    /// Latching is per host: throttling one provider must not block another.
    #[tokio::test]
    async fn latching_one_host_does_not_suppress_another() {
        let throttled = "gate-test-hostA.invalid";
        let other = "gate-test-hostB.invalid";
        let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp_millis();
        let body = format!(
            r#"{{"error":{{"metadata":{{"headers":{{"X-RateLimit-Remaining":"0","X-RateLimit-Reset":"{reset}"}}}}}},"message":"429 rate limit exceeded: per-hour"}}"#
        );
        let inner = MockClient::new(&body);
        let calls = inner.calls.clone();
        let c = client(inner);

        let _ = c.send_streaming(request_to(throttled)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Same client, different host — must still be attempted.
        let _ = c.send_streaming(request_to(other)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a throttle on one host must not gate another",
        );

        super::super::rate_limit_gate::clear(throttled);
        super::super::rate_limit_gate::clear(other);
    }
}
