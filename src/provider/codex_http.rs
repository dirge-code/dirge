use std::sync::Arc;

use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

/// Re-resolves the ChatGPT/Codex OAuth bearer (and its expiry) when the frozen
/// one expires mid-session. Boxed so tests can inject a fake; the live seam
/// wraps `load_fresh_openai_oauth`, which refreshes-and-persists (dirge-30nl).
pub(crate) use crate::provider::refreshable_token::RefreshFn as CodexRefreshFn;
use crate::provider::refreshable_token::{self, RefreshableToken};

/// ChatGPT/Codex sits at no refresh margin today — its tokens are
/// long-lived enough to cross the expiry boundary rarely. See the field
/// docs on `RefreshableToken::margin_ms`.
const REFRESH_MARGIN_MS: i64 = 0;

// `token` is `Option` only to satisfy the `HttpClientExt: Default` bound; a
// default instance never rewrites the Authorization header.
#[derive(Clone, Default)]
pub(crate) struct CodexHttpClient {
    inner: reqwest::Client,
    token: Option<Arc<RefreshableToken>>,
}

// Redacts the token so it can't leak via `{:?}`.
impl std::fmt::Debug for CodexHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexHttpClient")
            .field("bearer_token", &"<redacted>")
            .finish()
    }
}

impl CodexHttpClient {
    /// The live OAuth path: seed with the bearer + expiry resolved at build,
    /// plus a refresher that re-resolves (and persists) a fresh credential once
    /// the token expires mid-session (dirge-30nl).
    pub(crate) fn new_refreshable(
        bearer_token: String,
        expires_at_ms: Option<i64>,
        refresher: CodexRefreshFn,
    ) -> Self {
        Self {
            inner: reqwest::Client::new(),
            token: Some(Arc::new(RefreshableToken::renewable(
                bearer_token,
                expires_at_ms,
                refresher,
                REFRESH_MARGIN_MS,
                "chatgpt-codex",
            ))),
        }
    }

    /// True when this client rewrites the Authorization header from a
    /// refreshable token (the OAuth path); false for the default pass-through
    /// client. Used to assert the Codex refresh seam wiring (dirge-8gdv.4).
    #[cfg(test)]
    pub(crate) fn is_refreshable(&self) -> bool {
        self.token.is_some()
    }

