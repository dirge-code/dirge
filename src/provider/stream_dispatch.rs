//! One place the provider list lives for stream-fn construction
//! (dirge-iy20).
//!
//! `AnyAgent::build_stream_fn_with_filter` (matching `AnyAgentInner`)
//! and `AnyModel::build_stream_fn` (matching `AnyModel`) dispatch through
//! this shared provider list and call the same
//! `rig_stream_fn_from_model_with_filter` helper. Adding a provider here
//! updates both paths, while each enum match remains exhaustive.

/// Dispatch over a provider enum to build a `StreamFn`.
///
/// `$value` is matched against `$enum::{OpenRouter,…,Custom}`. Each
/// arm binds `$bind` and evaluates `$model` (written in terms of
/// `$bind`) to get the model to stream from. `tools`/`timeout`/
/// `provider`/`filter` are pasted into every arm — match arms are
/// mutually exclusive, so a moved value (e.g. `tools` without a
/// clone) is fine.
macro_rules! dispatch_stream_fn {
    (
        match $value:expr ;
        $enum:ident ( $bind:ident ) => $model:expr ,
        tools = $tools:expr ,
        timeout = $timeout:expr ,
        provider = $provider:expr ,
        model_name = $model_name:expr ,
        filter = $filter:expr $(,)?
    ) => {{
        use $crate::agent::agent_loop::rig_stream_fn_from_model_with_filter as __stream_fn;
        // Provider-specific wire adapters key off canonical backend names, not
        // configured aliases. OpenAI Responses needs canonical `openai` for
        // reasoning/tool-call ID conversion; Cerebras needs canonical `cerebras`
        // for its top-level reasoning_effort shape. These concrete dispatch arms
        // know the backend even when a role route was configured under an alias.
        // Other providers retain the passed identity until they need the same
        // canonicalization treatment.
        match $value {
            $enum::OpenRouter($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::OpenAI($bind) => __stream_fn(
                $model,
                $tools,
                $timeout,
                Some("openai".to_string()),
                $model_name,
                $filter,
            ),
            $enum::ChatGptOpenAI($bind) => __stream_fn(
                $model,
                $tools,
                $timeout,
                Some("openai".to_string()),
                $model_name,
                $filter,
            ),
            $enum::OpenAICodex($bind) => __stream_fn(
                $model,
                $tools,
                $timeout,
                Some("openai".to_string()),
                $model_name,
                $filter,
            ),
            $enum::Anthropic($bind) => {
                // dirge-607: enable automatic prompt caching (1h TTL) so the
                // stable system-prompt + tool-definition prefix is cached
                // server-side across turns. Without this every turn bills full
                // input tokens even though the prefix never changes.
                __stream_fn($model.with_automatic_caching_1h(), $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::AnthropicOauth($bind) => {
                // dirge-607: same as Anthropic arm above; the OAuth/Claude-Code
                // path is the primary user-facing path and the biggest source of
                // token burn on the Pro 5x plan.
                __stream_fn($model.with_automatic_caching_1h(), $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::Gemini($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::DeepSeek($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::Glm($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::Cerebras($bind) => __stream_fn(
                $model,
                $tools,
                $timeout,
                Some("cerebras".to_string()),
                $model_name,
                $filter,
            ),
            $enum::OpenCode($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::Ollama($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
            $enum::Custom($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $model_name, $filter)
            }
        }
    }};
}

pub(crate) use dispatch_stream_fn;
