use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use crate::agent::agent_loop::message::{DeltaPhase, StreamEvent};
use crate::agent::agent_loop::stream::StreamFn;
use crate::permission::ask::{AskRequest, AskSender, UserDecision};

const BILLING_FALLBACK_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BillingFallbackRequest {
    pub(crate) error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BillingFallbackDecision {
    UseApiKey,
    Decline,
    Unavailable(String),
}

pub(crate) type BillingFallbackPrompt = Arc<
    dyn Fn(BillingFallbackRequest) -> Pin<Box<dyn Future<Output = BillingFallbackDecision> + Send>>
        + Send
        + Sync,
>;

pub(crate) fn prompt_from_ask_sender(ask_tx: Option<AskSender>) -> BillingFallbackPrompt {
    Arc::new(move |request| {
        let ask_tx = ask_tx.clone();
        Box::pin(async move {
            let Some(ask_tx) = ask_tx else {
                return BillingFallbackDecision::Unavailable(
                    "OpenAI subscription quota/model access appears exhausted, but API-key billing fallback requires interactive confirmation. Re-run interactively to approve OpenAI API charges, or wait for subscription quota to recover."
                        .to_string(),
                );
            };
            let (reply, decision) = tokio::sync::oneshot::channel();
            let input = format!(
                "OpenAI subscription quota/model access appears exhausted. Switch this request to OpenAI API-key billing? This may incur OpenAI API charges. Original error: {}",
                request.error,
            );
            let send = tokio::time::timeout(
                BILLING_FALLBACK_CONFIRMATION_TIMEOUT,
                ask_tx.send(AskRequest {
                    tool: "openai_api_billing".to_string(),
                    input,
                    reason: None,
                    reply,
                }),
            )
            .await;
            let Ok(send) = send else {
                return BillingFallbackDecision::Unavailable(
                    "OpenAI API-key billing fallback requires interactive confirmation, but the confirmation request was not delivered in time."
                        .to_string(),
                );
            };
            if send.is_err() {
                return BillingFallbackDecision::Unavailable(
                    "OpenAI API-key billing fallback requires interactive confirmation, but the confirmation channel is unavailable."
                        .to_string(),
                );
            }
            match tokio::time::timeout(BILLING_FALLBACK_CONFIRMATION_TIMEOUT, decision).await {
                Ok(Ok(UserDecision::AllowOnce | UserDecision::AllowAlways(_))) => {
                    BillingFallbackDecision::UseApiKey
                }
                Ok(Ok(UserDecision::Deny)) => BillingFallbackDecision::Decline,
                Ok(Err(_)) => BillingFallbackDecision::Unavailable(
                    "OpenAI API-key billing fallback requires interactive confirmation, but the confirmation was cancelled."
                        .to_string(),
                ),
                Err(_) => BillingFallbackDecision::Unavailable(
                    "OpenAI API-key billing fallback requires interactive confirmation, but the confirmation was not answered in time."
                        .to_string(),
                ),
            }
        })
    })
}

pub(crate) fn with_openai_api_billing_fallback(
    primary: StreamFn,
    fallback: StreamFn,
    prompt: BillingFallbackPrompt,
) -> StreamFn {
    Arc::new(move |ctx, opts| {
        let primary = primary.clone();
        let fallback = fallback.clone();
        let prompt = prompt.clone();
        Box::pin(async_stream::stream! {
            let mut committed = false;
            let mut stream = primary(ctx.clone(), opts.clone());
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Error { error }
                        if !committed && is_openai_subscription_exhausted_error(&error) =>
                    {
                        let decision = prompt(BillingFallbackRequest { error: error.clone() }).await;
                        match decision {
                            BillingFallbackDecision::UseApiKey => {
                                yield StreamEvent::Retry {
                                    attempt: 1,
                                    delay_ms: 0,
                                    error: "OpenAI subscription quota/model access appears exhausted; switching this request to confirmed API-key billing fallback".to_string(),
                                };
                                let mut fallback_stream = fallback(ctx.clone(), opts.clone());
                                while let Some(fallback_event) = fallback_stream.next().await {
                                    yield fallback_event;
                                }
                                return;
                            }
                            BillingFallbackDecision::Decline => {
                                yield StreamEvent::Error {
                                    error: format!(
                                        "OpenAI subscription quota/model access appears exhausted; API-key billing fallback was not approved. Original error: {error}"
                                    ),
                                };
                                return;
                            }
                            BillingFallbackDecision::Unavailable(message) => {
                                yield StreamEvent::Error { error: message };
                                return;
                            }
                        }
                    }
                    StreamEvent::Delta { partial, phase } => {
                        if is_content_delta(phase) {
                            committed = true;
                        }
                        yield StreamEvent::Delta { partial, phase };
                    }
                    other => yield other,
                }
            }
        })
    })
}

