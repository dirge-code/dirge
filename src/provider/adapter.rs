//! Per-provider reasoning wire profiles — the single source of truth for how
//! a provider expresses "think this hard" (effort) and "don't think" (disable).
//!
//! Consolidates what was previously split across:
//! - `build_provider_additional_params` (agent_loop/rig_stream_factory.rs) — ENABLE
//! - `reasoning_disable_for_kind` (provider/summarize.rs) — DISABLE
//!
//! Adding or tuning a provider's reasoning shape now happens in exactly one file.

use crate::agent::agent_loop::types::{ThinkingBudgets, ThinkingLevel};

// ---------------------------------------------------------------------------
// Wire shape enums
// ---------------------------------------------------------------------------

/// How a provider encodes reasoning EFFORT on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortWire {
    /// Nested `{"reasoning":{"effort":"low"|"medium"|"high"|"xhigh"|"max"}}` —
    /// OpenAI Responses, which accepts the full tier set.
    NestedEffort,
    /// Nested `{"reasoning":{"effort":"low"|"medium"|"high"}}` — generic
    /// OpenAI-compatible endpoints (custom, openrouter).
    ///
    /// Same shape as [`EffortWire::NestedEffort`] but clamped to the classic
    /// three values. A self-hosted vLLM/llama.cpp/LM Studio backend validates
    /// `effort` against that set and 400s on anything else, and we cannot know
    /// what an arbitrary `custom` base URL is running.
    NestedStandardEffort,
    /// Top-level `{"reasoning_effort":"low"|"medium"|"high"|"max"}` — hosted
    /// DeepSeek honors this (not the nested form) and supports the "max" tier.
    TopLevelEffort,
    /// Top-level `{"reasoning_effort":"low"|"high"|"max"}` — z.ai GLM. GLM-5.3
    /// accepts only these three values (anything else errors); GLM-5.2 accepts a
    /// wider set that collapses to the same three. Our six-level
    /// `ThinkingLevel` is mapped to a value both accept — see
    /// `thinking_level_to_glm_effort`.
    TopLevelEffortGlm,
    /// Top-level `{"reasoning_effort":"low"|"medium"|"high"}` with
    /// unsupported extreme levels clamped to the standard three-value set.
    TopLevelStandardEffort,
    /// `{"thinking":{"type":"enabled","budget_tokens":N}}` — Anthropic (budget).
    AnthropicBudget,
    /// `{"generationConfig":{"thinkingConfig":{"thinkingBudget":N}}}` —
    /// Gemini 2.5, which takes a token budget and rejects `thinkingLevel`.
    ///
    /// The nesting is not decoration. rig 0.41 deserializes
    /// `additional_params` into `AdditionalParameters { generation_config,
    /// #[serde(flatten)] additional_params }` with `rename_all = "camelCase"`,
    /// so it claims exactly the key `generationConfig` and flattens everything
    /// else into the request body at the TOP level. A bare `thinking_config`
    /// was therefore never a `generationConfig` field — it went out top-level
    /// and Gemini answered `Unknown name "thinking_config": Cannot find
    /// field` (GH #832).
    GeminiBudget,
    /// `{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"}}}` —
    /// Gemini 3, which takes a depth level. It still accepts `thinkingBudget`
    /// for back-compat but rejects a request carrying both, so the two are
    /// mutually exclusive and the model id picks one.
    GeminiLevel,
    /// Generic `{"reasoning_level":<level>}` passthrough — Ollama / unknown.
    GenericLevel,
}

/// How a provider DISABLES extended reasoning for tool-less one-shots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableWire {
    /// `{"thinking":{"type":"disabled"}}` — hosted DeepSeek / GLM.
    ThinkingToggle,
    /// `{"chat_template_kwargs":{"thinking":false}}` — self-hosted vLLM/SGLang.
    ChatTemplateKwargs,
    /// `{"think":false}` — Ollama.
    OllamaThink,
    /// `{"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}` —
    /// Gemini 2.5, where a zero budget is a real "no thinking". Gemini 3 has
    /// no equivalent (Google documents that 3 Pro / Flash / Flash-Lite cannot
    /// be fully turned off), so it carries [`DisableWire::None`] instead.
    GeminiZeroBudget,
    /// No safe disable knob — request left untouched (OpenAI, Anthropic, unknown).
    None,
}

