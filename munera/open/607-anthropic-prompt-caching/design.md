# 607 — Anthropic Prompt Caching

## What

Enable Anthropic prompt caching for the OAuth (Claude Code) path so that
the system prompt + tool definitions are cached server-side across turns.
Without this, every turn bills full input tokens even though the system
prompt and tools are stable — the primary cause of dirge consuming the
Claude Pro 5x plan far faster than psi does on the same workload.

## Why

A dirge session with a long system prompt (~8k tokens) and ~80 tool
definitions sends those tokens on every single turn. Anthropic's prompt
caching can cache that prefix for 5 min (default) or 1 hour, reducing
per-turn input cost by ~80–90% on a typical multi-turn session.

psi implements this via explicit `cache_control: ephemeral` markers on
system blocks and tools (G4 gap in munera/plan.md). Dirge can achieve the
same result more simply via rig's automatic-caching API.

## Root cause (confirmed by code audit)

1. **No caching call**: `rig_stream_fn_from_model_with_filter` calls
   `model.stream(request)` where `model` is a generic `M: CompletionModel`.
   rig's `.with_automatic_caching()` / `.with_prompt_caching()` methods live
   on the concrete `GenericCompletionModel` type, not the trait — so the
   generic stream factory can't call them. The concrete type is only known
   at the `dispatch_stream_fn!` macro arms in `stream_dispatch.rs`.

2. **Missing beta header**: `ANTHROPIC_OAUTH_BETA` is
   `"claude-code-20250219,oauth-2025-04-20"`. psi also sends
   `prompt-caching-scope-2026-01-05` (and `prompt-caching-2024-07-31` when
   manual cache_control markers are present). The scope beta is missing from
   dirge entirely.

## Approach

### Primary fix — automatic caching (path A)

In `stream_dispatch.rs`, the `AnthropicOauth` arm passes the concrete
`CompletionModel<AnthropicHttpClient>` to `__stream_fn`. Before passing,
call `.with_automatic_caching_1h()` on it:

```rust
$enum::AnthropicOauth($bind) => {
    __stream_fn(
        $model.with_automatic_caching_1h(),
        ...
    )
}
```

`with_automatic_caching_1h()` adds a top-level `cache_control:
{"type":"ephemeral","ttl":"1h"}` to the request body. Anthropic's API
automatically places and advances the cache breakpoint — no manual
marker placement, no beta header required for the automatic mode.

Also apply to `AnyModel::Anthropic` (API-key path) for consistency.

### Secondary fix — beta header

Add `prompt-caching-scope-2026-01-05` to `ANTHROPIC_OAUTH_BETA` in
`anthropic_http.rs`. This beta enables per-session cache scope (caches
survive across requests within the same session), which psi uses.

### What NOT to do

- Do NOT use `with_prompt_caching()` (manual breakpoints): requires
  `prompt-caching-2024-07-31` beta and careful marker placement on
  system blocks. More complex, more fragile with the OAuth shaper.
- Do NOT try to inject cache_control via `additional_params`: rig treats
  that as an opaque JSON blob merged into the request, but caching is a
  model-level concern in rig's API.
- Do NOT touch the `shape_oauth_messages_payload` shaper: automatic
  caching adds a top-level field, not per-block markers, so the shaper
  doesn't interfere.

## Acceptance criteria

- [ ] `AnyModel::AnthropicOauth` arm calls `.with_automatic_caching_1h()`
- [ ] `AnyModel::Anthropic` arm calls `.with_automatic_caching_1h()`
- [ ] `prompt-caching-scope-2026-01-05` added to `ANTHROPIC_OAUTH_BETA`
- [ ] Existing tests pass (`cargo test -p dirge`)
- [ ] Wire dump (`DIRGE_DUMP_REQUESTS=1`) shows `cache_control` in request body
- [ ] `/cache` slash command shows non-zero `cache_creation_input_tokens` on turn 2+

## Risk

Low. `with_automatic_caching_1h()` adds one JSON field; the shaper
already handles unknown top-level fields (passes through). The OAuth
classifier is not affected — billing header and identity blocks are
unrelated to caching. Worst case: Anthropic ignores the field and
behaviour is unchanged.

The `Anthropic` (API-key) arm is lower-risk still — no OAuth shaper
involved.