pub(crate) fn is_openai_subscription_exhausted_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not supported") || lower.contains("unsupported") {
        return false;
    }
    if lower.contains("rate limit") {
        return false;
    }
    if model_access_is_plan_limited(&lower) {
        return true;
    }
    if lower.contains("usage limit")
        || lower.contains("insufficient_quota")
        || lower.contains("billing_not_active")
        || lower.contains("billing_hard_limit_reached")
        || lower.contains("exceeded your current quota")
        || lower.contains("quota_exceeded")
        || lower.contains("quota exceeded")
        || lower.contains("quota exhausted")
        || lower.contains("plan limit")
        || lower.contains("monthly limit")
    {
        return true;
    }
    if lower.contains("limit reached") {
        return lower.contains("subscription")
            || lower.contains("quota")
            || lower.contains("billing")
            || lower.contains("plan")
            || lower.contains("monthly")
            || lower.contains("usage");
    }
    lower.contains("subscription")
        && (lower.contains("limit") || lower.contains("quota") || lower.contains("exhausted"))
}

fn model_access_is_plan_limited(lower: &str) -> bool {
    if !lower.contains("model") {
        return false;
    }
    let access_denied = lower.contains("do not have access")
        || lower.contains("don't have access")
        || lower.contains("not available")
        || lower.contains("does not include");
    let plan_scoped = lower.contains("plan")
        || lower.contains("subscription")
        || lower.contains("account")
        || lower.contains("workspace");
    access_denied && plan_scoped
}