/// Per-provider reasoning wire profile — the single source of truth for how a
/// provider expresses "think this hard" and "don't think".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningProfile {
    pub effort: EffortWire,
    pub disable: DisableWire,
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Map a provider name (as used in `provider_name` / `oneshot_provider_kind`)
/// and the concrete model id to their reasoning wire profile.
///
/// The model matters because a provider's shape is not always uniform across
/// its own generations: Gemini 2.5 and Gemini 3 take mutually exclusive
/// thinking knobs (GH #832). `None` for the model means "unknown id", and every
/// arm answers with the shape that is safe not knowing.
pub fn reasoning_profile(provider: Option<&str>, model: Option<&str>) -> ReasoningProfile {
    match provider {
        Some("anthropic") => ReasoningProfile {
            effort: EffortWire::AnthropicBudget,
            disable: DisableWire::None,
        },
        Some("deepseek") => ReasoningProfile {
            effort: EffortWire::TopLevelEffort,
            disable: DisableWire::ThinkingToggle,
        },
        Some("glm") => ReasoningProfile {
            effort: EffortWire::TopLevelEffortGlm,
            disable: DisableWire::ThinkingToggle,
        },
        Some("cerebras") => ReasoningProfile {
            effort: EffortWire::TopLevelStandardEffort,
            disable: DisableWire::None,
        },
        Some("openai") => ReasoningProfile {
            effort: EffortWire::NestedEffort,
            disable: DisableWire::None,
        },
        Some("custom") | Some("openrouter") => ReasoningProfile {
            effort: EffortWire::NestedStandardEffort,
            disable: DisableWire::ChatTemplateKwargs,
        },
        Some("opencode") => ReasoningProfile {
            effort: EffortWire::TopLevelEffort,
            disable: DisableWire::ThinkingToggle,
        },
        Some("gemini") => match gemini_generation(model) {
            GeminiGeneration::Level => ReasoningProfile {
                effort: EffortWire::GeminiLevel,
                // No knob: thinking is not fully disablable on Gemini 3, and
                // emitting a level would be claiming an "off" that does not
                // exist.
                disable: DisableWire::None,
            },
            GeminiGeneration::Budget => ReasoningProfile {
                effort: EffortWire::GeminiBudget,
                disable: DisableWire::GeminiZeroBudget,
            },
        },
        Some("ollama") => ReasoningProfile {
            effort: EffortWire::GenericLevel,
            disable: DisableWire::OllamaThink,
        },
        _ => ReasoningProfile {
            effort: EffortWire::GenericLevel,
            disable: DisableWire::None,
        },
    }
}

// ---------------------------------------------------------------------------
// Level → effort helpers
// ---------------------------------------------------------------------------

/// Map our `ThinkingLevel` enum to OpenAI Responses `reasoning.effort`
/// strings. OpenAI's `ReasoningEffort` Literal is the full set
/// `none | minimal | low | medium | high | xhigh | max` (verified against
/// openai-python `reasoning_effort.py`), so `xhigh` and `max` are passed
/// through distinctly. `Off` → None (no reasoning key in the request).
/// `Minimal` clamps to `"low"` — OpenAI accepts `minimal` but dirge's
/// `Minimal` and `Low` already share `low` on the other effort wires; keep
/// them joined here for consistency. OpenAI is the canonical full-tier
/// provider; the 3-tier folds (DeepSeek/GLM `xhigh`→`max`, Cerebras
/// `xhigh`/`max`→`high`) live in their own mapping functions.
fn thinking_level_to_openai_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

/// Map `ThinkingLevel` to the classic OpenAI `reasoning.effort` triple for
/// generic OpenAI-compatible endpoints. `Xhigh`/`Max` clamp DOWN to `"high"`:
/// unlike OpenAI proper, an arbitrary self-hosted backend validates against
/// `low`/`medium`/`high` and rejects the extended tiers outright.
fn thinking_level_to_openai_compat_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high"),
    }
}

/// DeepSeek's hosted API honors a top-level `reasoning_effort` string and
/// supports a "max" tier above "high". DeepSeek accepts `low`/`high`/`max`
/// only and has NO `xhigh` tier — per the "rounds up" rule, `Xhigh` folds
/// to `max` (its ceiling) and `Max` is also `max`. `Minimal`/`Low`→`low`.
/// (DeepSeek's own docs fold `medium`→`high`, but we send `medium` and let
/// the server fold — `medium` is a valid wire string DeepSeek accepts.)
fn thinking_level_to_deepseek_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("max"),
    }
}

