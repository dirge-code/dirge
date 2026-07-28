# 607 — Plan

## Approach

Two-file change. No new abstractions, no trait changes, no protocol risk.

1. `src/provider/stream_dispatch.rs` — add `.with_automatic_caching_1h()` to
   the `Anthropic` and `AnthropicOauth` dispatch arms.
2. `src/provider/anthropic_http.rs` — add `prompt-caching-scope-2026-01-05`
   to `ANTHROPIC_OAUTH_BETA`.

## Decisions

- **1h TTL**: dirge sessions routinely exceed 5 min; 1h avoids cache misses
  mid-session. psi uses 5m (default) but that's because psi manages its own
  markers and can be more surgical. Automatic caching with 1h is the right
  default for a CLI tool.
- **Both Anthropic arms**: API-key users also benefit; no downside.
- **Automatic not manual**: avoids touching the OAuth shaper, avoids beta
  header dependency, simpler.
- **scope beta**: low-risk additive; psi ships it, Anthropic ignores unknown
  betas gracefully.

## Risks

- `with_automatic_caching_1h()` is on rig 0.39.0 — verify method exists
  before writing (already confirmed in rig source audit).
- The `dispatch_stream_fn!` macro passes `$model` by move into `__stream_fn`;
  calling `.with_automatic_caching_1h()` on it before the move is fine since
  it consumes and returns `Self`.
