# deepseek-harness (dsh) → dirge adoption plan

Survey comparing `/Users/yogthos/src/deepseek-harness` (dsh, DeepSeek's official
TypeScript agent harness) against dirge. Two tracks: general harness
improvements, and DeepSeek-specific steering. Ends with the per-model config
verdict.

## What dsh is

MIT-licensed cordis-based monorepo. The pieces surveyed:

- `llm-deepseek` — transport-only adapter (fetch + SSE), thinking-mode
  passback, `reasoningEffort` off/high/max, per-model catalog with
  `contextWindow`.
- `system-prompt` — ordered section registry, waterfall assembly event,
  validated `toolOrder`, prompt variables.
- `agent-default-model` — per-agent default `{provider, model, reasoningEffort}`.
- `compaction-tool-result-pruner` — replay-safe deterministic tool-result
  pruning (head/middle/tail, `thresholdChars`).
- `llm-retry` — retry policy owned by each provider config (package config is
  deliberately empty), durable scheduled retries, jitter hook.
- `repeat-tool-reminder` — advisory guard: gentle nudge at 3, detailed at 5/8
  consecutive identical calls (deep-key-sorted canonical args), resets on user
  interjection.
- `timeout-policy` — per-tool declared `timeoutMs`, structured `TOOL_TIMEOUT`
  error the model/retry can route on.
- `persona` — per-agent persona row with explicit KV-cache prefix-stability
  guarantees.

## What dirge already has (no adoption needed)

Most of the DeepSeek wire-specific hardening dsh does, dirge already does —
often more thoroughly:

- **Reasoning effort shapes**: `ReasoningProfile` per provider type
  (`src/provider/adapter.rs`) — top-level `reasoning_effort` for DeepSeek
  (incl. `xhigh → "max"`), nested for OpenAI, budget for Anthropic/Gemini,
  `reasoning_level` generic; disable via `thinking:{type:disabled}` (hosted
  DeepSeek/GLM), `chat_template_kwargs` (vLLM/SGLang), `thinking_budget:0`
  (Gemini).
- **Thinking disabled for cheap purposes**: `src/provider/summarize.rs` already
  disables thinking for title/summary-style one-shots — dsh's
  `purpose === 'session-title'` equivalent.
- **Reasoning passback**: reasoning blocks are echoed back for every backend
  except OpenAI Responses (`rig_stream_factory.rs`), renamed
  `reasoning_content → reasoning` for Cerebras at the wire boundary.
- **R1 quirks**: `scavenge.rs` recovers tool-call JSON emitted inside
  `reasoning_content`; `heal.rs` stamps empty `reasoning_content`.
- **Cache accounting**: DeepSeek `cached_tokens` and Anthropic
  `cache_creation_input_tokens` tracked separately in usage/session.
- **Empty-response / retry / rate limits**: stream-retry preserves rendered
  output; `rate_limit_gate.rs` latches known-reset 429s and synthesizes the
  doomed request instead of sending it; billing fallback routes
  quota-shaped 429s.
- **Context windows**: static per-model table + embedded models.dev snapshot
  (`context_window_for_model`) — dsh's catalog equivalent.
- **Tool-loop stuckness**: storm breaker *suppresses* repeated identical calls
  and surfaces a first-person explanation; `capability.rs` distinguishes
  recovered fumbles from stuck loops; reflexion accumulates lessons. This is
  stronger than dsh's advisory-only reminder in the veto dimension, but
  lacks dsh's advisory *nudge* tier (see A2).
- **Per-agent model pinning**: subagent profiles (`.dirge/agents/`) pin model +
  system prompt + tool allowlist + max_turns/timeout/tier — dsh's
  `agent-default-model` equivalent, minus effort.

## Track A — general harness improvements (adopt)

- **A1. Per-tool timeout policy with a structured error code.**
  dsh: tools declare `timeoutMs`; a guard arms the deadline and substitutes a
  structured `TOOL_TIMEOUT` error result the model can route on.
  dirge today: bash has a `timeout` param, plugin hooks have a timeout,
  `task_status` has a hard cap — but there is no uniform per-tool declared
  budget with one recognizable error kind in the tool result.
  Adopt: add an optional `timeout_ms` to the `LoopTool` trait (or its def
  metadata), enforced centrally in the dispatch path, emitting
  `Error: tool call timed out after Nms` as an error tool result. Small,
  provider-agnostic, model-legible.

