//! Concrete `rig` client/model dispatch enums.
//!
//! Split out of `provider/mod.rs` (dirge-4y4l): [`AnyClient`] and
//! [`AnyModel`] erase the per-provider `rig` client/model types behind
//! a single enum so the rest of the codebase dispatches uniformly. The
//! agent-building wiring that constructs these lives in the parent
//! module; here we only hold the enums plus the operations that fan out
//! over their variants (model construction, one-shot prompts, stream-fn
//! building, conversation compaction).

use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::{anthropic, chatgpt, gemini, ollama, openai, openrouter};

use crate::agent::prompt;
use crate::session::SessionMessage;

use super::anthropic_http::AnthropicHttpClient;
use super::codex_http::CodexHttpClient;
use super::summarize;

const OPENAI_CODEX_OAUTH_DEFAULT_MODEL: &str = "gpt-5.5";

pub enum AnyClient {
    OpenRouter(openrouter::Client),
    OpenAI(openai::CompletionsClient),
    ChatGptOpenAI(openai::Client<CodexHttpClient>),
    OpenAICodex(chatgpt::Client),
    Anthropic(anthropic::Client),
    AnthropicOauth(anthropic::Client<AnthropicHttpClient>),
    Gemini(gemini::Client),
    DeepSeek(openai::CompletionsClient),
    Glm(openai::CompletionsClient),
    OpenCode(openai::CompletionsClient),
    Ollama(ollama::Client),
    Custom(openai::CompletionsClient),
}

impl AnyClient {
    /// Whether this client speaks the ChatGPT/Codex subscription backend.
    /// Only these two variants map the OpenAI default model id to the Codex
    /// subscription default (dirge-ovjk).
    pub(crate) fn is_codex(&self) -> bool {
        matches!(
            self,
            AnyClient::ChatGptOpenAI(_) | AnyClient::OpenAICodex(_)
        )
    }

    pub fn completion_model(&self, name: impl Into<String>) -> AnyModel {
        let name = name.into();
        match self {
            AnyClient::OpenRouter(c) => AnyModel::OpenRouter(c.completion_model(name)),
            AnyClient::OpenAI(c) => AnyModel::OpenAI(c.completion_model(name)),
            // The Codex subscription default is resolved upstream by
            // `resolve_model_name` (dirge-ovjk) and stored as the session's
            // model, so `name` already carries the right id here — no remap.
            AnyClient::ChatGptOpenAI(c) => AnyModel::ChatGptOpenAI(c.completion_model(name)),
            AnyClient::OpenAICodex(c) => AnyModel::OpenAICodex(c.completion_model(name)),
            AnyClient::Anthropic(c) => AnyModel::Anthropic(c.completion_model(name)),
            AnyClient::AnthropicOauth(c) => AnyModel::AnthropicOauth(c.completion_model(name)),
            AnyClient::Gemini(c) => AnyModel::Gemini(c.completion_model(name)),
            AnyClient::DeepSeek(c) => AnyModel::DeepSeek(c.completion_model(name)),
            AnyClient::Glm(c) => AnyModel::Glm(c.completion_model(name)),
            AnyClient::OpenCode(c) => AnyModel::OpenCode(c.completion_model(name)),
            AnyClient::Ollama(c) => AnyModel::Ollama(c.completion_model(name)),
            AnyClient::Custom(c) => AnyModel::Custom(c.completion_model(name)),
        }
    }
}

