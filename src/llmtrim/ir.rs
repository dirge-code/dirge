//! Neutral request/response model.
//!
//! JSON is the source of truth: a [`Request`] wraps the original provider body as
//! a [`serde_json::Value`], and stage mutations are applied at JSON-pointer
//! addresses. Any field a stage does not touch is reproduced byte-for-byte on
//! serialization — lossless passthrough by construction, which is stronger than a
//! typed struct with a `#[serde(flatten)]` catch-all (no field can be silently
//! re-typed or dropped).

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The closed LLM provider a request targets. llmtrim is API-only and
/// provider-agnostic; each kind has its own wire shape and tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Google,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Google => "google",
        }
    }
}

impl FromStr for ProviderKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" | "oai" | "gpt" => Ok(ProviderKind::OpenAi),
            "anthropic" | "claude" => Ok(ProviderKind::Anthropic),
            "google" | "gemini" | "googleai" => Ok(ProviderKind::Google),
            other => bail!("unknown provider '{other}' (expected openai|anthropic|google)"),
        }
    }
}

/// A provider request body. `raw` is the source of truth for serialization.
#[derive(Debug, Clone)]
pub struct Request {
    kind: ProviderKind,
    raw: Value,
    /// Out-of-band model id for providers that don't carry it in the body (Gemini puts the
    /// model in the URL path). Never serialized — `to_json_string` emits only `raw`.
    model_hint: Option<String>,
    /// Whether the cache stage (Stage A) will run for this request, i.e. `config.cache`
    /// after `auto` preset routing has resolved. Never serialized.
    ///
    /// The cache zone needs this to decide whether a routed-Anthropic request is *going*
    /// to get a breakpoint: Stage A runs last (it fingerprints the final prefix), so the
    /// content stages ask before the marker exists and have to predict it. Predicting from
    /// the model id alone is wrong when the stage is switched off — the freeze would then
    /// protect a cached prefix that never gets created (dirge-01tu).
    ///
    /// Defaults to `false`, which errs toward compressing: a caller that forgets to set it
    /// keeps the pre-freeze behavior rather than silently disabling compression.
    cache_stage_enabled: bool,
}

impl Request {
    /// Parse a request body string.
    pub fn parse(kind: ProviderKind, body: &str) -> Result<Self> {
        let raw: Value = serde_json::from_str(body).context("request body is not valid JSON")?;
        Ok(Self {
            kind,
            raw,
            model_hint: None,
            cache_stage_enabled: false,
        })
    }

    /// Build a request from an already-parsed value (avoids a re-parse).
    pub fn from_value(kind: ProviderKind, raw: Value) -> Self {
        Self {
            kind,
            raw,
            model_hint: None,
            cache_stage_enabled: false,
        }
    }

    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Record an out-of-band model id (e.g. Gemini's, which lives in the URL path, not the
    /// body). Never affects serialization; only [`Request::model_id`] reads it.
    pub fn set_model_hint(&mut self, model: Option<&str>) {
        self.model_hint = model.map(str::to_string);
    }

    /// Record whether the cache stage will run for this request (`config.cache`, after
    /// `auto` routing resolves). Never affects serialization; only the cache zone reads it.
    pub fn set_cache_stage_enabled(&mut self, enabled: bool) {
        self.cache_stage_enabled = enabled;
    }

    /// Whether the cache stage will run — i.e. whether a routed-Anthropic request is going
    /// to receive a breakpoint the content stages must not rewrite around.
    pub fn cache_stage_enabled(&self) -> bool {
        self.cache_stage_enabled
    }

    /// The request's model id: the body's `model` field if present, else the out-of-band hint.
    /// `None` when neither is set.
    pub fn model_id(&self) -> Option<&str> {
        self.raw
            .get("model")
            .and_then(Value::as_str)
            .or(self.model_hint.as_deref())
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub fn raw_mut(&mut self) -> &mut Value {
        &mut self.raw
    }

    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string(&self.raw).context("failed to serialize request")
    }

    /// Read a string at a JSON pointer (RFC 6901), e.g. `/messages/0/content`.
    pub fn get_str(&self, pointer: &str) -> Option<&str> {
        self.raw.pointer(pointer).and_then(Value::as_str)
    }

    /// Replace the value at a JSON pointer. Returns `false` if the pointer is absent.
    pub fn set(&mut self, pointer: &str, value: Value) -> bool {
        match self.raw.pointer_mut(pointer) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }
}

/// A provider response body (source of truth = `raw`), used for rehydration.
#[derive(Debug, Clone)]
pub struct Response {
    kind: ProviderKind,
    raw: Value,
}

impl Response {
    pub fn parse(kind: ProviderKind, body: &str) -> Result<Self> {
        let raw: Value = serde_json::from_str(body).context("response body is not valid JSON")?;
        Ok(Self { kind, raw })
    }

    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub fn raw_mut(&mut self) -> &mut Value {
        &mut self.raw
    }

    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string(&self.raw).context("failed to serialize response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_parses_aliases() {
        assert_eq!(
            "openai".parse::<ProviderKind>().unwrap(),
            ProviderKind::OpenAi
        );
        assert_eq!(
            "Claude".parse::<ProviderKind>().unwrap(),
            ProviderKind::Anthropic
        );
        assert!("bogus".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn request_round_trip_is_lossless() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":7,"unknown_field":{"keep":[1,2,3]}}"#;
        let req = Request::parse(ProviderKind::OpenAi, body).unwrap();
        let out = req.to_json_string().unwrap();
        let a: Value = serde_json::from_str(body).unwrap();
        let b: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(a, b, "unknown fields must survive round-trip");
    }

    #[test]
    fn model_id_prefers_body_then_hint_and_never_serializes() {
        // Body model wins when present (OpenAI/Anthropic).
        let mut req = Request::parse(ProviderKind::OpenAi, r#"{"model":"gpt-4o"}"#).unwrap();
        assert_eq!(req.model_id(), Some("gpt-4o"));
        req.set_model_hint(Some("ignored"));
        assert_eq!(
            req.model_id(),
            Some("gpt-4o"),
            "body model outranks the hint"
        );

        // No body model (Gemini): the out-of-band hint is used, and never serialized.
        let mut g = Request::parse(ProviderKind::Google, r#"{"contents":[]}"#).unwrap();
        assert_eq!(g.model_id(), None);
        g.set_model_hint(Some("gemini-3-pro"));
        assert_eq!(g.model_id(), Some("gemini-3-pro"));
        assert_eq!(
            g.to_json_string().unwrap(),
            r#"{"contents":[]}"#,
            "the hint must not leak into the forwarded body"
        );
    }

    #[test]
    fn pointer_get_and_set() {
        let mut req = Request::parse(
            ProviderKind::OpenAi,
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(req.get_str("/messages/0/content"), Some("hi"));
        assert!(req.set("/messages/0/content", Value::String("bye".into())));
        assert_eq!(req.get_str("/messages/0/content"), Some("bye"));
        assert!(!req.set("/nope/0", Value::Null));
    }
}
