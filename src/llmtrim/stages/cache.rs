//! Stage A — cache discipline (provider prefix caching). Lossless / opt-in.
//!
//! The #2 lever: mark the invariant prefix (system prompt + tool schemas)
//! with provider cache breakpoints so the prefix is billed once and reused across
//! calls. On Anthropic this places `cache_control: {ephemeral}` (≤4 breakpoints); on
//! OpenAI it's a no-op (the longest matching prefix is cached automatically).
//!
//! Lossless — adds caching hints, never changes content — so it uses the
//! `Structural` gate (always applied; the discount is latent, realized on a later
//! call, not in per-call input tokens). Off by default: Anthropic cache *writes*
//! cost ~25% more, so it only pays off when the prefix is read again (multi-turn or
//! templated/structural reuse). Runs last so it fingerprints the final prefix.

use anyhow::Result;
use serde_json::{Value, json};

use crate::llmtrim::gate::{GateKind, PlanEntry, Transform};
use crate::llmtrim::ir::Request;
use crate::llmtrim::provider::Provider;
use crate::llmtrim::stages::tools::fnv1a;

pub struct CacheStage {
    /// Maximum cache breakpoints to place (Anthropic allows up to 4).
    pub max_breakpoints: usize,
    /// Inject a `prompt_cache_key` — see [`DenseConfig::cache_prompt_key`], which explains
    /// why this is not safe to send everywhere.
    ///
    /// [`DenseConfig::cache_prompt_key`]: crate::llmtrim::config::DenseConfig::cache_prompt_key
    pub prompt_key: bool,
    /// TTL for an automatic breakpoint: `"1h"`, or empty for the API default of 5 minutes.
    pub auto_ttl: String,
}

impl Transform for CacheStage {
    fn name(&self) -> &str {
        "cache"
    }

    fn gate_kind(&self) -> GateKind {
        GateKind::Structural
    }

    fn scope(&self) -> crate::llmtrim::gate::Scope {
        // Adds `cache_control` metadata (to tool/system blocks); content TEXT is unchanged.
        crate::llmtrim::gate::Scope::Tools
    }

    fn apply(
        &self,
        req: &mut Request,
        provider: &dyn Provider,
        _plan: &mut Vec<PlanEntry>,
    ) -> Result<()> {
        // Stabilize the prefix so it's byte-identical across SDK restarts (raises the
        // provider's cache-hit rate). Skip when the client placed its own breakpoints —
        // reordering tools or injecting a key would bust the cache it set up.
        if !has_client_breakpoint(req.raw()) {
            sort_tools(req);
            if self.prompt_key {
                let key = format!("{:016x}", cache_prefix_hash(req));
                provider.set_prompt_cache_key(req, &key);
            }
            set_router_cache_breakpoint(req, &self.auto_ttl);
        }
        provider.set_cache_breakpoints(req, self.max_breakpoints);
        Ok(())
    }
}

/// What a routed model needs from us to cache anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouterCache {
    /// Top-level `cache_control`: the upstream picks the breakpoint and advances it as the
    /// conversation grows. Anthropic only; it is also the only family that takes a TTL.
    Automatic,
    /// A `cache_control` on the last block of the system message. The upstream accepts
    /// per-block breakpoints but not the top-level form, so the stable instruction prefix
    /// is all we can pin.
    SystemBlock,
    /// Nothing to do: the upstream caches the longest matching prefix by itself, and a
    /// marker would be inert at best.
    Implicit,
}