/// dirge-tv3p: build the compaction summarizer prompt from the to-be-discarded
/// messages. Pure + synchronous (serialize the conversation, assemble the
/// prompt, run the prompt-injection delimiter check) so the UI thread can do it
/// before handing the slow LLM call to a background task. Returns `Err` when the
/// untrusted inputs smuggle the reserved delimiter (caller skips compaction).
pub(crate) fn build_compaction_prompt(
    messages: &[SessionMessage],
    previous_summary: Option<&str>,
    instructions: Option<&str>,
) -> anyhow::Result<String> {
    // C6 (audit fix): no more 6000-char truncation. A 300K-token session was
    // previously summarized from ~1500 tokens of content — fidelity collapsed
    // exactly when compaction was most needed. Feed the full prefix; the
    // summarizer model has plenty of room unless the prefix itself is bigger
    // than its window, in which case its own context-overflow path surfaces a
    // real error rather than silently lying. Pi and opencode feed the full prefix.
    let conversation = summarize::serialize_conversation(messages);

    // `/compress <focus>` argument: free-form text after the slash command is a
    // Hermes-style FOCUS TOPIC — the summarizer allocates ~60-70% of its budget
    // to topic-related information. Maps context_compressor.py:1050-1054.
    let instructions_block = match instructions {
        Some(text) if !text.trim().is_empty() => format!(
            "FOCUS TOPIC: \"{}\"\n\
             The user has requested that this compaction PRIORITISE preserving \
             all information related to the focus topic above. For content \
             related to \"{}\", include full detail — exact values, file paths, \
             command outputs, error messages, and decisions. For content NOT \
             related to the focus topic, summarise more aggressively. The \
             focus topic sections should receive roughly 60-70% of the \
             summary token budget. Even for the focus topic, NEVER preserve \
             API keys, tokens, passwords, or credentials — use [REDACTED].",
            text.trim(),
            text.trim(),
        ),
        _ => "(none)".to_string(),
    };

    // dirge-u13u: prompt-injection defense. Before fencing the untrusted inputs
    // with our distinctive delimiter pair, scan them for the delimiter itself —
    // a smuggled delimiter (via a prior tool output, fetched URL, paste) could
    // close our fence and inject instructions outside it. Bail rather than risk
    // it; the warning stays operator-side (tracing). The caller treats this
    // `Err` as "skip compaction for this turn".
    let prev_summary_value = previous_summary.unwrap_or("(none)");
    if prompt::input_contains_compaction_delimiter(&[
        &conversation,
        prev_summary_value,
        &instructions_block,
    ]) {
        tracing::warn!(
            "compaction input contains the untrusted-material delimiter — \
             skipping compaction this turn to avoid prompt-injection risk"
        );
        anyhow::bail!("compaction aborted: input contains reserved delimiter string");
    }

    Ok(prompt::COMPACTION_PROMPT
        .replace("{conversation}", &conversation)
        .replace("{previous_summary}", prev_summary_value)
        .replace("{instructions}", &instructions_block))
}

/// dirge-tv3p: run the compaction summarizer LLM over a prebuilt prompt and
/// strip any reserved delimiters the model echoed. This is the SLOW half — the
/// event loop runs it on a spawned task so the UI stays responsive. A stray
/// delimiter in the output would corrupt the next turn's system prompt (where
/// the summary is injected), so strip before returning.
pub(crate) async fn run_compaction(model: AnyModel, prompt_text: String) -> anyhow::Result<String> {
    let response = summarize::summarize_with_model(model, prompt_text).await?;
    Ok(prompt::strip_compaction_delimiters(&response))
}

/// Resolve the effective model name for `client`, substituting the Codex
/// subscription default for the OpenAI default id — but ONLY when the model
/// was not explicitly chosen (dirge-ovjk).
///
/// This is the single place that knows an explicit `gpt-4o` (a user who
/// typed `--model gpt-4o` / `/model gpt-4o`) from a *defaulted* `gpt-4o` (no
/// model set, so the OpenAI default was filled in). The old `codex_model_name`
/// remap ran inside `completion_model`, downstream of that distinction, so it
/// rewrote an explicit `gpt-4o` to `gpt-5.5` with no way to tell the two
/// apart. Resolving here, where `explicit` is still known, and storing the
/// result as the session's model gives every `completion_model` call site a
/// single already-correct name to pass through.
pub(crate) fn resolve_model_name(client: &AnyClient, requested: &str, explicit: bool) -> String {
    resolve_codex_default(client.is_codex(), requested, explicit)
}

