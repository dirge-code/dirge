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
    /// Nested `{"reasoning":{"effort":"low"|"medium"|"high"}}` — OpenAI
    /// Responses / openai-compat (openai, glm, custom, openrouter).
    NestedEffort,
    /// Top-level `{"reasoning_effort":"low"|"medium"|"high"|"max"}` — hosted
    /// DeepSeek honors this (not the nested form) and supports the "max" tier.
    TopLevelEffort,
    /// `{"thinking":{"type":"enabled","budget_tokens":N}}` — Anthropic (budget).
    AnthropicBudget,
    /// `{"thinking_config":{"thinking_budget":N}}` — Gemini (budget).
    GeminiBudget,
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
    /// `{"thinking_config":{"thinking_budget":0}}` — Gemini.
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
/// to its reasoning wire profile.
pub fn reasoning_profile(provider: Option<&str>) -> ReasoningProfile {
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
            effort: EffortWire::NestedEffort,
            disable: DisableWire::ThinkingToggle,
        },
        Some("openai") => ReasoningProfile {
            effort: EffortWire::NestedEffort,
            disable: DisableWire::None,
        },
        Some("custom") | Some("openrouter") => ReasoningProfile {
            effort: EffortWire::NestedEffort,
            disable: DisableWire::ChatTemplateKwargs,
        },
        Some("opencode") => ReasoningProfile {
            effort: EffortWire::TopLevelEffort,
            disable: DisableWire::ThinkingToggle,
        },
        Some("gemini") => ReasoningProfile {
            effort: EffortWire::GeminiBudget,
            disable: DisableWire::GeminiZeroBudget,
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

/// Map our `ThinkingLevel` enum to OpenAI Responses `reasoning.
/// effort` strings ("low" | "medium" | "high"). `Off` → None
/// (no reasoning key in the request).
///
/// Pi's `Minimal` / `Xhigh` are clamped to the nearest OpenAI
/// effort since OpenAI's API only accepts the three.
fn thinking_level_to_openai_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh => Some("high"),
    }
}

/// DeepSeek's hosted API honors a top-level `reasoning_effort` string and
/// supports a "max" tier above "high" (which OpenAI rejects). Verified:
/// low→max gives a clean ~2x reasoning-depth separation, whereas the
/// nested `reasoning:{effort}` shape is ignored.
fn thinking_level_to_deepseek_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("max"),
    }
}

/// Token budget for a thinking level. Reads from the caller's
/// `ThinkingBudgets` if provided, falling back to defaults
/// reasonable for token-budget reasoning models (Anthropic
/// budget mode, Gemini 2.x).
///
/// Defaults match the rough scale pi uses (`providers/simple-
/// options.ts:33-...`): minimal 1024, low 2048, medium 4096,
/// high 16384. `Off` returns 0 — caller skips the key entirely.
fn budget_for_level(level: ThinkingLevel, budgets: Option<&ThinkingBudgets>) -> u32 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => budgets.and_then(|b| b.minimal).unwrap_or(1024),
        ThinkingLevel::Low => budgets.and_then(|b| b.low).unwrap_or(2048),
        ThinkingLevel::Medium => budgets.and_then(|b| b.medium).unwrap_or(4096),
        ThinkingLevel::High | ThinkingLevel::Xhigh => budgets.and_then(|b| b.high).unwrap_or(16384),
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
            EffortWire::TopLevelEffort => thinking_level_to_deepseek_effort(level)
                .map(|e| serde_json::json!({ "reasoning_effort": e })),
            EffortWire::AnthropicBudget => {
                let b = budget_for_level(level, budgets);
                (b > 0).then(
                    || serde_json::json!({ "thinking": { "type": "enabled", "budget_tokens": b } }),
                )
            }
            EffortWire::GeminiBudget => {
                let b = budget_for_level(level, budgets);
                (b > 0).then(|| serde_json::json!({ "thinking_config": { "thinking_budget": b } }))
            }
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
            DisableWire::GeminiZeroBudget => {
                Some(serde_json::json!({ "thinking_config": { "thinking_budget": 0 } }))
            }
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
            ("glm", EffortWire::NestedEffort, DisableWire::ThinkingToggle),
            ("openai", EffortWire::NestedEffort, DisableWire::None),
            (
                "custom",
                EffortWire::NestedEffort,
                DisableWire::ChatTemplateKwargs,
            ),
            (
                "openrouter",
                EffortWire::NestedEffort,
                DisableWire::ChatTemplateKwargs,
            ),
            (
                "opencode",
                EffortWire::TopLevelEffort,
                DisableWire::ThinkingToggle,
            ),
            (
                "gemini",
                EffortWire::GeminiBudget,
                DisableWire::GeminiZeroBudget,
            ),
            ("ollama", EffortWire::GenericLevel, DisableWire::OllamaThink),
        ];
        for &(name, effort, disable) in cases {
            let p = reasoning_profile(Some(name));
            assert_eq!(
                (p.effort, p.disable),
                (effort, disable),
                "profile mismatch for {name}"
            );
        }
    }

    #[test]
    fn profile_table_none_and_unknown() {
        let none = reasoning_profile(None);
        assert_eq!(
            (none.effort, none.disable),
            (EffortWire::GenericLevel, DisableWire::None)
        );
        let unknown = reasoning_profile(Some("bogus"));
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
            Some(serde_json::json!({ "thinking_config": { "thinking_budget": 0 } }))
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
            v["thinking_config"]["thinking_budget"],
            budget_for_level(ThinkingLevel::Low, None)
        );
        assert_eq!(p.effort_params(ThinkingLevel::Off, None), None);
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
    }
}
