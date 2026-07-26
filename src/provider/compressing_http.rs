use bytes::Bytes;
use futures::StreamExt;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use std::pin::Pin;

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

/// Outcome of a [`StreamingWithHeaders`] send: the rig-shaped result, plus the
/// response headers when the inner client was able to keep them on a non-2xx.
/// `None` means rig already flattened the non-2xx into status+body and the body
/// text is all the caller has.
pub(super) struct StreamingSend {
    pub(super) result: http_client::Result<StreamingResponse>,
    pub(super) headers: Option<http::HeaderMap>,
}

/// A streaming send that also surfaces the response headers when the server
/// returns a non-2xx.
///
/// rig's [`HttpClientExt::send_streaming`] flattens any non-2xx into
/// `InvalidStatusCodeWithMessage(status, body)` and drops the `HeaderMap`
/// before dirge can inspect it, so providers that report rate-limit state ONLY
/// in real HTTP headers (OpenAI's `x-ratelimit-reset-requests`, Groq's
/// `retry-after`, …) are invisible on the streaming path — which carries every
/// provider request in dirge. For the raw `reqwest::Client` inner we drive the
/// request ourselves instead of delegating that status decision to rig (see
/// [`reqwest_streaming_with_headers`]); every other inner falls back to
/// ordinary rig delegation, so nothing about its behaviour changes.
pub(super) trait StreamingWithHeaders {
    fn send_streaming_with_headers(
        &self,
        req: http::Request<Bytes>,
    ) -> impl Future<Output = StreamingSend> + Send;
}

impl StreamingWithHeaders for reqwest::Client {
    fn send_streaming_with_headers(
        &self,
        req: http::Request<Bytes>,
    ) -> impl Future<Output = StreamingSend> + Send {
        reqwest_streaming_with_headers(self, req)
    }
}

/// Drive a streaming request through `reqwest` directly so the response headers
/// survive a non-2xx, then package the result for the generic
/// [`CompressingHttpClient::send_streaming`] caller.
///
/// This mirrors rig's `reqwest::Client::send_streaming` on the happy path —
/// status → copy the headers → map the byte stream into a `BoxedStream` — and
/// diverges only on the non-2xx branch, where rig hands the `reqwest::Response`
/// to its private `non_success_status_error` (which keeps the status and body
/// text but throws the `HeaderMap` away). We keep the headers instead and hand
/// back the exact same error rig would have produced, so `classify_error` and
/// every test that matches on `Invalid status code {status} with message:
/// {body}` are unaffected.
fn reqwest_streaming_with_headers(
    client: &reqwest::Client,
    req: http::Request<Bytes>,
) -> Pin<Box<dyn Future<Output = StreamingSend> + Send>> {
    let client = client.clone();
    Box::pin(async move {
        let (parts, body) = req.into_parts();
        let built = client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .build();
        let reqwest_req = match built {
            Ok(req) => req,
            // A build failure is a request error, not a response: there is no
            // `HeaderMap` to keep and no behaviour change versus rig, which
            // maps this to an `Instance` error.
            Err(error) => {
                return StreamingSend {
                    result: Err(http_client::Error::Instance(Box::new(error))),
                    headers: None,
                };
            }
        };
        let response = match client.execute(reqwest_req).await {
            Ok(response) => response,
            Err(error) => {
                return StreamingSend {
                    result: Err(http_client::Error::Instance(Box::new(error))),
                    headers: None,
                };
            }
        };
        if !response.status().is_success() {
            // The whole point: keep the headers rig would discard.
            let headers = response.headers().clone();
            let status = response.status();
            let message = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
            return StreamingSend {
                result: Err(http_client::Error::InvalidStatusCodeWithMessage(
                    status, message,
                )),
                headers: Some(headers),
            };
        }

        // Happy path: rebuild the `StreamingResponse` exactly the way rig does.
        let mut builder = http::Response::builder()
            .status(response.status())
            .version(response.version());
        if let Some(headers) = builder.headers_mut() {
            *headers = response.headers().clone();
        }
        let stream: http_client::sse::BoxedStream = Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|error| http_client::Error::Instance(Box::new(error)))),
        );
        StreamingSend {
            result: builder.body(stream).map_err(http_client::Error::Protocol),
            headers: None,
        }
    })
}