/// Ask a router to cache the stable prefix.
///
/// A router (OpenRouter, and the gateways that copy its `vendor/model` ids) speaks the
/// OpenAI wire shape but forwards to whatever provider owns the model, so caching behaviour
/// is the upstream's and differs per route. OpenAI, DeepSeek, GLM and Cerebras cache the
/// longest matching prefix by themselves. Anthropic, Alibaba Qwen and Gemini cache nothing
/// without an explicit breakpoint, so those routes re-bill the whole system prompt every
/// turn — the hole dirge-607 closed on the direct Anthropic path, still open here because
/// the OpenAI shape has nowhere to put a per-block marker.
///
/// Where the marker goes is per family and not interchangeable: only Anthropic accepts the
/// top-level form, and only Anthropic accepts a 1h TTL. Qwen and Gemini take per-block
/// breakpoints, so they get one on the system message.
///
/// Everything is gated on a vendor prefix. Unprefixed ids mean the provider was addressed
/// directly, where a `cache_control` is an unrecognized argument and a 400 rather than
/// something ignored. The cache-zone freeze shares the Anthropic half of this decision
/// ([`crate::llmtrim::cache_zone::routes_to_anthropic`]) so the two agree about which
/// requests carry a cached prefix that must not be rewritten; the `SystemBlock` routes need
/// no such agreement, because instructions are never compressible either way.
///
/// Runs after `set_prompt_cache_key`, and only when the client set no breakpoints of its
/// own: the routing pin has to survive (a router falls back to `prompt_cache_key` to keep a
/// conversation on the endpoint holding its cache), and our marker must never land beside
/// someone else's, which risks the ttl-ordering violation the API rejects.
fn set_router_cache_breakpoint(req: &mut Request, auto_ttl: &str) {
    // Classify first so the borrow of `raw` ends before the mutation below.
    let kind = match crate::llmtrim::cache_zone::router_model(req.raw()) {
        Some((vendor, model)) => classify_route(vendor, model),
        None => return,
    };
    match kind {
        RouterCache::Automatic => set_automatic_marker(req, auto_ttl),
        RouterCache::SystemBlock => mark_system_message(req),
        RouterCache::Implicit => {}
    }
}

/// Map a router `vendor/model` id to the caching mechanism its upstream needs.
fn classify_route(vendor: &str, model: &str) -> RouterCache {
    if vendor.eq_ignore_ascii_case("anthropic") {
        return RouterCache::Automatic;
    }
    // Alibaba's caching covers the qwen3 line; Google's covers Gemini but not the open
    // Gemma weights, which have no cache to address. An unsupported model in either family
    // just leaves the marker unused.
    let gemini = vendor.eq_ignore_ascii_case("google")
        && model
            .get(..6)
            .is_some_and(|m| m.eq_ignore_ascii_case("gemini"));
    if vendor.eq_ignore_ascii_case("qwen") || gemini {
        return RouterCache::SystemBlock;
    }
    RouterCache::Implicit
}

/// Place the top-level automatic marker, honoring the configured TTL. An empty `ttl` means
/// the API default (5 minutes), which is expressed by omitting the field rather than by
/// sending `"5m"`.
fn set_automatic_marker(req: &mut Request, ttl: &str) {
    let marker = if ttl.is_empty() {
        json!({"type": "ephemeral"})
    } else {
        json!({"type": "ephemeral", "ttl": ttl})
    };
    if let Some(obj) = req.raw_mut().as_object_mut() {
        obj.entry("cache_control").or_insert(marker);
    }
}

/// Mark the end of the system message as a cache breakpoint, in the OpenAI chat shape.
///
/// A string `content` is lifted to a one-element block array, since `cache_control` is a
/// per-block field. No TTL: 1h is Anthropic-only, and these routes reject it.
fn mark_system_message(req: &mut Request) {
    let Some(messages) = req
        .raw_mut()
        .get_mut("messages")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let Some(system) = messages
        .iter_mut()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    else {
        return;
    };
    match system.get_mut("content") {
        Some(Value::String(text)) => {
            let text = std::mem::take(text);
            if let Some(obj) = system.as_object_mut() {
                obj.insert(
                    "content".to_string(),
                    json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": {"type": "ephemeral"},
                    }]),
                );
            }
        }
        Some(Value::Array(blocks)) => {
            if let Some(last) = blocks.last_mut()
                && let Some(obj) = last.as_object_mut()
            {
                obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            }
        }
        _ => {}
    }
}

/// Whether the client placed its own cache breakpoint, i.e. a `cache_control` on a block
/// inside `system`, `messages`, or `tools`. A `cache_control` at the top level of the body
/// is Anthropic's automatic-caching marker (dirge-607): the API chooses the breakpoint
/// itself, so it pins no block we could reorder. Canonicalizing the prefix still matters
/// there — more so, since a 1h entry that a churning tool order never matches is pure
/// cache-write cost.
fn has_client_breakpoint(raw: &Value) -> bool {
    ["system", "messages", "tools"]
        .iter()
        .filter_map(|key| raw.get(*key))
        .any(crate::llmtrim::cache_zone::has_cache_control)
}