    // Rig 0.37's OpenAI Responses adapter moves `preamble` into the
    // first `input` system message, then serializes `instructions: null`.
    // The ChatGPT Codex backend wants the opposite shape: a non-empty
    // Responses-native `instructions` field, no `system` role in
    // `input`, and `store: false`. Keep the fix inside Dirge by
    // normalizing the outgoing `/responses` JSON body at the
    // transport boundary instead of vendoring or forking rig-core.
    /// dirge-bz0a: the bearer is resolved by the CALLER, in async context,
    /// because a mid-session renewal blocks (see `refreshable_token`). This
    /// stays sync and pure so it can run inline once the bearer is in hand.
    fn normalized_request(
        req: Request<Bytes>,
        bearer: Option<String>,
    ) -> http_client::Result<Request<Bytes>> {
        let (mut parts, body) = req.into_parts();
        // Overwrite the build-time bearer with the freshly resolved one
        // (dirge-30nl). Absent a refreshable token the header is left as rig
        // set it from the static api_key.
        if let Some(bearer) = bearer
            && let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {bearer}"))
        {
            parts.headers.insert(http::header::AUTHORIZATION, value);
        }
        let body = if is_responses_path(parts.uri.path()) {
            normalize_codex_responses_body(body)
        } else {
            body
        };

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

impl HttpClientExt for CodexHttpClient {
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
        // `T` is not `'static` and the returned future is; the body
        // conversion is the only step that needs `T`, so it happens here.
        let req: Request<Bytes> = req.map(Into::into);
        async move {
            let req = Self::normalized_request(req, refreshable_token::bearer(token).await)?;
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
        let is_responses_stream = is_responses_path(req.uri().path());
        let token = self.token.clone();
        let req: Request<Bytes> = req.map(Into::into);
        async move {
            let req = Self::normalized_request(req, refreshable_token::bearer(token).await)?;
            let mut response = inner.send_streaming(req).await?;
            if is_responses_stream
                && !response
                    .headers()
                    .contains_key(reqwest::header::CONTENT_TYPE)
            {
                response.headers_mut().insert(
                    reqwest::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("text/event-stream"),
                );
            }
            Ok(response)
        }
    }
}

/// Same reasoning as the Anthropic client: the inner is a plain
/// `reqwest::Client`, so delegate to the header-preserving path instead of
/// letting rig's status check drop the `HeaderMap`. OpenAI reports its
/// `x-ratelimit-reset-*` windows in headers only.
impl super::compressing_http::StreamingWithHeaders for CodexHttpClient {
    fn send_streaming_with_headers(
        &self,
        req: http::Request<Bytes>,
    ) -> impl Future<Output = super::compressing_http::StreamingSend> + Send {
        use super::compressing_http::StreamingSend;
        let inner = self.inner.clone();
        let is_responses_stream = is_responses_path(req.uri().path());
        let token = self.token.clone();
        async move {
            let req = Self::normalized_request(req, refreshable_token::bearer(token).await);
            let mut send = match req {
                Ok(req) => inner.send_streaming_with_headers(req).await,
                Err(e) => StreamingSend {
                    result: Err(e),
                    headers: None,
                },
            };
            // rig's `openai` (Responses) provider does not set
            // `.allow_missing_content_type()` (only the `chatgpt` provider
            // does), so the Codex/ChatGPT `/responses` endpoint — which omits
            // a Content-Type rig's strict SSE decoder accepts — is rejected as
            // `Invalid content type was returned`. This seam is the only
            // streaming path `CompressingHttpClient` routes through, so the
            // injection that used to live in `send_streaming` must live here.
            if is_responses_stream
                && let Ok(response) = send.result.as_mut()
                && !response
                    .headers()
                    .contains_key(reqwest::header::CONTENT_TYPE)
            {
                response.headers_mut().insert(
                    reqwest::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("text/event-stream"),
                );
            }
            send
        }
    }
}

fn is_responses_path(path: &str) -> bool {
    path.ends_with("/responses")
}

fn normalize_codex_responses_body(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };

    let instructions = if value
        .as_object()
        .and_then(|obj| obj.get("instructions"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .is_some_and(|instructions| !instructions.is_empty())
    {
        None
    } else {
        // Rig has already preserved Dirge's actual system prompt in
        // `input`; we mirror that into the Responses-native field Codex
        // requires. The fallback is intentionally minimal and should only
        // matter for malformed/test requests with no system input.
        Some(extract_system_instructions(&value).unwrap_or_else(|| ".".to_string()))
    };

    let Some(obj) = value.as_object_mut() else {
        return body;
    };
    if let Some(instructions) = instructions {
        obj.insert(
            "instructions".to_string(),
            serde_json::Value::String(instructions),
        );
    }
    obj.insert("store".to_string(), serde_json::Value::Bool(false));
    strip_system_input_items(obj);

    serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
}

fn strip_system_input_items(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(input) = obj
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    input.retain(|item| item.get("role").and_then(serde_json::Value::as_str) != Some("system"));
}

fn extract_system_instructions(value: &serde_json::Value) -> Option<String> {
    let input = value.get("input")?.as_array()?;
    // Collect EVERY system message, not just the first: `strip_system_input_
    // items` deletes all of them, so lifting only the first would silently
    // drop any later system content. Join them in order.
    let combined = input
        .iter()
        .filter(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        .filter_map(extract_message_text)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Some(combined).filter(|text| !text.is_empty())
}

fn extract_message_text(item: &serde_json::Value) -> Option<String> {
    match item.get("content")? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("content"))
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::auth::RefreshedAuth;

    /// Drive one request through the real path: resolve the client's
    /// bearer the way `send` does (async since dirge-bz0a, so a renewal
    /// cannot block the runtime) and normalize with it.
    async fn authorization(client: &CodexHttpClient, preexisting: Option<&str>) -> Option<String> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("https://api/responses");
        if let Some(bearer) = preexisting {
            builder = builder.header(http::header::AUTHORIZATION, bearer);
        }
        let req = builder.body(Bytes::from("{}")).unwrap();
        let bearer = refreshable_token::bearer(client.token.clone()).await;
        let out =
            CodexHttpClient::normalized_request(req, bearer).expect("normalization must succeed");
        out.headers()
            .get(http::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn refreshable_client_overwrites_authorization_with_refreshed_bearer() {
        let refresher: CodexRefreshFn = Arc::new(|| {
            Ok(RefreshedAuth {
                bearer_token: "FRESH".to_string(),
                expires_at_ms: Some(i64::MAX),
            })
        });
        // expiry in the past -> the refresher fires on the first request.
        let client = CodexHttpClient::new_refreshable("STALE".to_string(), Some(0), refresher);

        assert_eq!(
            authorization(&client, Some("Bearer STALE"))
                .await
                .as_deref(),
            Some("Bearer FRESH")
        );
    }

    #[tokio::test]
    async fn refreshable_client_keeps_fresh_bearer_without_refreshing() {
        let refresher: CodexRefreshFn = Arc::new(|| panic!("must not refresh a fresh token"));
        let client =
            CodexHttpClient::new_refreshable("CURRENT".to_string(), Some(i64::MAX), refresher);

        assert_eq!(
            authorization(&client, Some("Bearer CURRENT"))
                .await
                .as_deref(),
            Some("Bearer CURRENT")
        );
    }

    #[tokio::test]
    async fn refresh_failure_falls_back_to_the_stale_bearer() {
        let refresher: CodexRefreshFn = Arc::new(|| anyhow::bail!("network down"));
        let client = CodexHttpClient::new_refreshable("STALE".to_string(), Some(0), refresher);

        // Fail-open: the request still carries the old bearer (no regression vs
        // the frozen-header behavior) rather than dropping Authorization.
        assert_eq!(
            authorization(&client, Some("Bearer STALE"))
                .await
                .as_deref(),
            Some("Bearer STALE")
        );
    }

    #[tokio::test]
    async fn default_client_leaves_authorization_untouched() {
        let client = CodexHttpClient::default();

        assert_eq!(
            authorization(&client, Some("Bearer PREEXISTING"))
                .await
                .as_deref(),
            Some("Bearer PREEXISTING")
        );
    }

    #[tokio::test]
    async fn merges_multiple_system_messages_into_instructions() {
        // `strip_system_input_items` deletes ALL system items, so every
        // system message must be lifted into `instructions` — not just the
        // first — or the rest would be silently lost.
        let body = Bytes::from(
            serde_json::json!({
                "input": [
                    { "role": "system", "content": "First." },
                    { "role": "system", "content": "Second." },
                    { "role": "user", "content": "Hi" }
                ]
            })
            .to_string(),
        );

        let value: serde_json::Value =
            serde_json::from_slice(&normalize_codex_responses_body(body)).unwrap();

        assert_eq!(value["instructions"], "First.\nSecond.");
        // Both system items stripped; only the user item remains.
        assert_eq!(value["input"].as_array().unwrap().len(), 1);
        assert_eq!(value["input"][0]["role"], "user");
    }

    #[tokio::test]
    async fn injects_responses_instructions_from_system_input() {
        let body = Bytes::from(
            serde_json::json!({
                "model": "gpt-5",
                "input": [
                    {
                        "type": "message",
                        "role": "system",
                        "content": [{ "type": "input_text", "text": "Follow Dirge instructions." }]
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Hi" }]
                    }
                ]
            })
            .to_string(),
        );

        let value: serde_json::Value =
            serde_json::from_slice(&normalize_codex_responses_body(body)).unwrap();

        assert_eq!(value["instructions"], "Follow Dirge instructions.");
        assert_eq!(value["store"], false);
        assert_eq!(value["input"].as_array().unwrap().len(), 1);
        assert_eq!(value["input"][0]["role"], "user");
    }

    #[tokio::test]
    async fn preserves_existing_instructions_but_still_strips_system_input() {
        let body = Bytes::from(
            serde_json::json!({
                "instructions": "Existing",
                "input": [
                    { "role": "system", "content": "Replacement" }
                ]
            })
            .to_string(),
        );

        let value: serde_json::Value =
            serde_json::from_slice(&normalize_codex_responses_body(body)).unwrap();

        assert_eq!(value["instructions"], "Existing");
        assert_eq!(value["store"], false);
        assert!(value["input"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn overrides_true_store_for_codex_backend() {
        let body = Bytes::from(
            serde_json::json!({
                "store": true,
                "input": [
                    { "role": "user", "content": "Hi" }
                ]
            })
            .to_string(),
        );

        let value: serde_json::Value =
            serde_json::from_slice(&normalize_codex_responses_body(body)).unwrap();

        assert_eq!(value["store"], false);
    }

    /// #718 regression: when streaming was re-routed through the
    /// `send_streaming_with_headers` seam, the `Content-Type: text/event-stream`
    /// fixup the old `send_streaming` did for the Codex/ChatGPT `/responses`
    /// endpoint was dropped. That endpoint omits a content-type rig's strict
    /// SSE decoder accepts, and rig's `openai` (Responses) provider does not
    /// call `.allow_missing_content_type()` (only the `chatgpt` provider does),
    /// so the stream was rejected as `Invalid content type was returned`. The
    /// seam must restore the injection on the happy-path `/responses` response.
    #[tokio::test]
    async fn responses_stream_injects_text_event_stream_when_absent() {
        use crate::provider::compressing_http::StreamingWithHeaders;

        // Loopback server: 200 OK with NO Content-Type, mirroring the Codex
        // /responses streaming endpoint. No mock crate needed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = b"data: {\"type\":\"response.completed\"}\n\n";
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
            let _ = sock.flush().await;
        });

        let client = CodexHttpClient::default();
        let req = Request::builder()
            .method("POST")
            .uri(format!("http://{addr}/responses"))
            .body(Bytes::from_static(b"{}"))
            .unwrap();

        let sent = client.send_streaming_with_headers(req).await;
        let response = sent
            .result
            .expect("happy-path /responses stream must succeed");
        let ct = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .expect("Content-Type must be injected on a /responses stream lacking one");
        assert_eq!(ct, "text/event-stream");
    }
}
