//! Compaction summarization.
//!
//! Serializes conversation history into a prompt for the summarizer
//! model and invokes the model with retry logic. Extracted from
//! `provider/mod.rs`.

use rig::streaming::StreamingChat;

/// Call the summarizer model with the full conversation prefix.
/// The summarizer is invoked by `/compress`, often exactly when the
/// user's context is about to overflow. Uses a retry loop with the
/// same `RecoveryPolicy` shape as the main agent.
///
/// PROV-9: bound the prompt size before dispatch. `/compress` is
/// typically invoked when the conversation already exceeds the
/// model's context window — handing the same un-bounded blob to
/// the summarizer guarantees a ContextLength failure.
///
/// The bound is [`oneshot_prompt_budget_bytes`], derived from the
/// summarizer model's own window. It was a fixed 128 KB until
/// dirge-5zca, on the reasoning that "we can't know the exact window
/// for every provider here" — but [`crate::config::context_window_for_model`]
/// does know, and the fixed number was wrong in both directions.
///
/// When the prompt still does not fit, the head-and-tail strategy
/// preserves the most recent turns (where the recent context lives)
/// plus the earliest turns (which often set up the task).
pub(crate) async fn summarize_with_model(
    model: super::AnyModel,
    prompt: String,
) -> anyhow::Result<String> {
    oneshot_with_model(
        model,
        "summarizer",
        "You are a conversation summarizer.",
        prompt,
    )
    .await
}