/// Pure core of [`resolve_model_name`]: the Codex default applies only to a
/// non-explicit OpenAI default id on a Codex client; everything else is the
/// requested name verbatim.
fn resolve_codex_default(is_codex: bool, requested: &str, explicit: bool) -> String {
    if !explicit && is_codex && requested == super::default_model_for("openai") {
        OPENAI_CODEX_OAUTH_DEFAULT_MODEL.to_string()
    } else {
        requested.to_string()
    }
}

/// Decide the effective startup model name and its explicitness, then resolve
/// the Codex default (dirge-ovjk follow-up). Consolidates the fresh / resume /
/// `--model`-override cases:
///   - resuming WITHOUT a `--model` override honors the session's saved
///     `(model, explicit)` — so an explicit `gpt-4o` under Codex survives a
///     resume, while a pre-fix session saved as the OpenAI default (its
///     `explicit` deserializes to `false`) still maps to the Codex default;
///   - a fresh start, or a `--model` override on resume, uses the
///     CLI/config-resolved `(requested, requested_explicit)`.
///
/// Returns `(resolved_name, explicit)`. The caller stores both on the session,
/// so the effective name drives the startup agent build AND the persisted
/// `explicit` flag lets the next resume repeat the decision faithfully.
pub(crate) fn resolve_startup_model(
    client: &AnyClient,
    requested: &str,
    requested_explicit: bool,
    cli_model_override: bool,
    resumed_session: Option<(&str, bool)>,
) -> (String, bool) {
    resolve_startup_model_for(
        client.is_codex(),
        requested,
        requested_explicit,
        cli_model_override,
        resumed_session,
    )
}

/// Pure core of [`resolve_startup_model`].
fn resolve_startup_model_for(
    is_codex: bool,
    requested: &str,
    requested_explicit: bool,
    cli_model_override: bool,
    resumed_session: Option<(&str, bool)>,
) -> (String, bool) {
    let (name, explicit) = match resumed_session {
        Some((saved, saved_explicit)) if !cli_model_override => (saved, saved_explicit),
        _ => (requested, requested_explicit),
    };
    (resolve_codex_default(is_codex, name, explicit), explicit)
}

#[cfg(test)]
mod resolve_model_name_tests {
    use super::*;

    // The whole remap hinges on this identity — guard the premise so a change
    // to the OpenAI default can't silently make the matrix below vacuous.
    #[test]
    fn openai_default_is_the_id_that_gets_remapped() {
        assert_eq!(super::super::default_model_for("openai"), "gpt-4o");
    }

    #[test]
    fn defaulted_openai_id_under_codex_becomes_the_codex_default() {
        assert_eq!(
            resolve_codex_default(true, "gpt-4o", false),
            OPENAI_CODEX_OAUTH_DEFAULT_MODEL
        );
    }

    #[test]
    fn explicit_gpt_4o_under_codex_is_preserved() {
        // dirge-ovjk: the bug. An explicit choice must never be rewritten,
        // even when it happens to equal the OpenAI default id.
        assert_eq!(resolve_codex_default(true, "gpt-4o", true), "gpt-4o");
    }

    #[test]
    fn non_default_model_under_codex_is_verbatim_either_way() {
        assert_eq!(resolve_codex_default(true, "o3", false), "o3");
        assert_eq!(resolve_codex_default(true, "o3", true), "o3");
    }

    #[test]
    fn non_codex_clients_never_remap() {
        assert_eq!(resolve_codex_default(false, "gpt-4o", false), "gpt-4o");
        assert_eq!(resolve_codex_default(false, "gpt-4o", true), "gpt-4o");
        assert_eq!(resolve_codex_default(false, "o3", false), "o3");
    }

    // --- startup / resume resolution (dirge-ovjk follow-ups) -------------

    #[test]
    fn fresh_start_resolves_from_the_cli_config_model() {
        // No resumed session: use (requested, requested_explicit).
        assert_eq!(
            resolve_startup_model_for(true, "gpt-4o", false, false, None),
            ("gpt-5.5".to_string(), false)
        );
        assert_eq!(
            resolve_startup_model_for(true, "gpt-4o", true, false, None),
            ("gpt-4o".to_string(), true)
        );
    }