/// Map `ThinkingLevel` to a z.ai GLM `reasoning_effort` value. GLM-5.3 accepts
/// only `low` / `high` / `max` (anything else errors); GLM-5.2 accepts a wider
/// set that collapses to those three. There is no `xhigh` tier, so `Xhigh`
/// rounds up to `max` (the "rounds up" rule) and `Max` is `max` too.
/// `Minimal`/`Low`→`low`, `Medium`/`High`→`high` (mirroring z.ai's own
/// GLM-5.2 fold).
fn thinking_level_to_glm_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium | ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("max"),
    }
}

/// Which thinking knob a Gemini model takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiGeneration {
    /// Gemini 2.5 and earlier — `thinkingBudget`, a token count.
    Budget,
    /// Gemini 3 — `thinkingLevel`, a depth.
    Level,
}

/// Classify a Gemini model id by which thinking knob it takes.
///
/// Gemini 2.5 accepts `thinkingBudget` and ignores/rejects `thinkingLevel`;
/// Gemini 3 prefers `thinkingLevel` and rejects a request carrying both. An id
/// we cannot classify keeps the budget: it is the shape every generation still
/// accepts, so an unknown id degrades to today's behaviour rather than to a
/// 400. An OpenRouter-style `vendor/` prefix is stripped first, matching
/// [`crate::provider::model_family`].
fn gemini_generation(model: Option<&str>) -> GeminiGeneration {
    let Some(model) = model else {
        return GeminiGeneration::Budget;
    };
    let id = model.trim().to_ascii_lowercase();
    let bare = id.rsplit('/').next().unwrap_or(id.as_str());
    let Some(rest) = bare.strip_prefix("gemini-") else {
        return GeminiGeneration::Budget;
    };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match major.parse::<u32>() {
        Ok(v) if v >= 3 => GeminiGeneration::Level,
        _ => GeminiGeneration::Budget,
    }
}

/// Map `ThinkingLevel` to a Gemini 3 `thinkingLevel`.
///
/// Only `low` / `medium` / `high` are emitted. Gemini 3 also has a `minimal`
/// tier, but unevenly — 3.6-Flash has it and 3.7-Flash does not — so `Minimal`
/// folds to `low`, which every 3.x model accepts and which is where `Minimal`
/// already lands on most of dirge's other wires. `Xhigh`/`Max` fold to `high`,
/// Gemini 3's ceiling. `Off` returns None: the caller reaches for the disable
/// wire, which on Gemini 3 is deliberately nothing.
fn thinking_level_to_gemini_level(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high"),
    }
}

/// Wrap a `thinkingConfig` body where rig will actually find it — see
/// [`EffortWire::GeminiBudget`] for why the nesting is load-bearing.
fn gemini_generation_config(thinking: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "generationConfig": { "thinkingConfig": thinking } })
}

/// Token budget for a thinking level. Reads from the caller's
/// `ThinkingBudgets` if provided, falling back to defaults
/// reasonable for token-budget reasoning models (Anthropic
/// budget mode, Gemini 2.x).
///
/// Defaults match the rough scale pi uses (`providers/simple-
/// options.ts:33-...`): minimal 1024, low 2048, medium 4096,
/// high 16384. `Off` returns 0 — caller skips the key entirely.
/// The thinking allocation a level is granted, in tokens.
///
/// `pub(crate)` because it is also the basis of the client-side runaway cap in
/// [`crate::agent::agent_loop::thinking_budget`] (dirge-vzsy): that cap must be
/// derived from what we actually grant, or the harness ends up cutting off
/// reasoning it just finished asking for.
pub(crate) fn budget_for_level(level: ThinkingLevel, budgets: Option<&ThinkingBudgets>) -> u32 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => budgets.and_then(|b| b.minimal).unwrap_or(1024),
        ThinkingLevel::Low => budgets.and_then(|b| b.low).unwrap_or(2048),
        ThinkingLevel::Medium => budgets.and_then(|b| b.medium).unwrap_or(4096),
        ThinkingLevel::High => budgets.and_then(|b| b.high).unwrap_or(16384),
        // Xhigh and Max differ on OpenAI/Anthropic; the budget tiers must
        // differ too, or `/effort max` would grant the same thinking budget
        // as `xhigh` on an Anthropic/Gemini model. Xhigh keeps the legacy
        // 16384 default (long-horizon agentic thinking); Max gets a larger
        // "unconstrained capability" budget. Both are caller-overridable.
        // Falls back to `high` before the literal: `xhigh` is new, and a
        // caller that had tuned `high` was getting that value at this tier
        // before the Xhigh/Max split. Ignoring it here would silently cut
        // their budget back to the default.
        ThinkingLevel::Xhigh => budgets.and_then(|b| b.xhigh.or(b.high)).unwrap_or(16384),
        ThinkingLevel::Max => budgets.and_then(|b| b.max).unwrap_or(32768),
    }
}