/// Canonicalize `tools[]`: recursively sort every JSON-object key (schemas included), then
/// sort the tools by name. Object key order and tool order are semantically irrelevant, so
/// this is lossless — but it makes the prefix deterministic across SDKs that emit tools in
/// hash-randomized order, which otherwise bust the provider cache on every restart.
fn sort_tools(req: &mut Request) {
    let Some(Value::Array(tools)) = req.raw_mut().get_mut("tools") else {
        return;
    };
    for tool in tools.iter_mut() {
        sort_keys(tool);
        sort_function_declarations(tool);
    }
    tools.sort_by(|a, b| tool_name(a).cmp(tool_name(b)));
}

/// Order Gemini's nested declarations by name.
///
/// Gemini wraps the tools one level deeper than everyone else —
/// `tools: [{ functionDeclarations: [...] }]` — so the outer sort above sees a single
/// element with no name to sort on, and `sort_keys` only orders object keys, never array
/// elements. Without this the declaration order is whatever the SDK emitted, and Gemini's
/// implicit cache is pure prefix matching: a tool block that reshuffles between runs never
/// matches the earlier prefix, which is the one thing we can do for that provider.
fn sort_function_declarations(tool: &mut Value) {
    // rig serializes camelCase; accept the snake_case spelling too so a hand-built or
    // proxied body gets the same treatment.
    for key in ["functionDeclarations", "function_declarations"] {
        if let Some(Value::Array(decls)) = tool.get_mut(key) {
            decls.sort_by(|a, b| tool_name(a).cmp(tool_name(b)));
        }
    }
}

