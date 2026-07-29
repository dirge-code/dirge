//! Cache-zone discipline — never recompress the provider's frozen (cached) prefix.
//!
//! When a request carries `cache_control` markers (Claude Code sets these on the stable
//! prefix), the provider caches everything up to the last marker and bills it at ~0.1×.
//! Rewriting that content — even to save tokens — changes the cached bytes and busts the
//! cache, which usually costs *more* than the tokens saved (the "input compression is a
//! false economy" trap). So the content-mutating stages compress only the **live zone**:
//! the segments after the last `cache_control` marker. Each new tool result is therefore
//! compressed exactly once — when it first arrives in the live zone — then frozen.
//!
//! Anthropic's *automatic* caching (dirge-607) puts a single `cache_control` at the TOP
//! LEVEL of the body instead of on a block: the API picks the breakpoint itself, placing it
//! on the last cacheable block and advancing it as the conversation grows. There is no
//! marker to read the position off, but the position is implied — everything sent on an
//! earlier turn is inside the cached prefix. So a top-level marker freezes the system
//! prompt and every message except the newest, which is the one that hasn't been cached
//! yet and therefore is still ours to compress (exactly once, as above).
//!
//! No markers ⇒ no known cache ⇒ everything is compressible (behavior unchanged):
//! determinism keeps an identical prefix cache-stable across calls, and Stage A's OpenAI
//! `prompt_cache_key` pins auto-cached prefixes.

use std::collections::HashSet;

use serde_json::Value;

use crate::llmtrim::ir::Request;
use crate::llmtrim::provider::Provider;

/// Content-text pointers safe to compress: every content pointer minus those inside the
/// frozen (cached) prefix, and minus the system/developer instructions — which are never
/// compressible, cached or not. The stages iterate this instead of
/// [`Provider::content_text_pointers`]; the token gate still counts *all* content.
pub fn compressible_pointers(req: &Request, provider: &dyn Provider) -> Vec<String> {
    let frozen = frozen_pointers(req, provider);
    provider
        .content_text_pointers(req)
        .into_iter()
        .filter(|p| !frozen.contains(p) && !is_instruction(req, provider, p))
        .collect()
}

/// Does this pointer address the system/developer instructions?
///
/// Instructions are never compressible, cached or not. They are the text the model *conditions
/// on* rather than reads as data, so a fold that is harmless in a tool result can invert a
/// directive: n-gram substitution once rewrote Claude Code's title-prompt few-shot examples from
/// `Good (Korean session): {"title": …}` to `Good (Korean §3 …`, deleting the conditional and
/// leaving "Korean titles are good, English titles are bad" — so every session title came back in
/// Korean. Instructions are also small and near-always inside the provider's cached prefix, so
/// there was never much to win here.
///
/// On real Claude Code traffic this changes nothing (593 of 594 captured requests carry
/// `cache_control` on `system`, so it was already frozen); it closes the gap for the utility
/// calls that don't — title generation, summarisation, and any non-caching client.
fn is_instruction(req: &Request, provider: &dyn Provider, pointer: &str) -> bool {
    // Top-level instruction fields (Anthropic `/system`, Responses `/instructions`, Gemini
    // `/systemInstruction/...`) have no turn index; otherwise ask the provider for the role.
    pointer.starts_with("/system")
        || pointer.starts_with("/instructions")
        || provider.role_at(req, pointer) == Some(crate::llmtrim::provider::Role::System)
}

/// Content-text pointers inside the frozen prefix — everything up to and including the
/// last `cache_control`-marked message, plus a cache-controlled `system`. Empty when the
/// request carries no `cache_control` markers (nothing known-cached to protect).
pub fn frozen_pointers(req: &Request, provider: &dyn Provider) -> HashSet<String> {
    let raw = req.raw();
    let automatic = has_automatic_cache_marker(req);
    let system_frozen = automatic || raw.get("system").is_some_and(has_cache_control);
    let messages = raw.get("messages").and_then(Value::as_array);
    let marked_until = messages.and_then(|msgs| {
        msgs.iter()
            .enumerate()
            .filter(|(_, m)| has_cache_control(m))
            .map(|(i, _)| i)
            .max()
    });
    // Under automatic caching the breakpoint sits at the end of the request the provider
    // last saw, so treat every message but the newest as cached. `len - 2` is the index of
    // the second-to-last message; a 1-message request has no earlier turn to protect.
    let automatic_until = automatic
        .then(|| messages.map(Vec::len).filter(|len| *len >= 2))
        .flatten()
        .map(|len| len - 2);
    // `None` sorts below `Some`, so this takes whichever boundary reaches further.
    let frozen_until = marked_until.max(automatic_until);

    if frozen_until.is_none() && !system_frozen {
        return HashSet::new();
    }
    provider
        .content_text_pointers(req)
        .into_iter()
        .filter(|p| is_frozen(p, frozen_until, system_frozen))
        .collect()
}