/// Room reserved for the visible answer on top of a thinking budget.
///
/// Anthropic's `max_tokens` covers thinking tokens AND output tokens, and the
/// API rejects any request where `budget_tokens >= max_tokens`. So the ceiling
/// has to be the budget plus enough left over for the turn to actually say
/// something — a tool call plus its reasoning preamble sits comfortably here.
pub const REASONING_OUTPUT_HEADROOM: u32 = 8_192;

/// The `max_tokens` a request must carry to be able to spend `level`'s
/// thinking budget, or `None` for providers that put no budget on the wire.
///
/// This exists because rig picks `max_tokens` for us when we leave it unset,
/// and its Anthropic default is 2048 for any model id it doesn't recognise
/// (`default_max_tokens_for_model` matches `claude-opus-4*` / `claude-sonnet-4*`
/// / `claude-haiku-4-5*` only — every Claude 5 id falls through). 2048 is below
/// every budget tier above `minimal`, so leaving it unset means the API rejects
/// the request outright. Anything that emits `budget_tokens` must therefore
/// also pin the ceiling above it.
///
/// Deliberately `None` for the effort-string providers: they send no budget, so
/// forcing a ceiling would only override the model's own, larger default.
pub fn max_tokens_for_reasoning(
    provider: Option<&str>,
    model: Option<&str>,
    level: ThinkingLevel,
    budgets: Option<&ThinkingBudgets>,
) -> Option<u64> {
    if !matches!(
        reasoning_profile(provider, model).effort,
        EffortWire::AnthropicBudget
    ) {
        return None;
    }
    let budget = budget_for_level(level, budgets);
    if budget == 0 {
        return None;
    }
    Some(u64::from(budget) + u64::from(REASONING_OUTPUT_HEADROOM))
}

/// Cerebras accepts `reasoning_effort` values `low` / `medium` / `high` /
/// `none` only (per inference-docs.cerebras.ai) — no `xhigh` or `max` tier.
/// `Xhigh` and `Max` both clamp down to `"high"`; `Minimal`→`low`; `Off`
/// omits the key.
fn thinking_level_to_cerebras_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high"),
    }
}

// ---------------------------------------------------------------------------
// Encode methods
// ---------------------------------------------------------------------------

impl ReasoningProfile {
    /// Request params to REQUEST reasoning at `level`. Returns a single-key
    /// JSON object to be merged into the request's additional params, or None
    /// when there is nothing to add.
    pub fn effort_params(
        &self,
        level: ThinkingLevel,
        budgets: Option<&ThinkingBudgets>,
    ) -> Option<serde_json::Value> {
        match self.effort {
            EffortWire::NestedEffort => thinking_level_to_openai_effort(level)
                .map(|e| serde_json::json!({ "reasoning": { "effort": e } })),
            EffortWire::NestedStandardEffort => thinking_level_to_openai_compat_effort(level)
                .map(|e| serde_json::json!({ "reasoning": { "effort": e } })),
            EffortWire::TopLevelEffort => thinking_level_to_deepseek_effort(level)
                .map(|e| serde_json::json!({ "reasoning_effort": e })),
            EffortWire::TopLevelEffortGlm => thinking_level_to_glm_effort(level)
                .map(|e| serde_json::json!({ "reasoning_effort": e })),
            EffortWire::TopLevelStandardEffort => thinking_level_to_cerebras_effort(level)
                .map(|effort| serde_json::json!({ "reasoning_effort": effort })),
            EffortWire::AnthropicBudget => {
                let b = budget_for_level(level, budgets);
                (b > 0).then(
                    || serde_json::json!({ "thinking": { "type": "enabled", "budget_tokens": b } }),
                )
            }
            EffortWire::GeminiBudget => {
                let b = budget_for_level(level, budgets);
                (b > 0)
                    .then(|| gemini_generation_config(serde_json::json!({ "thinkingBudget": b })))
            }
            EffortWire::GeminiLevel => thinking_level_to_gemini_level(level)
                .map(|l| gemini_generation_config(serde_json::json!({ "thinkingLevel": l }))),
            EffortWire::GenericLevel => Some(
                serde_json::json!({ "reasoning_level": serde_json::to_value(level).unwrap_or(serde_json::Value::Null) }),
            ),
        }
    }

