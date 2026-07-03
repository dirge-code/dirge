//! Phase 4.5g — recovery / retry wrapper around `StreamFn`.
//!
//! Wraps an inner `StreamFn` so transient errors (Network,
//! RateLimit) automatically retry with exponential backoff +
//! Retry-After parsing. Non-transient errors (Auth,
//! ContextLength, Other) pass through unchanged so the loop can
//! surface them.
//!
//! ## When does retry fire?
//!
//! Only when the inner stream produces an `Error` event BEFORE
//! any committed content has been emitted. "Committed" means the
//! stream yielded a content-bearing event (Token delta, tool call
//! delta, etc.) that the caller can already observe downstream.
//! Retrying after committed content would emit duplicated tokens
//! to the consumer.
//!
//! Pi's existing `runner.rs` uses an equivalent gate
//! (`had_tool_calls`): once side effects have run, retry is
//! unsafe. Our gate is finer — it tracks any actual deltas, not
//! just tool calls — because mid-text retries would also produce
//! visible duplication.
//!
//! ## Backoff
//!
//! Uses `RecoveryPolicy::backoff_duration_for_msg` which combines
//! the policy's exponential schedule with `Retry-After` parsing
//! from the error message. Caps at 5 minutes (per the existing
//! policy). The backoff sleep is raced against `AbortSignal::cancelled`
//! via `tokio::select!`, so a cooperative cancel (signal only, no
//! `JoinHandle::abort`) resolves the stream promptly instead of waiting
//! out the full backoff.
//!
//! ## What this does NOT handle
//!
//! - `ContextOverflow` auto-compact. Pi/dirge's existing runner
//!   emits a distinct `ContextOverflow` AgentEvent for the UI to
//!   handle (run `/compress` + respawn). The new loop emits
//!   `Error` for a context-length error; the bridge could
//!   re-classify and emit `ContextOverflow` instead. Phase 4.5h
//!   wires this when flipping the default.
//! - Mid-tool-result retry. Once a tool has executed, side
//!   effects are permanent; the loop must not re-run them.
//!   `had_tool_calls` is the marker here (gated equivalently to
//!   pi: any tool dispatch → no retry).

use std::sync::Arc;

use futures::stream::StreamExt;

use crate::agent::recovery::{RecoveryPolicy, classify_error};

use super::message::{DeltaPhase, StreamEvent};
use super::stream::StreamFn;

#[cfg(test)]
use super::tool::AbortSignal;

/// Graceful-recovery nudge reinjected (once per run) when a request times out
/// mid-stream. A mid-assembly stream-chunk timeout aborts a stuck/hung stream
/// and triggers a retry, but a blind retry can stall the same way (e.g. the
/// model stuck in a long reasoning loop). Reinjecting a "conclude and act"
/// directive gives it a chance to break out instead of repeating the stall.
const STALL_RECOVERY_NUDGE: &str = "Your previous response did not complete in time — you may have been stuck in a long reasoning loop. Stop deliberating: state your conclusion concisely and take the next concrete action (a tool call, or your final answer) now.";

/// Whether `msg` looks like a stall timeout the nudge can help break
/// out of (request-level timeout, or a mid-assembly stream-chunk
/// timeout where the model was actively producing), vs a generic
/// transient blip (503) — or the plain transport stream-chunk timeout
/// ("provider stalled or connection silently dropped"), which is a
/// connection drop, not a reasoning-loop stall: blaming "a long
/// reasoning loop" there is mis-attributed, so the retry re-runs clean.
fn is_stall_timeout(msg: &str) -> bool {
    let l = msg.to_ascii_lowercase();
    (l.contains("timeout") || l.contains("timed out"))
        && !l.contains("connection silently dropped")
        // A request-establish timeout is a connection/handshake stall, not a
        // reasoning-loop stall — the "conclude now" nudge would misattribute
        // it (same reasoning as the silently-dropped case above, dirge-u44q).
        && !l.contains("establish")
}

/// Wrap an inner `StreamFn` with retry-on-transient-error
/// semantics.
///
/// Algorithm per outer invocation:
///   1. `attempts = 0`
///   2. Call inner stream; iterate events:
///      - non-Error events pass through; track `committed` if
///        the event represents observable content
///      - Error before committed → classify; if retryable AND
///        attempts < max_retries → sleep backoff, increment
///        attempts, restart inner stream
///      - Error after committed → pass through; stop
///      - non-retryable Error → pass through; stop
///   3. Stream ends normally → stop
///
/// Cancellation: `signal.is_cancelled()` is checked at retry
/// boundaries (between attempts). Mid-stream cancellation is
/// the inner stream's responsibility (the rig adapter's
/// `AbortSignal` poll inside its own loop).
pub fn retrying_stream_fn(inner: StreamFn, policy: RecoveryPolicy) -> StreamFn {
    retrying_stream_fn_with_non_retryable(inner, policy, Arc::new(|_| false))
}