/// Whether an automatic cache breakpoint is in play for this request.
///
/// Either the marker is already there — a `cache_control` at the top level of the body
/// rather than on a content block, as distinct from a client-placed breakpoint, which
/// always sits inside `system` / `messages` / `tools` — or Stage A is about to add one for
/// a routed Anthropic model. Stage A runs LAST (it fingerprints the final prefix), so on
/// that path the content stages consulting this would otherwise decide before the marker
/// exists and re-window a prefix the provider has cached.
///
/// The two arms differ in kind. A top-level `cache_control` is EVIDENCE — it is on the
/// body, so a cached prefix exists. The routed-model arm is a PREDICTION that Stage A will
/// add one, which only holds when Stage A is enabled; it is gated on `cache_stage_enabled`
/// accordingly (dirge-01tu). Freezing on the model id alone under a preset that leaves
/// Stage A off protected a prefix that never got cached, so the history was neither
/// compressed nor cached — strictly worse than not freezing.
fn has_automatic_cache_marker(req: &Request) -> bool {
    let raw = req.raw();
    // An explicit top-level marker is self-evidencing: it is already on the body, so the
    // upstream really does hold a cached prefix, whatever the config says.
    if raw.get("cache_control").is_some_and(|c| !c.is_null()) {
        return true;
    }
    // The routed case is a PREDICTION that Stage A will add the marker later, so it only
    // holds when Stage A is actually going to run (dirge-01tu). Anthropic caches nothing
    // without an explicit breakpoint, so freezing with the stage off would protect a
    // prefix that never gets cached — leaving the content neither compressed nor cached.
    req.cache_stage_enabled() && routes_to_anthropic(raw)
}

/// Split a router-style `vendor/model` id into its two halves, e.g.
/// `("anthropic", "claude-sonnet-4.5")`. `None` when the model is not vendor-prefixed,
/// which is how a router (OpenRouter and the gateways that copy its ids) is told apart
/// from a provider addressed directly. Ids may carry a `~` price-preference prefix, which
/// is stripped, and a `:variant` suffix, which stays with the model half.
///
/// Direct Anthropic and OpenAI traffic never matches (no vendor prefix), and Gemini
/// carries no `model` field in the body at all.
pub(crate) fn router_model(raw: &Value) -> Option<(&str, &str)> {
    raw.get("model")
        .and_then(Value::as_str)
        .map(|m| m.trim_start_matches(['~', '@']))
        .and_then(|m| m.split_once('/'))
}

/// Whether the request routes to Anthropic through a router, which is the case where the
/// upstream holds a cached prefix we must not rewrite.
pub(crate) fn routes_to_anthropic(raw: &Value) -> bool {
    router_model(raw).is_some_and(|(vendor, _)| vendor.eq_ignore_ascii_case("anthropic"))
}

/// `cache_control` present anywhere within `v` (a block, a message, or nested content).
pub(crate) fn has_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("cache_control") || m.values().any(has_cache_control),
        Value::Array(a) => a.iter().any(has_cache_control),
        _ => false,
    }
}