    /// Request params to DISABLE reasoning for a tool-less one-shot, or None
    /// when the provider has no safe disable knob.
    pub fn disable_params(&self) -> Option<serde_json::Value> {
        match self.disable {
            DisableWire::ThinkingToggle => {
                Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
            }
            DisableWire::ChatTemplateKwargs => {
                Some(serde_json::json!({ "chat_template_kwargs": { "thinking": false } }))
            }
            DisableWire::OllamaThink => Some(serde_json::json!({ "think": false })),
            DisableWire::GeminiZeroBudget => Some(gemini_generation_config(
                serde_json::json!({ "thinkingBudget": 0 }),
            )),
            DisableWire::None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- reasoning_profile table -----

    #[test]
    fn profile_table_all_known_providers() {
        // (provider, expected_effort, expected_disable)
        let cases: &[(&str, EffortWire, DisableWire)] = &[
            ("anthropic", EffortWire::AnthropicBudget, DisableWire::None),
            (
                "deepseek",
                EffortWire::TopLevelEffort,
                DisableWire::ThinkingToggle,
            ),
            (
                "glm",
                EffortWire::TopLevelEffortGlm,
                DisableWire::ThinkingToggle,
            ),
            (
                "cerebras",
                EffortWire::TopLevelStandardEffort,
                DisableWire::None,
            ),
            ("openai", EffortWire::NestedEffort, DisableWire::None),
            (
                "custom",
                EffortWire::NestedStandardEffort,
                DisableWire::ChatTemplateKwargs,
            ),
            (
                "openrouter",
                EffortWire::NestedStandardEffort,
                DisableWire::ChatTemplateKwargs,
            ),
            (
                "opencode",
                EffortWire::TopLevelEffort,
                DisableWire::ThinkingToggle,
            ),
            // Keyed by provider alone (model `None`), which is the
            // unclassifiable case: the budget wire, accepted by every Gemini
            // generation. The model-aware split is covered by
            // `gemini_profile_follows_the_model_generation`.
            (
                "gemini",
                EffortWire::GeminiBudget,
                DisableWire::GeminiZeroBudget,
            ),
            ("ollama", EffortWire::GenericLevel, DisableWire::OllamaThink),
        ];
        for &(name, effort, disable) in cases {
            let p = reasoning_profile(Some(name), None);
            assert_eq!(
                (p.effort, p.disable),
                (effort, disable),
                "profile mismatch for {name}"
            );
        }
    }

    #[test]
    fn profile_table_none_and_unknown() {
        let none = reasoning_profile(None, None);
        assert_eq!(
            (none.effort, none.disable),
            (EffortWire::GenericLevel, DisableWire::None)
        );
        let unknown = reasoning_profile(Some("bogus"), None);
        assert_eq!(
            (unknown.effort, unknown.disable),
            (EffortWire::GenericLevel, DisableWire::None)
        );
    }

    // ----- disable_params -----

    #[test]
    fn disable_params_all_variants() {
        assert_eq!(
            ReasoningProfile {
                effort: EffortWire::NestedEffort,
                disable: DisableWire::ThinkingToggle
            }
            .disable_params(),
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
        assert_eq!(
            ReasoningProfile {
                effort: EffortWire::NestedEffort,
                disable: DisableWire::ChatTemplateKwargs
            }
            .disable_params(),
            Some(serde_json::json!({ "chat_template_kwargs": { "thinking": false } }))
        );
        assert_eq!(
            ReasoningProfile {
                effort: EffortWire::NestedEffort,
                disable: DisableWire::OllamaThink
            }
            .disable_params(),
            Some(serde_json::json!({ "think": false }))
        );
        assert_eq!(
            ReasoningProfile {
                effort: EffortWire::NestedEffort,
                disable: DisableWire::GeminiZeroBudget
            }
            .disable_params(),
            Some(
                serde_json::json!({ "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } } })
            )
        );
        assert_eq!(
            ReasoningProfile {
                effort: EffortWire::NestedEffort,
                disable: DisableWire::None
            }
            .disable_params(),
            None
        );
    }

    // ----- effort_params -----

    #[test]
    fn effort_nested_high_off() {
        let p = ReasoningProfile {
            effort: EffortWire::NestedEffort,
            disable: DisableWire::None,
        };
        assert_eq!(
            p.effort_params(ThinkingLevel::High, None),
            Some(serde_json::json!({ "reasoning": { "effort": "high" } }))
        );
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
    }

    /// OpenAI is the canonical full-tier provider: `xhigh` and `max` are
    /// distinct wire values in OpenAI's `ReasoningEffort` Literal
    /// (`none|minimal|low|medium|high|xhigh|max`). The whole point of the
    /// Xhigh/Max split is that these do NOT collapse on OpenAI/gpt-5.x.
    #[test]
    fn openai_effort_keeps_xhigh_and_max_distinct() {
        let profile = reasoning_profile(Some("openai"), None);
        assert_eq!(profile.effort, EffortWire::NestedEffort);
        assert_eq!(
            profile.effort_params(ThinkingLevel::High, None),
            Some(serde_json::json!({"reasoning":{"effort":"high"}}))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Xhigh, None),
            Some(serde_json::json!({"reasoning":{"effort":"xhigh"}}))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Max, None),
            Some(serde_json::json!({"reasoning":{"effort":"max"}}))
        );
        assert_ne!(
            profile.effort_params(ThinkingLevel::Xhigh, None),
            profile.effort_params(ThinkingLevel::Max, None)
        );
        assert_eq!(profile.effort_params(ThinkingLevel::Off, None), None);
    }

    /// The new `Max` tier gets a larger thinking budget than `Xhigh` on the
    /// budget-wire providers (Anthropic, Gemini) — otherwise `/effort max`
    /// would grant the same thinking allocation as `/effort xhigh`,
    /// defeating the split's purpose.
    #[test]
    fn max_budget_exceeds_xhigh_budget() {
        let xhigh = budget_for_level(ThinkingLevel::Xhigh, None);
        let max = budget_for_level(ThinkingLevel::Max, None);
        assert!(max > xhigh, "Max ({max}) must exceed Xhigh ({xhigh})");
        assert!(xhigh > 0);
        assert!(max > 0);
        // The Anthropic budget wire carries them through distinctly.
        let p = ReasoningProfile {
            effort: EffortWire::AnthropicBudget,
            disable: DisableWire::None,
        };
        let xhigh_v = p.effort_params(ThinkingLevel::Xhigh, None).unwrap();
        let max_v = p.effort_params(ThinkingLevel::Max, None).unwrap();
        assert_ne!(
            xhigh_v["thinking"]["budget_tokens"], max_v["thinking"]["budget_tokens"],
            "Anthropic must grant Max more thinking budget than Xhigh"
        );
    }

    /// GLM (z.ai) accepts only top-level `reasoning_effort` with the
    /// values `low` / `high` / `max` on GLM-5.3 (anything else errors),
    /// and a wider set on GLM-5.2 that collapses to those three. Mapping
    /// our six-level `ThinkingLevel` so every level resolves to a value
    /// GLM-5.3 accepts: `Minimal`/`Low`→"low", `Medium`→"high",
    /// `High`→"high", `Xhigh`→"max". `Off` omits the key.
    #[test]
    fn glm_effort_top_level_with_max_and_collapse() {
        let profile = reasoning_profile(Some("glm"), None);
        assert_eq!(profile.effort, EffortWire::TopLevelEffortGlm);
        assert_eq!(
            profile.effort_params(ThinkingLevel::Minimal, None),
            Some(serde_json::json!({ "reasoning_effort": "low" }))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Low, None),
            Some(serde_json::json!({ "reasoning_effort": "low" }))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Medium, None),
            Some(serde_json::json!({ "reasoning_effort": "high" }))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::High, None),
            Some(serde_json::json!({ "reasoning_effort": "high" }))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Xhigh, None),
            Some(serde_json::json!({"reasoning_effort":"max"}))
        );
        assert_eq!(
            profile.effort_params(ThinkingLevel::Max, None),
            Some(serde_json::json!({"reasoning_effort":"max"}))
        );
        assert_eq!(profile.effort_params(ThinkingLevel::Off, None), None);
        // Disable knob is unchanged (GLM accepts thinking.type=disabled).
        assert_eq!(
            profile.disable_params(),
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
    }