    #[test]
    fn resume_honors_an_explicit_saved_model_under_codex() {
        // Follow-up #1: a session saved with an explicit gpt-4o keeps it on a
        // plain resume — the shim must not revert the choice to gpt-5.5.
        assert_eq!(
            resolve_startup_model_for(true, "gpt-4o", false, false, Some(("gpt-4o", true))),
            ("gpt-4o".to_string(), true)
        );
    }

    #[test]
    fn resume_still_shims_a_pre_fix_default_codex_session() {
        // A pre-fix session saved the unresolved OpenAI default; its explicit
        // flag deserializes to false, so it still maps to the Codex default.
        assert_eq!(
            resolve_startup_model_for(true, "gpt-4o", false, false, Some(("gpt-4o", false))),
            ("gpt-5.5".to_string(), false)
        );
    }

    #[test]
    fn resume_honors_a_saved_non_default_model() {
        // Follow-up #2: resuming without --model uses the session's own model,
        // not the CLI/config default (`requested` here is a different id).
        assert_eq!(
            resolve_startup_model_for(false, "gpt-4o", false, false, Some(("o3", true))),
            ("o3".to_string(), true)
        );
    }

    #[test]
    fn model_flag_overrides_the_saved_model_on_resume() {
        // A `--model` override this invocation wins over the saved session.
        assert_eq!(
            resolve_startup_model_for(true, "o3", true, true, Some(("gpt-4o", true))),
            ("o3".to_string(), true)
        );
    }
}

#[derive(Clone)]
pub enum AnyModel {
    OpenRouter(openrouter::completion::CompletionModel),
    OpenAI(openai::completion::CompletionModel),
    ChatGptOpenAI(openai::responses_api::ResponsesCompletionModel<CodexHttpClient>),
    OpenAICodex(chatgpt::ResponsesCompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    AnthropicOauth(
        anthropic::completion::CompletionModel<super::anthropic_http::AnthropicHttpClient>,
    ),
    Gemini(gemini::completion::CompletionModel),
    DeepSeek(openai::completion::CompletionModel),
    Glm(openai::completion::CompletionModel),
    OpenCode(openai::completion::CompletionModel),
    Ollama(ollama::CompletionModel),
    Custom(openai::completion::CompletionModel),
}

impl AnyModel {
    pub async fn btw_query(&self, prompt: String) -> anyhow::Result<String> {
        self.btw_query_with(prompt, None).await
    }

    /// One-shot, tool-less query with an optional system-prompt override.
    /// `preamble = None` uses the default concise-answer preamble; the `task`
    /// tool passes an agent profile's prompt here so a subagent can run with a
    /// specialized persona (dirge-ykeu Phase 4). Same recovery policy as
    /// `btw_query`.
    pub async fn btw_query_with(
        &self,
        prompt: String,
        preamble: Option<&str>,
    ) -> anyhow::Result<String> {
        let preamble = preamble.unwrap_or("Answer the user's question concisely.");
        // PROV-3: wrap the bare one-shot prompt in the same recovery
        // policy used for the main turn loop. Previously a single
        // 503 from the provider killed every `/btw` and subagent
        // (`task` tool) call with no retry. Network + rate-limit
        // failures now get the standard 3-retry exponential backoff;
        // auth / context-length / other still bail immediately.
        use crate::agent::recovery::{RecoveryPolicy, run_with_retry};
        let policy = RecoveryPolicy::default();
        // The retry/backoff loop lives in `run_with_retry` (dirge-6cvc);
        // the macro only exists to dispatch over `AnyModel`'s concrete
        // per-variant model type (each `$m` has a different type).
        macro_rules! one_shot {
            ($m:expr) => {{
                let m = $m.clone();
                run_with_retry(&policy, "btw_query", || {
                    let agent = rig::agent::AgentBuilder::new(m.clone())
                        .preamble(preamble)
                        .build();
                    let prompt = prompt.clone();
                    async move { agent.prompt(prompt).await }
                })
                .await
                .map_err(anyhow::Error::from)
            }};
        }
        match self {
            AnyModel::OpenRouter(m) => one_shot!(m),
            AnyModel::OpenAI(m) => one_shot!(m),
            AnyModel::ChatGptOpenAI(m) => one_shot!(m),
            AnyModel::OpenAICodex(m) => one_shot!(m),
            AnyModel::Anthropic(m) => one_shot!(m),
            AnyModel::AnthropicOauth(m) => one_shot!(m),
            AnyModel::Gemini(m) => one_shot!(m),
            AnyModel::DeepSeek(m) => one_shot!(m),
            AnyModel::Glm(m) => one_shot!(m),
            AnyModel::OpenCode(m) => one_shot!(m),
            AnyModel::Ollama(m) => one_shot!(m),
            AnyModel::Custom(m) => one_shot!(m),
        }
    }