/// A pointer is frozen if it addresses the cache-controlled `system`, or a message at or
/// before `frozen_until`.
fn is_frozen(ptr: &str, frozen_until: Option<usize>, system_frozen: bool) -> bool {
    if let Some(rest) = ptr.strip_prefix("/system") {
        return system_frozen && (rest.is_empty() || rest.starts_with('/'));
    }
    if let Some(rest) = ptr.strip_prefix("/messages/") {
        let idx = rest
            .split('/')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        return frozen_until.is_some_and(|until| idx <= until);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::ir::ProviderKind;
    use crate::llmtrim::provider::for_kind;
    use serde_json::json;

    fn req(v: Value) -> Request {
        Request::from_value(ProviderKind::Anthropic, v)
    }

    #[test]
    fn instructions_are_never_compressible_even_uncached() {
        // Claude Code's title-generation call carries no `cache_control`, so nothing was frozen
        // and the stages folded n-grams straight through the system prompt's few-shot examples —
        // inverting them, and turning every session title Korean. Instructions are off-limits.
        let r = req(json!({
            "system": [
                {"type": "text", "text": "Good (Korean session): {\"title\": \"결제 모듈 리팩토링\"}"},
            ],
            "messages": [{"role": "user", "content": "summarise this session"}],
        }));
        let p = for_kind(ProviderKind::Anthropic);
        assert!(
            frozen_pointers(&r, p.as_ref()).is_empty(),
            "no cache_control ⇒ nothing frozen"
        );
        let c = compressible_pointers(&r, p.as_ref());
        assert!(
            !c.iter().any(|p| p.starts_with("/system")),
            "system stays out of reach: {c:?}"
        );
        assert!(
            c.iter().any(|p| p.starts_with("/messages")),
            "the session content is still compressible: {c:?}"
        );

        // Same for a string `system`, and for a wire shape that carries instructions as a
        // system-role message rather than a top-level field.
        let r = req(json!({
            "system": "Return JSON with a single \"title\" field.",
            "messages": [
                {"role": "system", "content": "never fold me"},
                {"role": "user", "content": "but fold me"},
            ],
        }));
        let c = compressible_pointers(&r, p.as_ref());
        assert_eq!(c, vec!["/messages/1/content".to_string()], "got {c:?}");
    }

    #[test]
    fn no_markers_means_everything_compressible() {
        let r = req(json!({
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "second turn"},
            ]
        }));
        let p = for_kind(ProviderKind::Anthropic);
        assert!(frozen_pointers(&r, p.as_ref()).is_empty());
        assert_eq!(
            compressible_pointers(&r, p.as_ref()).len(),
            p.content_text_pointers(&r).len(),
            "no cache_control → all content compressible"
        );
    }

    #[test]
    fn cache_control_freezes_the_prefix_through_the_last_marker() {
        // Marker on message 1 → messages 0 and 1 frozen, message 2 (the live turn) free.
        let r = req(json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "cached A"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "cached B", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "live turn"}]},
            ]
        }));
        let p = for_kind(ProviderKind::Anthropic);
        let comp = compressible_pointers(&r, p.as_ref());
        assert!(
            comp.iter().all(|x| x.starts_with("/messages/2")),
            "only the live turn: {comp:?}"
        );
        let frozen = frozen_pointers(&r, p.as_ref());
        assert!(frozen.contains("/messages/0/content/0/text"));
        assert!(frozen.contains("/messages/1/content/0/text"));
    }

    #[test]
    fn cache_controlled_system_is_frozen() {
        let r = req(json!({
            "system": [{"type": "text", "text": "stable instructions", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "ask"}],
        }));
        let p = for_kind(ProviderKind::Anthropic);
        let frozen = frozen_pointers(&r, p.as_ref());
        assert!(
            frozen.contains("/system/0/text"),
            "marked system is frozen: {frozen:?}"
        );
        // The (unmarked) user turn stays compressible.
        assert!(
            compressible_pointers(&r, p.as_ref())
                .iter()
                .any(|x| x.starts_with("/messages/0"))
        );
    }

    #[test]
    fn top_level_automatic_marker_freezes_everything_but_the_newest_turn() {
        // dirge-607: automatic caching marks nothing, so before this the frozen set was
        // empty and the content stages re-windowed tool results the provider had already
        // cached. Re-compressing message 0 or 1 here truncates the cache hit at the first
        // changed block and re-bills the rest at the 1h write rate (2× base input).
        let r = req(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "turn one"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "a 10k-line build log from an earlier turn"}
                ]},
                {"role": "user", "content": [{"type": "text", "text": "the new tool result"}]},
            ]
        }));
        let p = for_kind(ProviderKind::Anthropic);
        let frozen = frozen_pointers(&r, p.as_ref());
        assert!(frozen.contains("/messages/0/content/0/text"));
        assert!(frozen.contains("/messages/1/content/0/text"));
        let comp = compressible_pointers(&r, p.as_ref());
        assert!(
            comp.iter().all(|x| x.starts_with("/messages/2")),
            "only the turn that hasn't been cached yet: {comp:?}"
        );
    }

    #[test]
    fn automatic_marker_leaves_the_opening_request_compressible() {
        // One message means no earlier turn is in the cache, so there is nothing to
        // protect and the usual compression applies. Freezing here would forfeit the
        // saving on the very request that is largest and cheapest to cut.
        let r = req(json!({
            "cache_control": {"type": "ephemeral"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "opening ask"}]}],
        }));
        let p = for_kind(ProviderKind::Anthropic);
        assert!(
            compressible_pointers(&r, p.as_ref())
                .iter()
                .any(|x| x.starts_with("/messages/0")),
            "single-message request stays compressible"
        );
    }

    #[test]
    fn automatic_marker_freezes_the_system_prompt() {
        // The automatic breakpoint always sits at or after the system prompt, so it is
        // cached from the first turn on even though it carries no marker of its own.
        let r = req(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "system": [{"type": "text", "text": "stable instructions"}],
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "user", "content": "two"},
            ],
        }));
        let p = for_kind(ProviderKind::Anthropic);
        assert!(
            frozen_pointers(&r, p.as_ref()).contains("/system/0/text"),
            "system is inside the automatic prefix"
        );
    }

    #[test]
    fn explicit_marker_past_the_automatic_boundary_still_wins() {
        // Both kinds present: the marker on the last message reaches further than
        // "all but the newest", so it sets the boundary.
        let r = req(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "one"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}}
                ]},
            ]
        }));
        let p = for_kind(ProviderKind::Anthropic);
        let frozen = frozen_pointers(&r, p.as_ref());
        assert!(frozen.contains("/messages/1/content/0/text"));
        assert!(
            compressible_pointers(&r, p.as_ref()).is_empty(),
            "the explicit marker covers the whole request"
        );
    }

    fn routed_anthropic_request(cache_stage_enabled: bool) -> Request {
        let mut r = Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": "anthropic/claude-sonnet-4.5",
                "messages": [
                    {"role": "user", "content": "an earlier turn"},
                    {"role": "assistant", "content": "ok"},
                    {"role": "user", "content": "the new tool result"},
                ]
            }),
        );
        r.set_cache_stage_enabled(cache_stage_enabled);
        r
    }

    #[test]
    fn routed_anthropic_model_freezes_before_the_marker_exists() {
        // Stage A adds the marker last, so on a routed Anthropic model the content stages
        // reach this first and would see a bare body. The model id is the signal.
        let r = routed_anthropic_request(true);
        let p = for_kind(ProviderKind::OpenAi);
        let comp = compressible_pointers(&r, p.as_ref());
        assert!(
            comp.iter().all(|x| x.starts_with("/messages/2")),
            "only the newest turn: {comp:?}"
        );
    }

    /// dirge-01tu: the routed-Anthropic freeze must not fire when Stage A is
    /// switched off.
    ///
    /// The freeze exists to protect a prefix the upstream has cached, and on a
    /// routed Anthropic model nothing is cached unless Stage A writes the
    /// breakpoint — Anthropic caches nothing without one. Stage A is gated on
    /// `config.cache`, which is true only in the `agent` / `aggressive` /
    /// `cache` presets and false in the lossless baseline that `safe` / `rag` /
    /// `code` / `reasoning` inherit. Under those presets the pre-fix code froze
    /// the history anyway, so the content was neither compressed (frozen) nor
    /// cached (no marker) — strictly worse than before the freeze existed.
    #[test]
    fn routed_anthropic_does_not_freeze_when_the_cache_stage_is_off() {
        let r = routed_anthropic_request(false);
        let p = for_kind(ProviderKind::OpenAi);
        assert!(
            frozen_pointers(&r, p.as_ref()).is_empty(),
            "no marker will be written, so there is no cached prefix to protect",
        );
        let comp = compressible_pointers(&r, p.as_ref());
        assert!(
            comp.iter().any(|x| x.starts_with("/messages/0")),
            "the earlier turns stay compressible: {comp:?}"
        );
    }

    /// An explicit top-level marker is self-evidencing — its presence proves a
    /// breakpoint exists — so it freezes regardless of the stage flag.
    #[test]
    fn explicit_top_level_marker_freezes_even_with_the_cache_stage_off() {
        let mut r = req(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "earlier"}]},
                {"role": "user", "content": [{"type": "text", "text": "newest"}]},
            ]
        }));
        r.set_cache_stage_enabled(false);
        let p = for_kind(ProviderKind::Anthropic);
        assert!(
            frozen_pointers(&r, p.as_ref()).contains("/messages/0/content/0/text"),
            "an already-present marker means the upstream really has cached this",
        );
    }

    #[test]
    fn implicitly_cached_route_keeps_everything_compressible() {
        let r = Request::from_value(
            ProviderKind::OpenAi,
            json!({
                "model": "openai/gpt-5.2",
                "messages": [
                    {"role": "user", "content": "one"},
                    {"role": "user", "content": "two"},
                ]
            }),
        );
        let p = for_kind(ProviderKind::OpenAi);
        assert!(frozen_pointers(&r, p.as_ref()).is_empty());
    }

    #[test]
    fn null_cache_control_is_not_an_automatic_marker() {
        // rig serializes an unset top-level cache_control as `null` in some shapes;
        // that is the absence of a marker, not a breakpoint.
        let r = req(json!({
            "cache_control": null,
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "user", "content": "two"},
            ]
        }));
        let p = for_kind(ProviderKind::Anthropic);
        assert!(frozen_pointers(&r, p.as_ref()).is_empty());
    }
}