fn is_content_delta(phase: DeltaPhase) -> bool {
    matches!(
        phase,
        DeltaPhase::TextStart
            | DeltaPhase::TextDelta
            | DeltaPhase::ThinkingStart
            | DeltaPhase::ThinkingDelta
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{AssistantMessage, StopReason, StreamEvent};
    use crate::agent::agent_loop::retrying_stream_fn_with_non_retryable;
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn, StreamOptions};
    use crate::agent::agent_loop::tool::AbortSignal;
    use crate::agent::recovery::RecoveryPolicy;
    use crate::permission::ask::{AskRequest, UserDecision};
    use futures::StreamExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    fn ctx() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        }
    }

    fn empty_assistant() -> AssistantMessage {
        AssistantMessage::new(Vec::new(), StopReason::Stop)
    }

    fn canned(events: Vec<StreamEvent>, calls: Arc<AtomicUsize>) -> StreamFn {
        let events = Arc::new(events);
        Arc::new(move |_ctx, _opts| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(futures::stream::iter((*events).clone()))
        })
    }

    async fn drain(stream_fn: StreamFn) -> Vec<StreamEvent> {
        let mut stream = stream_fn(ctx(), StreamOptions::from_signal(AbortSignal::new()));
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn approved_quota_exhaustion_switches_to_api_key_fallback_stream() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let prompted = Arc::new(AtomicBool::new(false));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429 Too Many Requests: usage limit reached for your ChatGPT subscription"
                    .to_string(),
            }],
            primary_calls.clone(),
        );
        let fallback = canned(
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: empty_assistant(),
                usage: None,
            }],
            fallback_calls.clone(),
        );
        let prompt: BillingFallbackPrompt = Arc::new({
            let prompted = prompted.clone();
            move |request| {
                assert!(request.error.contains("usage limit"));
                prompted.store(true, Ordering::SeqCst);
                Box::pin(async { BillingFallbackDecision::UseApiKey })
            }
        });

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert!(prompted.load(Ordering::SeqCst));
        assert!(matches!(events[0], StreamEvent::Retry { delay_ms: 0, .. }));
        assert!(matches!(events[1], StreamEvent::Done { .. }));
    }

    #[tokio::test]
    async fn declined_quota_exhaustion_does_not_call_fallback_stream() {
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429: exceeded your current quota".to_string(),
            }],
            Arc::new(AtomicUsize::new(0)),
        );
        let fallback = canned(Vec::new(), fallback_calls.clone());
        let prompt: BillingFallbackPrompt =
            Arc::new(|_| Box::pin(async { BillingFallbackDecision::Decline }));

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
        let StreamEvent::Error { error } = &events[0] else {
            panic!("declined fallback must surface an error");
        };
        assert!(error.contains("API-key billing fallback was not approved"));
    }

    #[tokio::test]
    async fn generic_rate_limit_does_not_prompt_or_fallback() {
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429 Too Many Requests: rate limit exceeded, retry later".to_string(),
            }],
            Arc::new(AtomicUsize::new(0)),
        );
        let fallback = canned(Vec::new(), fallback_calls.clone());
        let prompt: BillingFallbackPrompt = Arc::new(|_| {
            panic!("generic rate limits must not prompt for API-key billing fallback")
        });

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    #[tokio::test]
    async fn rate_limit_reached_does_not_prompt_or_fallback() {
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429 Too Many Requests: rate limit reached, retry later".to_string(),
            }],
            Arc::new(AtomicUsize::new(0)),
        );
        let fallback = canned(Vec::new(), fallback_calls.clone());
        let prompt: BillingFallbackPrompt = Arc::new(|_| {
            panic!("generic rate limits must not prompt for API-key billing fallback")
        });

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    #[tokio::test]
    async fn request_limit_reached_does_not_prompt_or_fallback() {
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429 Too Many Requests: request limit reached, retry later".to_string(),
            }],
            Arc::new(AtomicUsize::new(0)),
        );
        let fallback = canned(Vec::new(), fallback_calls.clone());
        let prompt: BillingFallbackPrompt = Arc::new(|_| {
            panic!("generic request limits must not prompt for API-key billing fallback")
        });

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    #[tokio::test]
    async fn subscription_exhaustion_prompts_before_retry_budget() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let prompted = Arc::new(AtomicBool::new(false));
        let primary = canned(
            vec![StreamEvent::Error {
                error: "429 Too Many Requests: usage limit reached for your ChatGPT subscription"
                    .to_string(),
            }],
            primary_calls.clone(),
        );
        let fallback = canned(
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: empty_assistant(),
                usage: None,
            }],
            fallback_calls.clone(),
        );
        let primary = retrying_stream_fn_with_non_retryable(
            primary,
            RecoveryPolicy::with_backoff(5, Duration::ZERO),
            Arc::new(is_openai_subscription_exhausted_error),
        );
        let prompt: BillingFallbackPrompt = Arc::new({
            let prompted = prompted.clone();
            move |_| {
                prompted.store(true, Ordering::SeqCst);
                Box::pin(async { BillingFallbackDecision::UseApiKey })
            }
        });

        let events = drain(with_openai_api_billing_fallback(primary, fallback, prompt)).await;

        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert!(prompted.load(Ordering::SeqCst));
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], StreamEvent::Retry { delay_ms: 0, .. }));
        assert!(matches!(events[1], StreamEvent::Done { .. }));
    }

    #[test]
    fn detector_does_not_match_unsupported_model_errors() {
        assert!(!is_openai_subscription_exhausted_error(
            "The 'gpt-5.3-codex' model is not supported when using Codex with a ChatGPT account."
        ));
    }

    #[test]
    fn detector_matches_plan_model_access_errors() {
        assert!(is_openai_subscription_exhausted_error(
            "You do not have access to this model on your current plan."
        ));
        assert!(is_openai_subscription_exhausted_error(
            "This model is not available on your plan."
        ));
    }

    #[tokio::test]
    async fn prompt_from_missing_ask_channel_reports_non_interactive_error() {
        let prompt = prompt_from_ask_sender(None);

        let decision = prompt(BillingFallbackRequest {
            error: "429: usage limit reached".to_string(),
        })
        .await;

        let BillingFallbackDecision::Unavailable(message) = decision else {
            panic!("missing ask channel must not approve API-key billing fallback");
        };
        assert!(message.contains("requires interactive confirmation"));
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_from_unanswered_ask_channel_times_out() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let prompt = prompt_from_ask_sender(Some(tx));
        let task = tokio::spawn(async move {
            prompt(BillingFallbackRequest {
                error: "429: usage limit reached".to_string(),
            })
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;

        let decision = tokio::time::timeout(Duration::from_millis(1), task)
            .await
            .expect("unanswered ask channel must time out instead of hanging")
            .unwrap();
        let BillingFallbackDecision::Unavailable(message) = decision else {
            panic!("unanswered ask channel must not hang or approve API-key billing fallback");
        };
        assert!(message.contains("confirmation was not answered"));
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_from_full_undrained_ask_channel_times_out() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (reply, _decision) = tokio::sync::oneshot::channel();
        tx.try_send(AskRequest {
            tool: "already_queued".to_string(),
            input: "pending".to_string(),
            reason: None,
            reply,
        })
        .unwrap();
        let prompt = prompt_from_ask_sender(Some(tx));
        let task = tokio::spawn(async move {
            prompt(BillingFallbackRequest {
                error: "429: usage limit reached".to_string(),
            })
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;

        let decision = tokio::time::timeout(Duration::from_millis(1), task)
            .await
            .expect("full ask channel must time out instead of hanging")
            .unwrap();
        let BillingFallbackDecision::Unavailable(message) = decision else {
            panic!("full ask channel must not hang or approve API-key billing fallback");
        };
        assert!(message.contains("confirmation request was not delivered"));
    }

    #[tokio::test]
    async fn prompt_from_ask_channel_approves_only_after_user_allows() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let prompt = prompt_from_ask_sender(Some(tx));
        let task = tokio::spawn(async move {
            prompt(BillingFallbackRequest {
                error: "429: usage limit reached".to_string(),
            })
            .await
        });

        let req = rx.recv().await.unwrap();
        assert_eq!(req.tool, "openai_api_billing");
        assert!(req.input.contains("may incur OpenAI API charges"));
        assert!(req.input.contains("usage limit"));
        req.reply.send(UserDecision::AllowOnce).unwrap();

        assert_eq!(task.await.unwrap(), BillingFallbackDecision::UseApiKey);
    }
}
