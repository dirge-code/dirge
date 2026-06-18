//! One place the provider list lives for stream-fn construction
//! (dirge-iy20).
//!
//! `AnyAgent::build_stream_fn_with_filter` (matching `AnyAgentInner`)
//! and `AnyModel::build_stream_fn` (matching `AnyModel`) both
//! enumerate the same eight providers and call the same
//! `rig_stream_fn_from_model_with_filter` helper. They were two
//! parallel 8-arm matches: adding a provider meant editing both, and
//! the compiler couldn't catch a missed arm (each match was already
//! exhaustive over its own enum). This macro is the single list.

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
        filter = $filter:expr $(,)?
    ) => {{
        use $crate::agent::agent_loop::rig_stream_fn_from_model_with_filter as __stream_fn;
        match $value {
            $enum::OpenRouter($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::OpenAI($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::ChatGptOpenAI($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $filter)
            }
            $enum::OpenAICodex($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::Anthropic($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::AnthropicOauth($bind) => {
                __stream_fn($model, $tools, $timeout, $provider, $filter)
            }
            $enum::Gemini($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::DeepSeek($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::Glm($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::Ollama($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
            $enum::Custom($bind) => __stream_fn($model, $tools, $timeout, $provider, $filter),
        }
    }};
}

pub(crate) use dispatch_stream_fn;