- **A2. Advisory repeat-call reminder as the tier before suppression.**
  dsh: gentle reminder at `thresholds[0]` (default 3), detailed reminder
  (tool name, run length, canonical deep-key-sorted args preview) at 5/8;
  counting happens post-execute so denied calls also count; a user
  interjection resets the chain; it never vetoes.
  dirge today: storm breaker jumps to suppression; nothing advisory precedes
  it.
  Adopt: insert an advisory nudge into the existing failure-tracker/storm
  path at count 3 (reuse dsh's canonicalization: deep key-sort of parsed
  args so key order doesn't defeat detection). Cheaper than a full suppress
  for the "model forgot it already ran this" case, which is common on
  smaller models.

- **A3. Retry policy owned per provider.**
  dsh: `llm-retry` has zero own config; each provider config carries its
  `retryPolicy`; mixing them fails loud at load.
  dirge today: `stream_chunk_timeout_secs` is already per-provider in
  `ProviderEntry`; retry counts/backoff are global.
  Adopt: optional `retry` object on `ProviderEntry` (max_attempts,
  backoff ceiling) falling back to the global defaults. Lets a flaky local
  vLLM retry aggressively without loosening a strict hosted API.

- **A4. Deterministic tool-result pruning for replay.**
  dsh: replay-safe, model-free head/middle/tail pruning with an explicit
  prune marker, threshold in chars, below the compaction threshold.
  dirge today: llmtrim has a full stage pipeline (jsoncrush, skeleton,
  toolout …) for *incoming* results, but old tool results riding in history
  are only addressed by full compaction.
  Adopt: a pre-compaction stage that deterministically prunes *stale* tool
  results in history (keep head/tail of each result, marker for the elided
  middle) once the session crosses a threshold. Deterministic →
  prefix-stable across requests → cache-friendly.

Not adopted (deliberately):
- dsh's sectioned system-prompt registry / persona rows — dirge's prompt is a
  deliberate static constant plus `capability_cards` dynamic projection;
  a plugin waterfall would fight the KV-cache stability dirge already
  documents. The one cheap borrow is `toolOrder` validation if capability
  cards ever gain ordering config.
- dsh's `agent-default-model` — subagent profiles already cover this.

## Track B — DeepSeek-specific steering

- **B1. Trim reasoning passback to tool-call turns (main win).**
  dsh: an assistant message's `reasoning_content` is replayed only when that
  message carries tool calls (the thinking-mode passback requirement); plain
  text turns omit it. Deterministic per message → prefix-stable → minimal
  tokens.
  dirge today (`rig_stream_factory.rs`): reasoning is echoed for EVERY
  assistant message on every non-OpenAI backend. On DeepSeek this re-buys
  all prior reasoning as input tokens every turn even where the API does
  not require it.
  Adopt: for providers whose wire requires passback (deepseek, llama.cpp
  family), include reasoning blocks only on assistant messages that contain
  tool calls. Keep Anthropic's rule as-is (its thinking blocks + signatures
  have their own contract). This is the single highest-value DeepSeek token
  saving in the survey. Cache caveat: the rule must be a pure function of
  the stored message (never "latest turn only") or it breaks DeepSeek
  prefix caching across requests — dsh's per-message rule is the safe shape.

- **B2. Effort defaults per model.**
  `deepseek-chat` wants effort off/high; `deepseek-reasoner`-class models
  want high/max. Today effort is a per-request UI/steering choice; a fresh
  session or model switch starts from the global default. Persist a
  `thinking`/effort default alongside the model (see per-model config
  below) so switching to a reasoning model starts in the right mode.

- **B3. (Verify) Empty-completion classification.**
  dsh treats empty completions as a retryable `EMPTY_RESPONSE` class. dirge
  bails on empty in one-shot summarize and drops empty assistant turns in
  history conversion, and stream-retry covers mid-stream aborts — but an
  explicit "empty finish with reason=stop" retry tier for chat-completions
  providers is worth a test to confirm it's already covered end-to-end.

## Per-model config: verdict

dirge today is **per-provider-alias, not per-model**:

- `ProviderEntry` (config.json `providers.<alias>`): default model,
  `options.temperature`, `multimodal` override, `stream_chunk_timeout_secs`,
  headers, auth, base_url.
- Static per-model knowledge: context window (table + models.dev), image
  support heuristic. Not user-configurable per model.
- Reasoning wire shapes: static per *provider type* — correct, since they're
  wire-protocol facts, not model preferences.
- Subagent profiles: per-agent model + preamble, but no thinking level.

What the DeepSeek work actually needs is narrow: **effort/thinking default per
model** (B2) and, optionally, per-model `max_tokens`/`context_window`
overrides within one alias. Everything else that "helps DeepSeek" in dsh
(passback, effort shapes, disable shapes, cache accounting, scavenge) is
per-provider or global and already broadly applicable across dirge's backends
— no general per-model config framework is warranted.

Minimal extension (avoid a generic `models:` map until a second use case
appears):

1. `ProviderEntry.thinking_level: Option<ThinkingLevel>` — default effort for
   the alias's default model, applied when the user hasn't chosen one this
   session.
2. If per-model-within-alias tuning shows up later (e.g. one alias serving
   both chat and reasoner), add `ProviderEntry.models: Map<model, {thinking_level, max_tokens, context_window}>` at that point.
3. `.dirge/agents/` profile schema: add optional `thinking` so a reviewer
   subagent can pin low effort while the coordinator runs high.

## Suggested order

1. B1 (passback trim) — biggest token win, contained change + tests in
   `rig_stream_factory.rs`.
2. A2 (advisory reminder) — UX win for small/reasoning models.
3. B2 + per-model thinking default — small config surface.
4. A1 (per-tool timeout) — uniformity win.
5. A3, A4 — when a concrete need bites.
