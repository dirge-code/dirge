use bytes::Bytes;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexHttpClient {
    inner: reqwest::Client,
}

impl CodexHttpClient {
    // Rig 0.37's OpenAI Responses adapter moves `preamble` into the
    // first `input` system message, then serializes `instructions: null`.
    // The ChatGPT Codex backend wants the opposite shape: a non-empty
    // Responses-native `instructions` field, no `system` role in
    // `input`, and `store: false`. Keep the fix inside Dirge by
    // normalizing the outgoing `/responses` JSON body at the
    // transport boundary instead of vendoring or forking rig-core.
    fn normalized_request<T>(req: Request<T>) -> http_client::Result<Request<Bytes>>
    where
        T: Into<Bytes>,
    {
        let (parts, body) = req.into_parts();
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
        let req = Self::normalized_request(req);
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
        let req = Self::normalized_request(req);
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