    /// Phase 4 part 1: build a standalone `StreamFn` from this
    /// model + tool definitions. Used to construct the escalation
    /// route when `ConfigRole::Escalation` resolves to a provider
    /// different from `ConfigRole::Default`. The result is plumbed
    /// into `LoopConfig.escalation_stream_fn` and invoked exactly
    /// once after a repair-exhaustion or tree-sitter failure.
    ///
    /// Tools and chunk timeout are passed in (not extracted) for
    /// symmetry with `AnyAgent::build_stream_fn_with_filter`. The
    /// escalation stream uses the SAME tool definitions as the
    /// default — only the model + provider differ.
    pub fn build_stream_fn(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
        chunk_timeout: std::time::Duration,
        provider_name: Option<String>,
    ) -> crate::agent::agent_loop::StreamFn {
        self.build_stream_fn_with_filter(tools, chunk_timeout, provider_name, None)
    }

    pub fn build_stream_fn_with_filter(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
        chunk_timeout: std::time::Duration,
        provider_name: Option<String>,
        tool_def_filter: Option<
            std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        >,
    ) -> crate::agent::agent_loop::StreamFn {
        // dirge-iy20: single provider list in `stream_dispatch`,
        // shared with `AnyAgent::build_stream_fn_with_filter`.
        crate::provider::stream_dispatch::dispatch_stream_fn! {
            match self;
            AnyModel(m) => m.clone(),
            tools = tools,
            timeout = Some(chunk_timeout),
            provider = provider_name,
            model_name = Some(self.name()),
            filter = tool_def_filter,
        }
    }

    /// Return the model identifier string that was passed when
    /// the model was built (`client.completion_model("…")`).
    /// Forwarded to `LoopConfig.model_name` so the
    /// `tool_input_repair` telemetry can record `(model, tool,
    /// repair_kind)`.
    pub fn name(&self) -> String {
        match self {
            AnyModel::OpenRouter(m) => m.model.clone(),
            AnyModel::OpenAI(m) => m.model.clone(),
            AnyModel::ChatGptOpenAI(m) => m.model.clone(),
            AnyModel::OpenAICodex(m) => m.model.clone(),
            AnyModel::Anthropic(m) => m.model.clone(),
            AnyModel::AnthropicOauth(m) => m.model.clone(),
            AnyModel::Gemini(m) => m.model.clone(),
            AnyModel::DeepSeek(m) => m.model.clone(),
            AnyModel::Glm(m) => m.model.clone(),
            AnyModel::OpenCode(m) => m.model.clone(),
            AnyModel::Ollama(m) => m.model.clone(),
            AnyModel::Custom(m) => m.model.clone(),
        }
    }
}

/// dirge-yai1 — pure-function tool-name filter used by tests to
/// exercise the filter shape `spawn_filtered_runner_with_cache`
/// applies internally. Gated `#[cfg(test)]` because production
/// code uses the inline filter directly.
#[cfg(test)]
pub(crate) fn filter_tool_names<'a>(
    all: impl Iterator<Item = &'a str>,
    allowed: &[&str],
) -> Vec<String> {
    all.filter(|n| allowed.contains(n))
        .map(String::from)
        .collect()
}