    #[test]
    fn effort_top_level_xhigh_high_off() {
        let p = ReasoningProfile {
            effort: EffortWire::TopLevelEffort,
            disable: DisableWire::None,
        };
        assert_eq!(
            p.effort_params(ThinkingLevel::Xhigh, None),
            Some(serde_json::json!({ "reasoning_effort": "max" }))
        );
        assert_eq!(
            p.effort_params(ThinkingLevel::High, None),
            Some(serde_json::json!({ "reasoning_effort": "high" }))
        );
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
    }

    #[test]
    fn effort_generic_level() {
        let p = ReasoningProfile {
            effort: EffortWire::GenericLevel,
            disable: DisableWire::None,
        };
        let v = p
            .effort_params(ThinkingLevel::Medium, None)
            .expect("generic level should produce value");
        assert_eq!(
            v["reasoning_level"],
            serde_json::to_value(ThinkingLevel::Medium).unwrap()
        );
    }

    #[test]
    fn effort_anthropic_budget_positive_and_zero() {
        let p = ReasoningProfile {
            effort: EffortWire::AnthropicBudget,
            disable: DisableWire::None,
        };
        // Medium level with default budget (4096)
        let v = p
            .effort_params(ThinkingLevel::Medium, None)
            .expect("medium should produce budget");
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(
            v["thinking"]["budget_tokens"],
            budget_for_level(ThinkingLevel::Medium, None)
        );
        // Off → no thinking key
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
    }