impl<Inner> HttpClientExt for CompressingHttpClient<Inner>
where
    Inner: HttpClientExt + StreamingWithHeaders + Clone + Send + Sync + 'static,
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
            let StreamingSend { result, headers } = inner.send_streaming_with_headers(req).await;
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
                    // A real `HeaderMap` means the inner drove reqwest itself
                    // and kept what rig's status-check would have discarded;
                    // otherwise rig already flattened the non-2xx into
                    // status+body and the body is all we have (OpenRouter
                    // nests its `X-RateLimit-*` inside it).
                    if let Some(hdrs) = &headers {
                        super::rate_limit_gate::note_from_headers(&endpoint, hdrs);
                    } else {
                        super::rate_limit_gate::note_from_error(&endpoint, &e.to_string());
                    }
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
        /// Headers surfaced on the streaming 429, to exercise the header-aware
        /// send path. `None` for the existing body-only tests.
        streaming_headers: Option<http::HeaderMap>,
    }

    impl MockClient {
        fn new(error: &str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                error: Arc::new(error.to_string()),
                streaming_headers: None,
            }
        }

        /// Like [`MockClient::new`], but the streaming path surfaces these
        /// response headers alongside the canned 429 — mirroring what the real
        /// reqwest inner keeps instead of letting rig discard them.
        fn new_with_headers(error: &str, headers: http::HeaderMap) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                error: Arc::new(error.to_string()),
                streaming_headers: Some(headers),
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

    impl super::StreamingWithHeaders for MockClient {
        fn send_streaming_with_headers(
            &self,
            req: http::Request<Bytes>,
        ) -> impl Future<Output = super::StreamingSend> + Send {
            let this = self.clone();
            async move {
                let result = this.send_streaming(req).await;
                super::StreamingSend {
                    result,
                    headers: this.streaming_headers.clone(),
                }
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

    /// A streaming 429 that reports its rate-limit state ONLY in headers (no
    /// signal in the body) must still latch the gate. rig flattens a non-2xx
    /// into status+body and throws the `HeaderMap` away, so the streaming path
    /// otherwise can't see `x-ratelimit-remaining`/`-reset` at all — the canned
    /// body here carries nothing the body parser can use, so the latch can
    /// only come from the surfaced headers.
    #[tokio::test]
    async fn streaming_429_latches_from_real_headers() {
        let host = "gate-test-stream-headers.invalid";
        let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp_millis();
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());

        let inner = MockClient::new_with_headers("Too Many Requests", headers);
        let calls = inner.calls.clone();
        let c = client(inner);
        super::super::rate_limit_gate::clear(host);

        // First request reaches the inner and comes back 429 carrying headers.
        let first = c.send_streaming(request_to(host)).await;
        assert!(first.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The headers are definitive (remaining 0 + a future reset), so the
        // next request must be suppressed locally and never reach the inner.
        let err = match c.send_streaming(request_to(host)).await {
            Ok(_) => panic!("the second request must be suppressed"),
            Err(e) => e.to_string(),
        };
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the suppressed request must never reach the inner client"
        );
        assert!(
            err.contains("did not send this request"),
            "suppressed error should say so: {err}"
        );

        super::super::rate_limit_gate::clear(host);
    }

    /// One-shot loopback HTTP/1.1 server: accepts a single connection, drains
    /// the request, and writes a canned response. Lets the reqwest-backed
    /// streaming path be exercised end-to-end without a real provider or a mock
    /// crate. Returns the URL to hit.
    async fn serve_once(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        let response = response.into_bytes();
        // Owned copy: the spawned task must be 'static.
        let body = body.to_vec();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain whatever the client sent so it can't RST mid-write.
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&response).await;
            let _ = sock.write_all(&body).await;
            let _ = sock.flush().await;
        });

        format!("http://{addr}/")
    }

    fn client_reqwest() -> CompressingHttpClient<reqwest::Client> {
        CompressingHttpClient::new(
            reqwest::Client::new(),
            crate::llmtrim::ir::ProviderKind::OpenAi,
            std::sync::Arc::new(crate::compression::dirge_default_config()),
            false,
        )
    }

    fn request_to_url(url: &str) -> Request<Bytes> {
        Request::builder()
            .method("POST")
            .uri(url)
            .body(Bytes::from_static(b"{}"))
            .unwrap()
    }

    /// The reqwest-backed streaming path must keep the response headers on a
    /// non-2xx (what rig discards) so `note_from_headers` can latch the gate,
    /// and the surfaced error must stay byte-compatible with rig's
    /// `Invalid status code {status} with message: {body}`.
    #[tokio::test]
    async fn reqwest_streaming_keeps_headers_on_429() {
        let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp_millis();
        let url = serve_once(
            "429 Too Many Requests",
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset.to_string()),
            ],
            b"Too Many Requests",
        )
        .await;

        // The endpoint key dirge uses is the URI authority (host:port).
        let key = super::endpoint_key(&url.parse::<http::Uri>().unwrap());
        super::super::rate_limit_gate::clear(&key);

        let c = client_reqwest();
        // `StreamingResponse` is not `Debug`, so extract the error by hand.
        let msg = match c.send_streaming(request_to_url(&url)).await {
            Ok(_) => panic!("a 429 must surface as an error"),
            Err(e) => e.to_string(),
        };

        assert!(
            msg.starts_with("Invalid status code "),
            "unexpected shape: {msg}"
        );
        assert!(msg.contains("429"), "status must be 429: {msg}");
        assert!(
            msg.ends_with("with message: Too Many Requests"),
            "body must be preserved byte-for-byte: {msg}"
        );
        assert!(
            super::super::rate_limit_gate::remaining(&key).is_some(),
            "the response headers must have latched the gate"
        );

        super::super::rate_limit_gate::clear(&key);
    }

    /// The reqwest-backed happy path must still stream the SSE bytes through
    /// unchanged — the header-capture rework must not regress the streaming
    /// behaviour that carries every provider request in dirge.
    #[tokio::test]
    async fn reqwest_streaming_preserves_sse_bytes_on_2xx() {
        let payload = b"data: hello\n\n";
        let url = serve_once("200 OK", &[("content-type", "text/event-stream")], payload).await;

        let c = client_reqwest();
        let resp = c
            .send_streaming(request_to_url(&url))
            .await
            .expect("a 2xx must stream");

        use futures::StreamExt;
        let mut body = resp.into_body();
        let mut collected = Vec::new();
        while let Some(chunk) = body.next().await {
            collected.extend_from_slice(&chunk.expect("chunk must decode"));
        }
        assert_eq!(collected.as_slice(), &payload[..]);
    }
}