/// Generic one-shot LLM call over any `AnyModel` variant with a caller-
/// supplied system preamble. Factored out of `summarize_with_model` so
/// every side-LLM role shares the same exhaustive provider dispatch and
/// retry/stream-drain path. `label` keeps each role distinct in telemetry.
/// The summarizer-sized prompt budget is a no-op for tiny approval/critic
/// prompts.
pub(crate) async fn oneshot_with_model(
    model: super::AnyModel,
    label: &'static str,
    preamble: &str,
    prompt: String,
) -> anyhow::Result<String> {
    let prompt = budgeted::BudgetedPrompt::new(label, &model.name(), prompt);
    // dirge-wire: opt-in dump so a mystery side-LLM call (which prompt, which
    // purpose, which model) is visible. No-op unless DIRGE_DUMP_REQUESTS is set.
    crate::provider::wire::dump_oneshot(
        label,
        model.provider_name(),
        &model.name(),
        preamble,
        prompt.as_str(),
    );
    // dirge-zt8p: disable extended reasoning for this one-shot (see
    // `reasoning_disable_for_kind`). Computed before the consuming match.
    let disable = reasoning_disable_for_kind(model.provider_name(), Some(model.name().as_str()));
    match model {
        super::AnyModel::OpenRouter(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::OpenAI(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::ChatGptOpenAI(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::OpenAICodex(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Anthropic(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::AnthropicOauth(m) => {
            run_oneshot(m, label, preamble, prompt, disable).await
        }
        super::AnyModel::Gemini(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::DeepSeek(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Glm(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Cerebras(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::OpenCode(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Kimi(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Ollama(m) => run_oneshot(m, label, preamble, prompt, disable).await,
        super::AnyModel::Custom(m) => run_oneshot(m, label, preamble, prompt, disable).await,
    }
}

/// dirge-zt8p: provider-specific params to disable extended reasoning
/// for tool-less one-shots. Delegates to the consolidated adapter mapping.
fn reasoning_disable_for_kind(kind: &str, model: Option<&str>) -> Option<serde_json::Value> {
    crate::provider::adapter::reasoning_profile(Some(kind), model).disable_params()
}

/// Fallback prompt budget for a model whose context window we cannot look up.
/// This was the budget for EVERY one-shot until dirge-5zca; it stays only as
/// the unknown-model answer, where a conservative fixed number is the best
/// available guess.
pub(crate) const ONESHOT_FALLBACK_BUDGET_BYTES: usize = 128 * 1024; // ~32k tokens

/// Fraction of a model's context window the INPUT side of a one-shot may use.
/// The remaining quarter covers the response the call exists to get and the
/// error in [`CHARS_PER_TOKEN`], which is a 4-bytes-per-token approximation and
/// runs optimistic on dense input (code, CJK). Overshooting is a hard 400 on
/// context length, so the margin is deliberately generous.
///
/// It is NOT
/// [`HISTORY_FOLD_THRESHOLD`](crate::agent::agent_loop::context_manager::HISTORY_FOLD_THRESHOLD),
/// which happens to be the same number today. That one answers "how full
/// before we fold"; this one answers "how much of the window may the request
/// spend on input". Tying them together would mean retuning the fold silently
/// retunes every side-LLM's safety margin.
///
/// [`CHARS_PER_TOKEN`]: crate::agent::compression::CHARS_PER_TOKEN
const ONESHOT_INPUT_FRACTION: f64 = 0.75;

/// A prompt that has been through the model's input budget.
///
/// The field is private to this inner module, so the ONLY way to obtain one is
/// [`BudgetedPrompt::new`] — and `run_oneshot` accepts nothing else. Skipping
/// the budget is therefore a compile error rather than a silent regression.
///
/// That is not decoration. Mutation testing on the dirge-5zca fix disabled the
/// budget check at the call site (`if false && prompt.len() > budget`) and the
/// entire suite stayed green: every test covered the pure budget FUNCTION, and
/// none covered whether the dispatcher applied it — which is precisely the seam
/// the original bug lived in.
mod budgeted {
    pub(crate) struct BudgetedPrompt(String);

    impl BudgetedPrompt {
        pub(crate) fn new(label: &str, model_name: &str, prompt: String) -> Self {
            let budget = super::oneshot_prompt_budget_bytes(model_name);
            if prompt.len() <= budget {
                return Self(prompt);
            }
            // dirge-5zca: this used to be silent. A compaction summary built
            // from a clipped transcript reads exactly like one built from the
            // whole thing, so the operator's only signal was the summary being
            // worse than expected. Say so, with the numbers.
            tracing::warn!(
                target: "dirge::provider",
                label,
                model = %model_name,
                prompt_bytes = prompt.len(),
                budget_bytes = budget,
                dropped_bytes = prompt.len() - budget,
                "one-shot prompt exceeds the model's input budget — truncating the middle",
            );
            Self(super::head_tail_truncate(&prompt, budget))
        }

        pub(crate) fn as_str(&self) -> &str {
            &self.0
        }

        pub(crate) fn into_inner(self) -> String {
            self.0
        }
    }
}

/// Prompt budget in bytes for a one-shot against `model_name` (dirge-5zca).
///
/// Derived from the model's own context window rather than a fixed number,
/// because a fixed number is wrong in both directions: 128 KB drops most of the
/// conversation on a 200k-token model, and blows the window outright on an
/// 8k-token one. [`crate::config::context_window_for_model`] is already the
/// single source of truth for the window, so this adds no second table.
pub(crate) fn oneshot_prompt_budget_bytes(model_name: &str) -> usize {
    let Some(window) = crate::config::context_window_for_model(model_name) else {
        return ONESHOT_FALLBACK_BUDGET_BYTES;
    };
    let input_tokens = (window as f64 * ONESHOT_INPUT_FRACTION) as u64;
    input_tokens.saturating_mul(crate::agent::compression::CHARS_PER_TOKEN) as usize
}

/// Trim a prompt to `budget` bytes by keeping a head + tail slice
/// with a placeholder noting the drop. Splits on `\n` so we don't
/// land mid-message. Used by the summarizer when the conversation
/// blob would overflow the summarizer's own context window.
pub(crate) fn head_tail_truncate(prompt: &str, budget: usize) -> String {
    if prompt.len() <= budget {
        return prompt.to_string();
    }
    // 40% head, 60% tail — recent context tends to matter more.
    let head_budget = budget * 4 / 10;
    let tail_budget = budget - head_budget - 128; // leave room for the marker

    // Find a newline-aligned head boundary at or before head_budget, floored to
    // a UTF-8 char boundary so slicing never panics.
    let head_end = prompt[..head_budget.min(prompt.len())]
        .rfind('\n')
        .unwrap_or(head_budget.min(prompt.len()));
    let head_end = crate::text::char_boundary_at_or_before(prompt, head_end);

    let tail_start_target = prompt.len().saturating_sub(tail_budget);
    let tail_start = prompt[tail_start_target..]
        .find('\n')
        .map(|i| tail_start_target + i + 1)
        .unwrap_or(tail_start_target);
    let tail_start = crate::text::char_boundary_at_or_after(prompt, tail_start);

    if tail_start <= head_end {
        // The two halves overlap — prompt is already short enough
        // after newline rounding. Fall through to verbatim.
        return prompt.to_string();
    }
    let dropped = tail_start - head_end;
    format!(
        "{}\n\n[... {} bytes truncated by summarizer-prompt budget ...]\n\n{}",
        &prompt[..head_end],
        dropped,
        &prompt[tail_start..],
    )
}

/// Runs one bounded side-LLM call.
///
/// `prompt` is a [`budgeted::BudgetedPrompt`] and not a `String` on purpose —
/// see that type's docs. It is the type system standing in for a test that
/// cannot be written without a mock provider.
async fn run_oneshot<M>(
    model: M,
    label: &'static str,
    preamble: &str,
    prompt: budgeted::BudgetedPrompt,
    reasoning_disable: Option<serde_json::Value>,
) -> anyhow::Result<String>
where
    M: rig::completion::CompletionModel + Clone + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    use crate::agent::recovery::{RecoveryPolicy, run_with_retry};
    let policy = RecoveryPolicy::default();
    // Own the preamble so each retry clone moves into the 'static stream
    // future — the caller may now pass a non-'static &str (e.g. from an
    // Arc<str> baked into a judge closure).
    let preamble = preamble.to_string();
    let prompt = prompt.into_inner();

    // The attempt/classify/backoff/sleep loop lives in `run_with_retry`
    // (dirge-6cvc). The closure builds + drains one stream and returns a
    // stream error as `Err(String)` so the helper can classify it; an
    // empty-but-clean response is returned as `Ok(String::new())` and
    // rejected (non-retryable) below.
    let response = run_with_retry(&policy, label, || {
        let model = model.clone();
        let prompt = prompt.clone();
        let preamble = preamble.clone();
        let reasoning_disable = reasoning_disable.clone();
        async move {
            // dirge-zt8p: turn off the model's extended-reasoning trace for this
            // one-shot — summarize/critic/approval don't benefit from it, and on
            // reasoning-by-default models it ~doubles latency. rig forwards an
            // agent's `additional_params` into the request.
            let mut builder = rig::agent::AgentBuilder::new(model).preamble(&preamble);
            if let Some(params) = reasoning_disable {
                builder = builder.additional_params(params);
            }
            let agent = builder.build();

            let mut stream = agent
                .stream_chat(prompt, Vec::<rig::completion::Message>::new())
                .max_turns(1)
                .await;

            let mut response = String::new();
            use futures::StreamExt;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                        rig::streaming::StreamedAssistantContent::Text(text),
                    )) => response.push_str(&text.text),
                    Ok(rig::agent::MultiTurnStreamItem::FinalResponse(res)) => {
                        return Ok(res.output.clone());
                    }
                    Err(e) => return Err(e.to_string()),
                    _ => {}
                }
            }
            Ok(response)
        }
    })
    .await
    .map_err(|msg| anyhow::anyhow!("one-shot LLM call failed: {msg}"))?;

    if response.is_empty() {
        anyhow::bail!("one-shot LLM call returned empty response");
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{
        ONESHOT_FALLBACK_BUDGET_BYTES, head_tail_truncate, oneshot_prompt_budget_bytes,
        reasoning_disable_for_kind,
    };

    /// Bytes of conversation a post-response fold hands the summarizer for a
    /// model with `window` tokens: the fold fires just past
    /// `HISTORY_FOLD_THRESHOLD` and the cut keeps `keep_recent_tokens`
    /// (default 20_000) as a verbatim tail, so everything before that goes to
    /// the summarizer at roughly `CHARS_PER_TOKEN` bytes each.
    fn fold_handoff_bytes(window: u64) -> usize {
        use crate::agent::agent_loop::context_manager::HISTORY_FOLD_THRESHOLD;
        let at_fold = (window as f64 * HISTORY_FOLD_THRESHOLD) as u64;
        (at_fold.saturating_sub(20_000) * 4) as usize
    }

    /// dirge-5zca, the TOO-SMALL direction. A fixed 128 KB budget drops the
    /// middle of the conversation on every model whose window is over about
    /// 70k tokens — which is nearly all of them — and does it silently, while
    /// `dispatch.rs`'s C6 comment says the full prefix reaches the summarizer.
    #[test]
    fn the_budget_clears_what_a_fold_hands_over() {
        for (model, window) in [
            ("gpt-4o", 128_000u64),
            ("glm-4.6", 200_000),
            ("claude-sonnet-4-6", 1_000_000),
        ] {
            let fed = fold_handoff_bytes(window);
            let budget = oneshot_prompt_budget_bytes(model);
            assert!(
                budget >= fed,
                "{model} ({window} tokens): a fold hands over {fed} bytes but the \
                 summarizer prompt budget is {budget} — {} bytes are dropped before \
                 the summarizer sees them",
                fed - budget,
            );
        }
    }

    /// dirge-5zca, the TOO-LARGE direction, and the reason this is a wrong
    /// number rather than a tuning question. `llama-3` is 8_000 tokens; the
    /// fixed 128 KB budget is about 32k tokens, four times its window, so the
    /// request 400s on context length — exactly the failure the cap was added
    /// (PROV-9) to prevent.
    #[test]
    fn a_small_window_gets_a_budget_that_fits_in_it() {
        let budget = oneshot_prompt_budget_bytes("llama-3-8b");
        let window_bytes = 8_000 * 4;
        assert!(
            budget < window_bytes,
            "an 8k-token model was handed a {budget}-byte prompt budget, which \
             does not fit its own {window_bytes}-byte window"
        );
        assert!(
            budget < ONESHOT_FALLBACK_BUDGET_BYTES,
            "a small-window model must get LESS than the unknown-model fallback"
        );
    }

    /// The budget has to actually vary with the window, or the two tests above
    /// could both pass against some other fixed number.
    #[test]
    fn the_budget_varies_with_the_window() {
        let small = oneshot_prompt_budget_bytes("llama-3-8b");
        let mid = oneshot_prompt_budget_bytes("gpt-4o");
        let big = oneshot_prompt_budget_bytes("claude-sonnet-4-6");
        assert!(
            small < mid && mid < big,
            "budget must track the window: 8k={small} 128k={mid} 1M={big}"
        );
    }

    /// The seam the bug actually lived in: having a correct budget means
    /// nothing unless the dispatcher applies it. `run_oneshot` now takes a
    /// `BudgetedPrompt` so skipping it cannot compile, and this pins the
    /// behaviour that type carries.
    #[test]
    fn the_budgeted_prompt_enforces_the_budget() {
        use super::budgeted::BudgetedPrompt;

        // Well under an 8k model's budget: through untouched.
        let small = "line\n".repeat(100);
        let out = BudgetedPrompt::new("summarizer", "llama-3-8b", small.clone());
        assert_eq!(
            out.as_str(),
            small,
            "a prompt within budget must not change"
        );

        // Well over it: cut to the budget, with the marker that says so.
        let budget = oneshot_prompt_budget_bytes("llama-3-8b");
        let big = "line\n".repeat(budget); // 5x the budget
        let out = BudgetedPrompt::new("summarizer", "llama-3-8b", big.clone());
        assert!(
            out.as_str().len() < big.len(),
            "an over-budget prompt must be truncated"
        );
        assert!(
            out.as_str()
                .contains("truncated by summarizer-prompt budget"),
            "the truncation must be marked where the model can see it"
        );
    }

    /// An unrecognised model keeps today's conservative fixed number — the one
    /// case where a guess is all that is available.
    #[test]
    fn an_unknown_model_falls_back_to_the_fixed_budget() {
        assert_eq!(
            oneshot_prompt_budget_bytes("totally-unknown-model-9000"),
            ONESHOT_FALLBACK_BUDGET_BYTES
        );
    }

    #[test]
    fn head_tail_truncate_short_prompt_passes_through() {
        let s = "line 1\nline 2\nline 3";
        assert_eq!(head_tail_truncate(s, 1024), s);
    }

    #[test]
    fn reasoning_disable_shapes_per_provider() {
        // Hosted DeepSeek / GLM / OpenCode: thinking:{type:"disabled"} toggle.
        for kind in ["deepseek", "glm", "opencode"] {
            assert_eq!(
                reasoning_disable_for_kind(kind, None),
                Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
                "{kind} should disable thinking via thinking:{{type:disabled}}",
            );
        }
        // Self-hosted vLLM / SGLang backends: chat_template_kwargs convention.
        for kind in ["custom", "openrouter"] {
            assert_eq!(
                reasoning_disable_for_kind(kind, None),
                Some(serde_json::json!({ "chat_template_kwargs": { "thinking": false } })),
                "{kind} should disable thinking via chat_template_kwargs",
            );
        }
        assert_eq!(
            reasoning_disable_for_kind("ollama", None),
            Some(serde_json::json!({ "think": false })),
        );
        // GH #832: nested where rig's Gemini provider reads it. An
        // unclassifiable id keeps the 2.5 budget wire.
        assert_eq!(
            reasoning_disable_for_kind("gemini", None),
            Some(
                serde_json::json!({ "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } } })
            ),
        );
        assert_eq!(
            reasoning_disable_for_kind("gemini", Some("gemini-2.5-flash")),
            Some(
                serde_json::json!({ "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } } })
            ),
        );
        // Gemini 3 cannot be fully turned off, so a one-shot sends no knob
        // rather than a level pretending to be one.
        assert_eq!(
            reasoning_disable_for_kind("gemini", Some("gemini-3.6-flash")),
            None,
        );
        // Anthropic defaults to no thinking; OpenAI has no safe "off" — both left
        // untouched.
        assert_eq!(reasoning_disable_for_kind("anthropic", None), None);
        assert_eq!(reasoning_disable_for_kind("openai", None), None);
        assert_eq!(reasoning_disable_for_kind("cerebras", None), None);
    }

    #[test]
    fn head_tail_truncate_keeps_head_and_tail() {
        let mut s = String::new();
        for i in 0..2000 {
            s.push_str(&format!("line {}\n", i));
        }
        let out = head_tail_truncate(&s, 4096);
        assert!(out.len() < s.len(), "output should be shorter");
        assert!(out.starts_with("line 0\n"));
        assert!(out.contains("truncated by summarizer-prompt budget"));
        assert!(out.ends_with("line 1999\n"));
    }
}