    /// GH #832: the budget must land INSIDE `generationConfig`, which is the
    /// only key rig's Gemini provider claims. A top-level `thinking_config` was
    /// flattened into the request body and rejected outright.
    #[test]
    fn effort_gemini_budget_positive_and_zero() {
        let p = ReasoningProfile {
            effort: EffortWire::GeminiBudget,
            disable: DisableWire::None,
        };
        let v = p
            .effort_params(ThinkingLevel::Low, None)
            .expect("low should produce budget");
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            budget_for_level(ThinkingLevel::Low, None)
        );
        assert!(
            v.get("thinking_config").is_none(),
            "the flat shape is what Gemini rejected: {v}",
        );
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
    }

    /// GH #832: Gemini 3 takes a depth, not a budget, and rejects a request
    /// carrying both — so the id picks exactly one knob.
    #[test]
    fn effort_gemini_level_emits_a_depth_not_a_budget() {
        let p = ReasoningProfile {
            effort: EffortWire::GeminiLevel,
            disable: DisableWire::None,
        };
        let v = p
            .effort_params(ThinkingLevel::Medium, None)
            .expect("medium should produce a level");
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert!(
            v["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none(),
            "budget and level are mutually exclusive on the wire: {v}",
        );
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
    }

    /// `minimal` exists on Gemini 3.6-Flash and not on 3.7-Flash, so it is not
    /// emitted: `Minimal` folds to `low`, the value every 3.x model accepts.
    /// `Xhigh`/`Max` fold to `high`, Gemini 3's ceiling.
    #[test]
    fn gemini_level_folds_to_the_three_universal_tiers() {
        for (level, want) in [
            (ThinkingLevel::Minimal, "low"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Xhigh, "high"),
            (ThinkingLevel::Max, "high"),
        ] {
            assert_eq!(thinking_level_to_gemini_level(level), Some(want));
        }
        assert_eq!(thinking_level_to_gemini_level(ThinkingLevel::Off), None);
    }

    /// GH #832: which knob a Gemini id takes. An id we cannot classify keeps
    /// the budget — the shape every generation still accepts — so an unknown
    /// id degrades to today's behaviour rather than to a 400.
    #[test]
    fn gemini_generation_picks_the_knob_by_major_version() {
        for id in ["gemini-2.5-flash", "gemini-2.0-pro", "GEMINI-2.5-PRO"] {
            assert_eq!(
                gemini_generation(Some(id)),
                GeminiGeneration::Budget,
                "{id}"
            );
        }
        for id in [
            "gemini-3.6-flash",
            "gemini-3.7-flash",
            "google/gemini-3.6-flash",
        ] {
            assert_eq!(gemini_generation(Some(id)), GeminiGeneration::Level, "{id}");
        }
        assert_eq!(gemini_generation(None), GeminiGeneration::Budget);
        assert_eq!(
            gemini_generation(Some("some-proxy-alias")),
            GeminiGeneration::Budget,
        );
    }

    /// The profile follows the id: a Gemini 3 model gets the level wire and no
    /// disable at all, because Google documents that 3 Pro / Flash / Flash-Lite
    /// cannot be fully turned off. Emitting a level there would be claiming an
    /// "off" that does not exist.
    #[test]
    fn gemini_profile_follows_the_model_generation() {
        let two = reasoning_profile(Some("gemini"), Some("gemini-2.5-flash"));
        assert_eq!(two.effort, EffortWire::GeminiBudget);
        assert_eq!(two.disable, DisableWire::GeminiZeroBudget);
        let three = reasoning_profile(Some("gemini"), Some("gemini-3.6-flash"));
        assert_eq!(three.effort, EffortWire::GeminiLevel);
        assert_eq!(three.disable, DisableWire::None);
        assert_eq!(three.disable_params(), None);
    }

    // ----- moved helper tests -----

    #[test]
    fn thinking_level_to_deepseek_effort_all_variants() {
        assert_eq!(thinking_level_to_deepseek_effort(ThinkingLevel::Off), None);
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::Minimal),
            Some("low")
        );
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::Low),
            Some("low")
        );
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::Medium),
            Some("medium")
        );
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::High),
            Some("high")
        );
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::Xhigh),
            Some("max")
        );
        // Max also folds up to "max" (same ceiling as Xhigh on the 3-tier
        // DeepSeek wire — no distinct xhigh tier).
        assert_eq!(
            thinking_level_to_deepseek_effort(ThinkingLevel::Max),
            Some("max")
        );
    }

    #[test]
    fn cerebras_uses_standard_top_level_effort_without_max_or_disable_knob() {
        let profile = reasoning_profile(Some("cerebras"), None);
        for (level, expected) in [
            (ThinkingLevel::Minimal, "low"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            // No xhigh/max tier on Cerebras — both clamp down to "high".
            (ThinkingLevel::Xhigh, "high"),
            (ThinkingLevel::Max, "high"),
        ] {
            let params = profile
                .effort_params(level, None)
                .expect("enabled Cerebras reasoning should produce params");
            assert_eq!(
                params,
                serde_json::json!({ "reasoning_effort": expected }),
                "unexpected Cerebras params for {level:?}",
            );
            assert_ne!(params["reasoning_effort"], "max");
            assert!(params.get("reasoning_level").is_none());
        }

        assert_eq!(profile.effort_params(ThinkingLevel::Off, None), None);
        assert_eq!(profile.disable_params(), None);
    }
}