/// Tool name across wire shapes: Anthropic top-level `name`, OpenAI `function.name`.
fn tool_name(tool: &Value) -> &str {
    tool.get("name")
        .or_else(|| tool.get("function").and_then(|f| f.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Recursively sort object keys in place (relies on serde_json's `preserve_order`).
fn sort_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for child in map.values_mut() {
                sort_keys(child);
            }
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, val) in entries {
                map.insert(k, val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

/// Fingerprint of the cacheable prefix (system + tool schemas). Two requests with a
/// byte-identical prefix share this hash → eligible for the provider prefix cache,
/// including across independent single-turn calls (structural reuse, *UniCache*).
/// Non-cryptographic (used only for equality of the prefix).
pub fn cache_prefix_hash(req: &Request) -> u64 {
    let raw = req.raw();
    let mut buf = String::new();
    let mut hashed_anything = false;

    // Anthropic-style: top-level `system` + `tools`.
    if let Some(sys) = raw.get("system") {
        buf.push_str(&sys.to_string());
        buf.push('\u{1f}'); // unit separator keeps adjacent fields distinct
        hashed_anything = true;
    }
    if let Some(tools) = raw.get("tools") {
        buf.push_str(&tools.to_string());
        buf.push('\u{1f}');
        hashed_anything = true;
    }

    // OpenAI-style: the leading run of system-role messages.
    if !hashed_anything && let Some(msgs) = raw.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if m.get("role").and_then(Value::as_str) == Some("system") {
                buf.push_str(&m.to_string());
                buf.push('\u{1f}');
            } else {
                break;
            }
        }
    }
    fnv1a(buf.bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::ir::ProviderKind;
    use crate::llmtrim::provider::{AnthropicProvider, GoogleProvider, OpenAiProvider};
    use serde_json::json;

    fn anthropic(body: Value) -> Request {
        Request::from_value(ProviderKind::Anthropic, body)
    }

    #[test]
    fn anthropic_caches_system_string_as_block() {
        let mut req = anthropic(json!({"system":"you are helpful","max_tokens":1,"messages":[]}));
        AnthropicProvider.set_cache_breakpoints(&mut req, 4);
        let sys = req.raw().get("system").unwrap();
        assert_eq!(
            sys.pointer("/0/cache_control/type").and_then(Value::as_str),
            Some("ephemeral"),
            "string system becomes a cached text block"
        );
        assert_eq!(
            sys.pointer("/0/text").and_then(Value::as_str),
            Some("you are helpful")
        );
    }

    #[test]
    fn anthropic_caches_last_tool() {
        let mut req = anthropic(json!({
            "max_tokens":1, "messages":[],
            "tools":[{"name":"a","input_schema":{}},{"name":"b","input_schema":{}}]
        }));
        AnthropicProvider.set_cache_breakpoints(&mut req, 4);
        assert_eq!(
            req.raw()
                .pointer("/tools/1/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
        assert!(req.raw().pointer("/tools/0/cache_control").is_none());
    }

    #[test]
    fn respects_max_breakpoints() {
        let mut req = anthropic(json!({
            "system":"sys","max_tokens":1,"messages":[],
            "tools":[{"name":"a","input_schema":{}}]
        }));
        AnthropicProvider.set_cache_breakpoints(&mut req, 1);
        // Only one breakpoint: the tool is marked, the system is left untouched.
        assert!(req.raw().pointer("/tools/0/cache_control").is_some());
        assert!(
            req.raw().get("system").unwrap().is_string(),
            "system not converted (budget spent)"
        );
    }

    #[test]
    fn openai_is_noop() {
        let body =
            json!({"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]});
        let mut req = Request::from_value(ProviderKind::OpenAi, body.clone());
        OpenAiProvider.set_cache_breakpoints(&mut req, 4);
        assert_eq!(
            req.raw(),
            &body,
            "OpenAI request is unchanged (automatic caching)"
        );
    }

    fn run_cache_stage(req: &mut Request, provider: &dyn Provider) {
        run_cache_stage_with(req, provider, true, "1h");
    }

    fn run_cache_stage_with(
        req: &mut Request,
        provider: &dyn Provider,
        prompt_key: bool,
        auto_ttl: &str,
    ) {
        let mut plan: Vec<PlanEntry> = Vec::new();
        CacheStage {
            max_breakpoints: 4,
            prompt_key,
            auto_ttl: auto_ttl.to_string(),
        }
        .apply(req, provider, &mut plan)
        .unwrap();
    }

    #[test]
    fn stabilize_sorts_tools_and_schema_keys() {
        let mut req = Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"type": "function", "function": {"name": "zebra", "parameters": {"b": 1, "a": 2}}},
                    {"type": "function", "function": {"name": "apple", "parameters": {}}},
                ]
            }),
        );
        run_cache_stage(&mut req, &OpenAiProvider);
        let tools = req.raw().get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(
            tools[0].pointer("/function/name").unwrap(),
            "apple",
            "tools sorted by name"
        );
        assert_eq!(tools[1].pointer("/function/name").unwrap(), "zebra");
        let keys: Vec<&str> = tools[1]
            .pointer("/function/parameters")
            .and_then(Value::as_object)
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["a", "b"], "schema keys canonicalized");
    }

    /// An OpenAI-shaped body as a router receives it.
    fn routed(model: &str) -> Request {
        Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": "a long stable system prompt"},
                    {"role": "user", "content": "hi"},
                ],
                "tools": [{"type": "function", "function": {"name": "read", "parameters": {}}}]
            }),
        )
    }

    #[test]
    fn routed_anthropic_model_gets_automatic_caching() {
        let mut req = routed("anthropic/claude-sonnet-4.5");
        run_cache_stage(&mut req, &OpenAiProvider);
        assert_eq!(
            req.raw().pointer("/cache_control/ttl").unwrap(),
            "1h",
            "router asked to cache the Anthropic route"
        );
        // The routing pin has to survive: it is what keeps the conversation on the
        // endpoint holding the cache we just paid to write.
        assert!(
            req.raw().get("prompt_cache_key").is_some(),
            "prompt_cache_key still set"
        );
    }

    #[test]
    fn automatic_marker_honors_the_configured_ttl() {
        // dirge-cbgz. 5m is the API default, expressed by omitting `ttl` rather than by
        // sending "5m" — which the API would reject.
        let mut req = routed("anthropic/claude-sonnet-4.5");
        run_cache_stage_with(&mut req, &OpenAiProvider, true, "");
        assert_eq!(
            req.raw().get("cache_control").unwrap(),
            &json!({"type": "ephemeral"}),
            "no ttl field for the 5m default"
        );

        let mut req = routed("anthropic/claude-sonnet-4.5");
        run_cache_stage_with(&mut req, &OpenAiProvider, true, "1h");
        assert_eq!(
            req.raw().pointer("/cache_control/ttl").unwrap(),
            "1h",
            "1h is sent explicitly"
        );
    }

    #[test]
    fn prompt_cache_key_is_suppressed_for_strict_backends() {
        // dirge-07ew: an OpenAI-compatible endpoint that validates its body rejects the
        // whole request over an unknown field rather than ignoring it, so the caller gets
        // to switch this off per backend.
        let mut req = routed("gpt-5.2");
        run_cache_stage_with(&mut req, &OpenAiProvider, false, "1h");
        assert!(
            req.raw().get("prompt_cache_key").is_none(),
            "no key when the backend does not accept one"
        );
        // Canonicalization is unaffected: it is what makes the prefix match at all.
        assert_eq!(
            req.raw().pointer("/tools/0/function/name").unwrap(),
            "read",
            "tools still canonicalized"
        );
    }

    #[test]
    fn qwen_and_gemini_routes_get_a_system_block_breakpoint() {
        // dirge-gcxb: those upstreams take per-block breakpoints but not the top-level
        // form, so the marker goes on the end of the system message.
        for model in ["qwen/qwen3-coder-plus", "google/gemini-3-pro"] {
            let mut req = routed(model);
            run_cache_stage(&mut req, &OpenAiProvider);
            assert!(
                req.raw().get("cache_control").is_none(),
                "{model} must not get the top-level form"
            );
            assert_eq!(
                req.raw()
                    .pointer("/messages/0/content/0/cache_control/type")
                    .unwrap(),
                "ephemeral",
                "{model} gets a system-block breakpoint"
            );
            // A string `content` is lifted to a block array, and the text survives intact.
            assert_eq!(
                req.raw().pointer("/messages/0/content/0/text").unwrap(),
                "a long stable system prompt",
                "{model} keeps its system text"
            );
            // No ttl: 1h is Anthropic-only and these routes reject it.
            assert!(
                req.raw()
                    .pointer("/messages/0/content/0/cache_control/ttl")
                    .is_none(),
                "{model} must not carry a ttl"
            );
        }
    }

    #[test]
    fn system_block_breakpoint_marks_the_last_block_of_an_array() {
        let mut req = Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": "google/gemini-3-pro",
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "preamble"},
                    {"type": "text", "text": "the bulk"},
                ]}]
            }),
        );
        run_cache_stage(&mut req, &OpenAiProvider);
        assert!(
            req.raw()
                .pointer("/messages/0/content/0/cache_control")
                .is_none(),
            "earlier blocks stay unmarked"
        );
        assert_eq!(
            req.raw()
                .pointer("/messages/0/content/1/cache_control/type")
                .unwrap(),
            "ephemeral",
            "the breakpoint ends the prefix"
        );
    }

    #[test]
    fn gemma_and_other_uncached_families_are_left_alone() {
        // Google's caching covers Gemini, not the open Gemma weights.
        for model in ["google/gemma-3-27b", "meta-llama/llama-4", "mistral/large"] {
            let mut req = routed(model);
            run_cache_stage(&mut req, &OpenAiProvider);
            assert!(req.raw().get("cache_control").is_none(), "{model}");
            assert!(
                req.raw()
                    .pointer("/messages/0/content/0/cache_control")
                    .is_none(),
                "{model} keeps its plain string system content"
            );
        }
    }

    #[test]
    fn routed_variants_and_price_prefixes_are_recognized() {
        for model in [
            "~anthropic/claude-sonnet-latest",
            "anthropic/claude-opus-4.5:nitro",
            "Anthropic/claude-haiku-4.5",
        ] {
            let mut req = routed(model);
            run_cache_stage(&mut req, &OpenAiProvider);
            assert!(
                req.raw().get("cache_control").is_some(),
                "{model} is an Anthropic route"
            );
        }
    }

    #[test]
    fn implicitly_cached_routes_get_no_marker() {
        // A top-level `cache_control` is an unrecognized argument on a real OpenAI-shaped
        // endpoint (a 400, not an ignore), and these providers cache the prefix themselves.
        for model in [
            "openai/gpt-5.2",
            "deepseek/deepseek-chat",
            "google/gemini-3-pro",
            "qwen/qwen3-coder-plus",
            "openrouter/auto",
            "gpt-5.2",
            "claude-sonnet-4-5",
        ] {
            let mut req = routed(model);
            run_cache_stage(&mut req, &OpenAiProvider);
            assert!(
                req.raw().get("cache_control").is_none(),
                "{model} must not get a top-level marker"
            );
        }
    }

    #[test]
    fn routed_marker_defers_to_a_client_breakpoint() {
        // A caller that placed its own breakpoint owns the policy; adding a 1h marker
        // beside a 5m block marker is the ttl-ordering violation the API rejects.
        let mut req = Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": "anthropic/claude-sonnet-4.5",
                "messages": [{"role": "system", "content": [
                    {"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}
                ]}]
            }),
        );
        run_cache_stage(&mut req, &OpenAiProvider);
        assert!(req.raw().get("cache_control").is_none());
    }

    #[test]
    fn stabilize_sorts_gemini_function_declarations() {
        // Gemini's tools sit one level down, so the outer sort can't see them. Its
        // implicit cache is pure prefix matching, so a declaration block that reshuffles
        // between runs never matches — canonicalizing it is the only lever we have there.
        let mut req = Request::from_value(
            ProviderKind::Google,
            json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "tools": [{"functionDeclarations": [
                    {"name": "zebra", "parameters": {"b": 1, "a": 2}},
                    {"name": "apple", "parameters": {}},
                ]}]
            }),
        );
        run_cache_stage(&mut req, &GoogleProvider);
        assert_eq!(
            req.raw()
                .pointer("/tools/0/functionDeclarations/0/name")
                .unwrap(),
            "apple",
            "declarations sorted by name"
        );
        let keys: Vec<&str> = req
            .raw()
            .pointer("/tools/0/functionDeclarations/1/parameters")
            .and_then(Value::as_object)
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["a", "b"], "schema keys canonicalized");
    }

    #[test]
    fn openai_gets_a_stable_prompt_cache_key() {
        let mut req = Request::from_value(
            ProviderKind::OpenAi,
            json!({"model": "gpt-4o", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}]}),
        );
        run_cache_stage(&mut req, &OpenAiProvider);
        assert!(
            req.raw()
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .is_some(),
            "prompt_cache_key injected for OpenAI"
        );
    }

    #[test]
    fn stabilize_defers_to_client_managed_caching() {
        // A client `cache_control` marker means it manages its own cache → we must not
        // reorder tools (that would bust it).
        let mut req = anthropic(json!({
            "max_tokens": 1, "messages": [],
            "tools": [
                {"name": "zebra", "input_schema": {}, "cache_control": {"type": "ephemeral"}},
                {"name": "apple", "input_schema": {}},
            ]
        }));
        run_cache_stage(&mut req, &AnthropicProvider);
        assert_eq!(
            req.raw().pointer("/tools/0/name").unwrap(),
            "zebra",
            "tool order preserved when the client manages caching"
        );
    }

    #[test]
    fn top_level_automatic_caching_still_stabilizes_tools() {
        // dirge-607: rig's `with_automatic_caching_1h()` puts a `cache_control` at the
        // top level of the body. That is not a client-placed breakpoint on any block, so
        // it must not switch off tool canonicalization — the 1h prefix cache depends on
        // the tool block being byte-identical across restarts.
        let mut req = anthropic(json!({
            "max_tokens": 1, "messages": [],
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "tools": [
                {"name": "zebra", "input_schema": {}},
                {"name": "apple", "input_schema": {}},
            ]
        }));
        run_cache_stage(&mut req, &AnthropicProvider);
        assert_eq!(
            req.raw().pointer("/tools/0/name").unwrap(),
            "apple",
            "tools canonicalized despite the top-level automatic-caching marker"
        );
        // The breakpoints themselves stay off: mixing our default-ttl markers with the
        // top-level 1h marker is exactly the ttl-ordering violation the API rejects.
        assert!(
            req.raw().pointer("/tools/1/cache_control").is_none(),
            "no 5m breakpoints added alongside the top-level 1h marker"
        );
    }

    #[test]
    fn prefix_hash_is_stable_and_distinct() {
        let a = anthropic(json!({"system":"SAME","messages":[{"role":"user","content":"q1"}]}));
        let b = anthropic(
            json!({"system":"SAME","messages":[{"role":"user","content":"q2 different"}]}),
        );
        let c = anthropic(json!({"system":"OTHER","messages":[{"role":"user","content":"q1"}]}));
        // Same prefix (system) → same hash even with different turns (structural reuse).
        assert_eq!(cache_prefix_hash(&a), cache_prefix_hash(&b));
        // Different prefix → different hash.
        assert_ne!(cache_prefix_hash(&a), cache_prefix_hash(&c));
    }
}
