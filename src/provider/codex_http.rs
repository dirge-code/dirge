use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

use crate::provider::auth::RefreshedAuth;

/// Re-resolves the ChatGPT/Codex OAuth bearer (and its expiry) when the frozen
/// one expires mid-session. Boxed so tests can inject a fake; the live seam
/// wraps `load_fresh_openai_oauth`, which refreshes-and-persists (dirge-30nl).
pub(crate) type CodexRefreshFn = Arc<dyn Fn() -> anyhow::Result<RefreshedAuth> + Send + Sync>;

struct TokenState {
    bearer: String,
    /// `None` means "never refresh" — an env/legacy-file token with no
    /// refresh grant that Dirge doesn't manage.
    expires_at_ms: Option<i64>,
}

/// A ChatGPT/Codex bearer that renews itself when it expires part-way through a
/// long session. Pre-fix the bearer was baked into a static `HeaderMap` at
/// client build, so a run that crossed token expiry died on a non-retryable
/// 401 (dirge-30nl). Mirrors the Anthropic seam but is deliberately kept
/// separate — this is a different transport and conflating them would widen the
/// auth-flow blast radius.
struct RefreshableToken {
    state: Mutex<TokenState>,
    refresher: CodexRefreshFn,
}

impl RefreshableToken {
    /// Current bearer, refreshing first if it has expired. A refresh failure
    /// keeps the stale token so the request fails exactly as it did before this
    /// fix rather than wedging the client; the next request retries. Refresh is
    /// rare (once per token lifetime) so doing it synchronously is acceptable.
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
                            "ChatGPT/Codex OAuth token expired and refresh failed; sending the stale token",
                        );
                    }
                }
            }
        }
        state.bearer.clone()
    }
}

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
            token: Some(Arc::new(RefreshableToken {
                state: Mutex::new(TokenState {
                    bearer: bearer_token,
                    expires_at_ms,
                }),
                refresher,
            })),
        }
    }

    // Rig 0.37's OpenAI Responses adapter moves `preamble` into the
    // first `input` system message, then serializes `instructions: null`.
    // The ChatGPT Codex backend wants the opposite shape: a non-empty
    // Responses-native `instructions` field, no `system` role in
    // `input`, and `store: false`. Keep the fix inside Dirge by
    // normalizing the outgoing `/responses` JSON body at the
    // transport boundary instead of vendoring or forking rig-core.
    fn normalized_request<T>(&self, req: Request<T>) -> http_client::Result<Request<Bytes>>
    where
        T: Into<Bytes>,
    {
        let (mut parts, body) = req.into_parts();
        // Overwrite the build-time bearer with a freshly resolved one; this is
        // where a mid-session refresh fires if the token has expired
        // (dirge-30nl). Absent a refreshable token the header is left as rig
        // set it from the static api_key.
        if let Some(token) = &self.token
            && let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {}", token.bearer()))
        {
            parts.headers.insert(http::header::AUTHORIZATION, value);
        }
        let body = body.into();
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
        let is_responses_stream = is_responses_path(req.uri().path());
        let req = self.normalized_request(req);
        async move {
            let req = req?;
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

    fn authorization(client: &CodexHttpClient, preexisting: Option<&str>) -> Option<String> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("https://api/responses");
        if let Some(bearer) = preexisting {
            builder = builder.header(http::header::AUTHORIZATION, bearer);
        }
        let req = builder.body(Bytes::from("{}")).unwrap();
        let out = client.normalized_request(req).unwrap();
        out.headers()
            .get(http::header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn refreshable_client_overwrites_authorization_with_refreshed_bearer() {
        let refresher: CodexRefreshFn = Arc::new(|| {
            Ok(RefreshedAuth {
                bearer_token: "FRESH".to_string(),
                expires_at_ms: Some(i64::MAX),
            })
        });
        // expiry in the past -> the refresher fires on the first request.
        let client = CodexHttpClient::new_refreshable("STALE".to_string(), Some(0), refresher);

        assert_eq!(
            authorization(&client, Some("Bearer STALE")).as_deref(),
            Some("Bearer FRESH")
        );
    }

    #[test]
    fn refreshable_client_keeps_fresh_bearer_without_refreshing() {
        let refresher: CodexRefreshFn = Arc::new(|| panic!("must not refresh a fresh token"));
        let client =
            CodexHttpClient::new_refreshable("CURRENT".to_string(), Some(i64::MAX), refresher);

        assert_eq!(
            authorization(&client, Some("Bearer CURRENT")).as_deref(),
            Some("Bearer CURRENT")
        );
    }

    #[test]
    fn refresh_failure_falls_back_to_the_stale_bearer() {
        let refresher: CodexRefreshFn = Arc::new(|| anyhow::bail!("network down"));
        let client = CodexHttpClient::new_refreshable("STALE".to_string(), Some(0), refresher);

        // Fail-open: the request still carries the old bearer (no regression vs
        // the frozen-header behavior) rather than dropping Authorization.
        assert_eq!(
            authorization(&client, Some("Bearer STALE")).as_deref(),
            Some("Bearer STALE")
        );
    }

    #[test]
    fn default_client_leaves_authorization_untouched() {
        let client = CodexHttpClient::default();

        assert_eq!(
            authorization(&client, Some("Bearer PREEXISTING")).as_deref(),
            Some("Bearer PREEXISTING")
        );
    }

    #[test]
    fn merges_multiple_system_messages_into_instructions() {
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

    #[test]
    fn injects_responses_instructions_from_system_input() {
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

    #[test]
    fn preserves_existing_instructions_but_still_strips_system_input() {
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

    #[test]
    fn overrides_true_store_for_codex_backend() {
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
}