#[cfg(test)]
mod max_tokens_tests {
    use super::*;

    /// Anthropic rejects a request whose thinking budget is not strictly
    /// below `max_tokens`. Every level that puts a budget on the wire must
    /// therefore also carry a ceiling above it.
    #[test]
    fn anthropic_ceiling_clears_every_budget() {
        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ] {
            let budget = u64::from(budget_for_level(level, None));
            let ceiling = max_tokens_for_reasoning(Some("anthropic"), None, level, None)
                .unwrap_or_else(|| panic!("{level:?} puts a budget on the wire but no ceiling"));
            assert!(
                ceiling > budget,
                "{level:?}: max_tokens {ceiling} must exceed budget_tokens {budget}",
            );
        }
    }

    /// The whole bug: rig defaults Anthropic `max_tokens` to 2048 for any
    /// model id it doesn't recognise, which includes every Claude 5 id. The
    /// ceiling has to beat that default, not just the budget.
    #[test]
    fn anthropic_ceiling_beats_rigs_2048_fallback() {
        let ceiling = max_tokens_for_reasoning(Some("anthropic"), None, ThinkingLevel::Max, None)
            .expect("max is a budget level");
        assert!(
            ceiling > 2_048,
            "must override rig's fallback, got {ceiling}"
        );
    }

    #[test]
    fn off_needs_no_ceiling() {
        assert_eq!(
            max_tokens_for_reasoning(Some("anthropic"), None, ThinkingLevel::Off, None),
            None,
        );
    }

    /// Only the budget-shaped wire needs this. Effort-string providers send
    /// no budget, so forcing a ceiling on them would override the model's
    /// own (larger) default for no reason.
    #[test]
    fn effort_string_providers_get_no_ceiling() {
        for provider in ["openai", "deepseek", "glm", "cerebras", "custom"] {
            assert_eq!(
                max_tokens_for_reasoning(Some(provider), None, ThinkingLevel::Max, None),
                None,
                "{provider} sends an effort string, not a budget",
            );
        }
    }

    /// A caller-supplied budget has to move the ceiling with it.
    #[test]
    fn ceiling_tracks_a_custom_budget() {
        let budgets = ThinkingBudgets {
            max: Some(120_000),
            ..Default::default()
        };
        let ceiling =
            max_tokens_for_reasoning(Some("anthropic"), None, ThinkingLevel::Max, Some(&budgets))
                .expect("max is a budget level");
        assert!(
            ceiling > 120_000,
            "ceiling {ceiling} must clear the 120k budget"
        );
    }
}