pub fn retrying_stream_fn_with_non_retryable(
    inner: StreamFn,
    policy: RecoveryPolicy,
    non_retryable: Arc<dyn Fn(&str) -> bool + Send + Sync>,
) -> StreamFn {
    let policy = Arc::new(policy);
    Arc::new(move |ctx, opts: super::stream::StreamOptions| {
        let inner = inner.clone();
        let policy = policy.clone();
        let non_retryable = non_retryable.clone();
        let signal_outer = opts.signal.clone();
        // Mutable so the stall-recovery nudge can be appended to the context
        // before a timeout retry.
        let mut ctx = ctx;
        Box::pin(async_stream::stream! {
            let mut attempts: usize = 0;
            // The stall-recovery nudge is reinjected at most once per run.
            let mut stall_nudged = false;
            loop {
                if signal_outer.is_cancelled() {
                    yield StreamEvent::Error {
                        error: "operation aborted before stream started".to_string(),
                    };
                    return;
                }
                let mut inner_stream = inner(ctx.clone(), opts.clone());

                // Per-attempt state.
                let mut committed = false;
                let mut retry_msg: Option<String> = None;

                while let Some(evt) = inner_stream.next().await {
                    match &evt {
                        StreamEvent::Error { error } => {
                            // Decide on retry vs surface BEFORE
                            // yielding the Error event downstream.
                            if non_retryable(error) {
                                yield evt;
                                return;
                            }
                            let kind = classify_error(error);
                            // Retry only when NO content has
                            // committed. Once the consumer has seen
                            // real text/thinking deltas, re-running
                            // the request would duplicate them
                            // downstream: the retry reset in
                            // stream.rs (PROV-5) clears the message
                            // history but not text already rendered
                            // to the UI, so attempt 2 would re-emit
                            // the response a second time. The #545
                            // mid-assembly-stall fix is preserved —
                            // a timeout before any text commits (e.g.
                            // a lone tool-call fragment) still retries
                            // below.
                            let retryable =
                                policy.should_retry(attempts, kind) && !committed;
                            if retryable {
                                retry_msg = Some(error.clone());
                                // Don't yield this Error — we're
                                // about to retry.
                                break;
                            }
                            // Retry exhausted, non-retryable kind,
                            // or committed → surface.
                            yield evt;
                            return;
                        }
                        StreamEvent::Delta { phase, .. } => {
                            // Real content streamed → no future
                            // retry is safe (would duplicate).
                            if phase.is_content() {
                                committed = true;
                            }
                            yield evt;
                        }
                        StreamEvent::Done { .. } => {
                            // Normal completion — pass through and
                            // terminate. (Some non-content phases
                            // may have streamed but the run finished
                            // cleanly.)
                            yield evt;
                            return;
                        }
                        StreamEvent::Start { .. } => {
                            // Start synthesized at stream begin —
                            // doesn't itself count as content;
                            // re-emitting it on retry is fine
                            // (consumers see Start on every
                            // attempt). We still yield it so the
                            // first-attempt consumer sees the
                            // expected sequence.
                            yield evt;
                        }
                        StreamEvent::Retry { .. } => {
                            // Inner stream should never produce
                            // Retry — only the outer retry loop
                            // does. Treat as unexpected terminal.
                            yield evt;
                            return;
                        }
                    }
                }

                // Inner stream exited the loop without Done. Either
                // we broke for retry, or the stream ended without
                // emitting a terminal event.
                match retry_msg {
                    Some(err_msg) => {
                        let backoff = policy.backoff_duration_for_msg(attempts, &err_msg);
                        // Race the backoff sleep against the AbortSignal so a
                        // cooperative cancel (signal only, no JoinHandle::abort)
                        // is observed promptly instead of stalling for the full
                        // backoff. `cancelled()` resolves immediately if the
                        // signal is already set and is race-free (its waiter is
                        // registered before the state check).
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = signal_outer.cancelled() => {
                                yield StreamEvent::Error {
                                    error: "operation aborted during retry backoff".to_string(),
                                };
                                return;
                            }
                        }
                        // Belt-and-suspenders: a cancel landing exactly as the
                        // sleep elapses is caught here too (the select! above
                        // may have taken the sleep arm in that instant).
                        if signal_outer.is_cancelled() {
                            yield StreamEvent::Error {
                                error: "operation aborted during retry backoff".to_string(),
                            };
                            return;
                        }
                        attempts += 1;
                        // If the previous attempt timed out, reinject a
                        // one-time "conclude and act" nudge so the retry can
                        // break out of a stall instead of repeating it.
                        let was_stall_timeout = is_stall_timeout(&err_msg);
                        // PROV-2: surface the retry so the UI can
                        // show a banner instead of freezing.
                        yield StreamEvent::Retry {
                            attempt: attempts as u32,
                            delay_ms: backoff.as_millis() as u64,
                            error: err_msg,
                        };
                        if was_stall_timeout && !stall_nudged {
                            stall_nudged = true;
                            ctx.messages.push(serde_json::json!({
                                "role": "user",
                                "content": STALL_RECOVERY_NUDGE,
                            }));
                        }
                        // Loop continues — next outer iteration
                        // calls the inner stream again.
                    }
                    None => {
                        // Stream closed without Done and without
                        // an Error we wanted to retry. Treat as
                        // natural termination — caller's
                        // stream_assistant_response has its own
                        // defensive fallback at this point.
                        return;
                    }
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{AssistantMessage, ContentBlock, StopReason};
    use crate::agent::agent_loop::stream::LlmContext;
    use crate::agent::recovery::RecoveryPolicy;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Drain a stream into a Vec.
    async fn drain(
        mut s: std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>,
    ) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(e);
        }
        out
    }

    /// Build a StreamFn that returns a canned list of events,
    /// optionally tracking call count.
    fn canned_stream_fn(events: Vec<Vec<StreamEvent>>) -> StreamFn {
        let counter = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(events));
        Arc::new(move |_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let attempts = events.lock().unwrap();
            let attempt_events = attempts.get(n).cloned().unwrap_or_default();
            Box::pin(futures::stream::iter(attempt_events))
        })
    }

    /// Stream factory + call counter pair for assertions.
    fn counted_canned(events: Vec<Vec<StreamEvent>>) -> (StreamFn, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(events));
        let counter_clone = counter.clone();
        let factory: StreamFn = Arc::new(move |_ctx, _opts| {
            let n = counter_clone.fetch_add(1, Ordering::SeqCst);
            let attempts = events.lock().unwrap();
            let attempt_events = attempts.get(n).cloned().unwrap_or_default();
            Box::pin(futures::stream::iter(attempt_events))
        });
        (factory, counter)
    }

    fn ctx() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            asset_dir: None,
        }
    }

    fn empty_assistant() -> AssistantMessage {
        AssistantMessage::new(vec![], StopReason::Stop)
    }

    fn assistant_with(text: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            StopReason::Stop,
        )
    }

    /// No errors → wrapper passes events through unchanged.
    #[tokio::test]
    async fn passthrough_when_no_errors() {
        let inner = canned_stream_fn(vec![vec![
            StreamEvent::Start {
                partial: empty_assistant(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("hello"),
                usage: None,
            },
        ]]);
        let wrapped = retrying_stream_fn(inner, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(events[1], StreamEvent::Done { .. }));
    }

    /// Network error on attempt 0; success on attempt 1 →
    /// wrapper retries; total 2 inner calls; consumer sees the
    /// successful stream's events.
    #[tokio::test]
    async fn retries_on_network_error() {
        // Cap attempts to a short backoff for the test.
        let policy = RecoveryPolicy::default();
        // Inner stream attempts:
        //  - attempt 0: only Error (network)
        //  - attempt 1: Start + Done
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                error: "connection timed out".to_string(),
            }],
            vec![
                StreamEvent::Start {
                    partial: empty_assistant(),
                },
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_with("after retry"),
                    usage: None,
                },
            ],
        ]);
        let wrapped = retrying_stream_fn(factory, policy);

        // We can't easily mock the sleep without injecting time;
        // accept the ~1s backoff cost for this test. Use
        // tokio::time::pause to make it free.
        tokio::time::pause();
        let drain_task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        // Advance virtual time to skip the backoff sleep.
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        let events = drain_task.await.unwrap();

        // Inner was called twice (one retry).
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        // Consumer events are FROM THE SUCCESSFUL ATTEMPT only.
        // The retried attempt's Error was suppressed.
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                StreamEvent::Start { .. } => "start",
                StreamEvent::Delta { .. } => "delta",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { .. } => "error",
                StreamEvent::Retry { .. } => "retry",
            })
            .collect();
        assert_eq!(kinds, vec!["retry", "start", "done"]);
    }

    /// Auth error → no retry, error passes through.
    #[tokio::test]
    async fn does_not_retry_auth_error() {
        let (factory, counter) = counted_canned(vec![vec![StreamEvent::Error {
            error: "401 unauthorized: invalid api key".to_string(),
        }]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Context-length error → no retry.
    #[tokio::test]
    async fn does_not_retry_context_length_error() {
        let (factory, counter) = counted_canned(vec![vec![StreamEvent::Error {
            error: "context length exceeded: prompt is too long".to_string(),
        }]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Error AFTER content streamed → no retry, error passes
    /// through. Mid-stream retries would emit duplicate tokens
    /// to the consumer.
    #[tokio::test]
    async fn does_not_retry_after_content_committed() {
        let (factory, counter) = counted_canned(vec![vec![
            StreamEvent::Start {
                partial: empty_assistant(),
            },
            // Real content streamed:
            StreamEvent::Delta {
                partial: assistant_with("partial "),
                phase: DeltaPhase::TextStart,
            },
            // Then network error:
            StreamEvent::Error {
                error: "connection reset".to_string(),
            },
        ]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        // No retry — only one inner call.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Consumer saw Start, Delta, Error in that order.
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::Delta {
                phase: DeltaPhase::TextStart,
                ..
            }
        ));
        assert!(matches!(events[2], StreamEvent::Error { .. }));
    }

    /// Regression for the duplicate-response bug: a stream-chunk
    /// timeout AFTER committed text content must SURFACE the error,
    /// not retry. #547 made timeouts retryable even when committed on
    /// the assumption the consumer discards the partial on
    /// `StreamEvent::Retry` — but that reset (stream.rs PROV-5) clears
    /// only the message history, not text already rendered to the UI,
    /// so attempt 2 re-emitted the response a second time. Gating
    /// retry on `!committed` keeps the #545 mid-assembly-stall fix
    /// (no text yet) while preventing duplication once text is shown.
    #[tokio::test]
    async fn surfaces_timeout_after_content_committed() {
        let (factory, counter) = counted_canned(vec![
            vec![
                StreamEvent::Start {
                    partial: empty_assistant(),
                },
                // Real text streamed → committed → retry would dup.
                StreamEvent::Delta {
                    partial: assistant_with("partial "),
                    phase: DeltaPhase::TextStart,
                },
                // Mid-assembly stream-chunk timeout (verbatim shape
                // produced by rig_stream.rs).
                StreamEvent::Error {
                    error: "stream chunk timed out after 29s while a tool call was mid-assembly"
                        .to_string(),
                },
            ],
            // Second attempt is supplied to prove it is NOT used.
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("ok"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        let events = task.await.unwrap();

        // Not retried — inner stream called exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // No Retry notice: the error was surfaced, not retried.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Retry { .. })),
            "expected no retry after committed content, got: {events:?}"
        );
        // The stream ends on the surfaced Error, not attempt 2's Done.
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
    }

    /// #545 (preserved): a mid-assembly stream-chunk timeout BEFORE
    /// any text content is committed still retries — the original
    /// case of a reasoning model that emits a tool-call fragment then
    /// stalls. No rendered text exists, so retrying is safe and
    /// avoids halting the whole run.
    #[tokio::test]
    async fn retries_timeout_before_content_committed() {
        let (factory, counter) = counted_canned(vec![
            vec![
                StreamEvent::Start {
                    partial: empty_assistant(),
                },
                // A tool-call fragment is NOT committed content
                // (DeltaPhase::is_content) — only text/thinking phases are.
                StreamEvent::Delta {
                    partial: empty_assistant(),
                    phase: DeltaPhase::ToolCallStart,
                },
                StreamEvent::Error {
                    error: "stream chunk timed out after 29s while a tool call was mid-assembly"
                        .to_string(),
                },
            ],
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("ok"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let events = task.await.unwrap();

        // Retried once → second attempt succeeded.
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Retry { .. }))
        );
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    /// Rate-limit error retries, retry-after-ms gets honoured
    /// (we test this indirectly via virtual time — advance past
    /// the retry-after and check the retry happened).
    #[tokio::test]
    async fn retries_on_rate_limit_with_retry_after() {
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                // RateLimit kind triggers via the "rate limit"
                // substring in classify_error. retry-after-ms is
                // 50ms so a short virtual-time advance covers it.
                error: "rate limit hit. retry-after-ms: 50".to_string(),
            }],
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("ok"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let events = task.await.unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    /// Cancel-via-AbortSignal during the retry backoff is observed
    /// PROMPTLY: the wrapper races the sleep against
    /// `signal.cancelled()` instead of sleeping the full backoff and
    /// only checking the flag afterward. Before the fix, a consumer
    /// that cancelled via the AbortSignal alone (no `JoinHandle::abort`)
    /// stalled for the entire backoff.
    #[tokio::test]
    async fn cancel_during_backoff_is_observed_promptly() {
        // A single retryable error whose parsed retry-after forces a
        // LARGE backoff (30s). We then cancel the AbortSignal and
        // assert the stream resolves within a couple of seconds —
        // impossible if the wrapper slept the full backoff first.
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                error: "rate limit hit. retry-after-ms: 30000".to_string(),
            }],
            // Second attempt is never reached: we cancel mid-backoff.
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("ok"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let signal = AbortSignal::new();
        let cancel_handle = signal.clone();

        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(signal),
            ))
            .await
        });

        // Let the drain reach the backoff sleep: the sole inner
        // attempt yields a single Error synchronously, so a couple of
        // yield_now cycles are enough to park it on the sleep.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Cancel via the AbortSignal ALONE (the cooperative path a
        // consumer uses without aborting the JoinHandle).
        cancel_handle.cancel();

        // Must resolve well under the 30s backoff. Before the fix this
        // elapsed the full backoff (the sleep ignored the signal).
        let events = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cancel during backoff must resolve the stream promptly")
            .expect("drain task should not panic");

        // Inner was called exactly once — we never proceeded to attempt 2.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(
            events.iter().any(|e| matches!(
                e,
                StreamEvent::Error { error }
                    if error == "operation aborted during retry backoff"
            )),
            "expected an 'operation aborted during retry backoff' Error, got: {events:?}"
        );
    }

    /// Max retries exceeded → final Error surfaces.
    #[tokio::test]
    async fn surfaces_error_after_max_retries() {
        // Default policy has 5 max retries. 6 consecutive Network
        // errors → first 5 retried, 6th surfaces.
        let attempts: Vec<Vec<StreamEvent>> = (0..6)
            .map(|_| {
                vec![StreamEvent::Error {
                    error: "network: connection timed out".to_string(),
                }]
            })
            .collect();
        let (factory, counter) = counted_canned(attempts);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        // Advance plenty for all backoffs to elapse.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        let events = task.await.unwrap();

        // max_retries = 5: attempts 0..=4 retry, attempt 5 surfaces.
        // 0 fails → retry; 1,2,3,4 fail → retry; 5 fails →
        // !should_retry → surface. Total: 6 inner calls.
        assert_eq!(counter.load(Ordering::SeqCst), 6);
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
    }

    /// AbortSignal cancelled before first attempt → emit
    /// abort error, no inner calls.
    #[tokio::test]
    async fn aborted_before_attempt_emits_error() {
        let (factory, counter) = counted_canned(vec![]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let signal = AbortSignal::new();
        signal.cancel();
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(signal),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Cancellation during retry backoff stops further attempts.
    /// Sequence: first attempt runs and fails → wrapper enters
    /// backoff sleep → test cancels → wrapper observes signal
    /// after sleep and emits abort Error. Second attempt's
    /// canned event is never delivered.
    #[tokio::test]
    async fn aborted_during_backoff_emits_error() {
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                error: "network glitch".to_string(),
            }],
            // Second attempt would succeed, but we abort first.
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("never seen"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let signal = AbortSignal::new();
        let signal_clone = signal.clone();

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(signal_clone),
            ))
            .await
        });
        // Let the spawned task start and run the first inner
        // attempt. Without yielding, the cancel below races with
        // the task's signal check at the top of the loop —
        // making the test flake. yield_now hands the runtime to
        // the spawn so the first attempt can run + start
        // sleeping in the backoff before we cancel.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        // First attempt should have run by now and the wrapper
        // is in `tokio::time::sleep`. Cancel while it's sleeping.
        signal.cancel();
        // Advance virtual time past the backoff so the sleep
        // returns and the post-sleep cancellation check fires.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        let events = task.await.unwrap();

        // Only the first attempt ran (second canned events
        // never consumed).
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Final event surfaces the abort.
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
    }

    /// Records the `messages` each inner attempt was called with, so a test can
    /// assert what the retry layer injected between attempts.
    fn recording_factory(first_error: &str) -> (StreamFn, Arc<Mutex<Vec<Vec<serde_json::Value>>>>) {
        let seen: Arc<Mutex<Vec<Vec<serde_json::Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let first_error = first_error.to_string();
        let factory: StreamFn = Arc::new(move |ctx: LlmContext, _opts| {
            seen2.lock().unwrap().push(ctx.messages.clone());
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let events = if n == 0 {
                vec![StreamEvent::Error {
                    error: first_error.clone(),
                }]
            } else {
                vec![StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: empty_assistant(),
                    usage: None,
                }]
            };
            Box::pin(futures::stream::iter(events))
        });
        (factory, seen)
    }

    fn has_stall_nudge(msgs: &[serde_json::Value]) -> bool {
        msgs.iter()
            .any(|m| m.get("content").and_then(|c| c.as_str()) == Some(STALL_RECOVERY_NUDGE))
    }

    /// A timeout retry reinjects the stall-recovery nudge into the next
    /// attempt's context (but not the first attempt's).
    #[tokio::test]
    async fn timeout_retry_injects_stall_nudge() {
        let (factory, seen) = recording_factory("request timeout after 120s");
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let _ = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 2, "one timeout retry → two inner calls");
        assert!(!has_stall_nudge(&recorded[0]), "no nudge on first attempt");
        assert!(
            has_stall_nudge(&recorded[1]),
            "stall nudge reinjected on the timeout retry"
        );
    }

    /// A non-timeout retryable error (e.g. 503) retries WITHOUT the nudge —
    /// it's specific to stalls, not generic transient blips.
    #[tokio::test]
    async fn non_timeout_retry_does_not_nudge() {
        let (factory, seen) = recording_factory("503 service unavailable");
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let _ = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert!(
            !has_stall_nudge(&recorded[1]),
            "no nudge for a non-timeout error"
        );
    }

    /// A plain stream-chunk timeout — the transport case ("provider
    /// stalled or connection silently dropped", no tool call open) —
    /// retries, but is NOT a reasoning-loop stall, so the
    /// stall-recovery nudge must not be injected. The nudge blames
    /// "a long reasoning loop", which is mis-attributed for a
    /// connection drop: the retry should just re-run cleanly.
    #[tokio::test]
    async fn plain_stream_chunk_timeout_does_not_nudge() {
        // Verbatim shape produced by rig_stream.rs for a plain
        // (no tool call open) stream-chunk timeout.
        let (factory, seen) = recording_factory(
            "stream chunk timed out after 300s (provider stalled or connection silently dropped) \
             — bump `stream_chunk_timeout_secs` in config.json if your model has long reasoning gaps",
        );
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let _ = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 2, "pre-commit timeout still retries");
        assert!(
            !has_stall_nudge(&recorded[1]),
            "transport stream-chunk timeout is not a reasoning-loop stall — no nudge"
        );
    }

    #[tokio::test]
    async fn request_establish_timeout_retries_without_a_stall_nudge() {
        // dirge-u44q: a request-establish timeout is a connection/handshake
        // stall, not a reasoning loop. It retries (classified Network) but
        // must NOT get the "conclude now" nudge.
        let (factory, seen) = recording_factory(
            "request establish timed out after 300s — the connection/handshake stalled before \
             the first response. Bump `timeouts.request_establish_secs` in config.json if a \
             legitimately slow first response was cut off.",
        );
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let _ = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        let recorded = seen.lock().unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "pre-commit establish timeout still retries"
        );
        assert!(
            !has_stall_nudge(&recorded[1]),
            "establish timeout is a connection stall — no reasoning-loop nudge"
        );
    }

    #[test]
    fn is_stall_timeout_excludes_connection_stalls() {
        // Mid-assembly reasoning stall — nudge helps.
        assert!(is_stall_timeout(
            "stream chunk timed out after 60s while a tool call was mid-assembly"
        ));
        // Connection-level stalls — nudge would misattribute.
        assert!(!is_stall_timeout(
            "stream chunk timed out (provider stalled or connection silently dropped)"
        ));
        assert!(!is_stall_timeout(
            "request establish timed out after 300s — the connection/handshake stalled"
        ));
        // Non-timeout errors are never stalls.
        assert!(!is_stall_timeout("503 service unavailable"));
    }
}
