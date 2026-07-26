//! Process-global "this endpoint is rate-limited until T" latch (GH #718).
//!
//! ## Why this exists
//!
//! dirge runs several independent provider-request paths — the agent turn,
//! the summarizer, post-session stages, subagents — each with its own retry
//! budget. None of them knows the others are being throttled, so a single
//! rate-limit window produces a storm: in the #718 report, 118 of 154
//! requests were 429s, and the wasted retries burned the reporter's entire
//! 50-request daily free-tier quota in under three minutes. The run then
//! spent another 13 minutes retrying against a cap that could not reset
//! until midnight UTC.
//!
//! When a provider tells us *definitively* that a window is exhausted
//! (`X-RateLimit-Remaining: 0` plus a reset, or an explicit `Retry-After`),
//! every further request before that reset is guaranteed to 429. This latch
//! records the deadline so those requests are never sent.
//!
//! ## Fail fast, don't sleep here
//!
//! The gate does not wait. It turns a doomed request into a synthesized 429
//! carrying the remaining wait, and lets the existing retry layer
//! (`agent_loop::retry`) do the sleeping — that layer races its backoff
//! against the `AbortSignal`, so a cancel is observed promptly. Sleeping
//! inside the HTTP client would be uncancellable.
//!
//! The synthesized message is deliberately shaped so `classify_error` and
//! `retry_after_from_error_msg` read it exactly like a real provider 429:
//! a short wait stays a retryable `RateLimit` with the correct backoff, and
//! a long one promotes to `UsageCap` and stops the run cleanly.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::sync_util::LockExt;

/// Upper bound on a recorded throttle. A provider that reports a
/// nonsensical reset (or a clock skew) must not be able to wedge an
/// endpoint for the life of the process.
const MAX_THROTTLE: Duration = Duration::from_secs(24 * 60 * 60);

struct Entry {
    until: Instant,
    /// The window the provider named, for the surfaced message.
    scope: Option<String>,
}

static GATE: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record that `endpoint` is rate-limited for another `wait`.
///
/// Extends an existing entry rather than shortening it: two concurrent
/// requests can both 429, and the later deadline is the safe one.
pub(crate) fn note(endpoint: &str, wait: Duration, scope: Option<String>) {
    if wait.is_zero() {
        return;
    }
    let until = Instant::now() + wait.min(MAX_THROTTLE);
    let mut gate = GATE.lock_ignore_poison();
    match gate.get_mut(endpoint) {
        Some(existing) if existing.until >= until => {}
        _ => {
            gate.insert(
                endpoint.to_string(),
                Entry {
                    until,
                    scope: scope.clone(),
                },
            );
            tracing::warn!(
                target: "dirge::rate_limit",
                endpoint = %endpoint,
                wait_secs = wait.as_secs(),
                scope = scope.as_deref().unwrap_or("-"),
                "provider rate limit reached; suppressing requests until it resets"
            );
        }
    }
}

/// Inspect a failed request's error text and latch the endpoint if the
/// provider definitively told us the window is exhausted. Returns the
/// recorded wait, or `None` if the error carried no such signal.
///
/// Deliberately conservative: a bare 429 with no reset information is left
/// alone, because guessing a deadline we were never given would be worse
/// than the existing exponential backoff.
pub(crate) fn note_from_error(endpoint: &str, error_msg: &str) -> Option<Duration> {
    use crate::agent::recovery::{ErrorKind, classify_error, rate_limit_signal};

    if !matches!(
        classify_error(error_msg),
        ErrorKind::RateLimit | ErrorKind::UsageCap
    ) {
        return None;
    }
    let signal = rate_limit_signal(error_msg);
    // Two definitive shapes: an explicit remaining-is-zero plus a reset, or
    // the provider naming a wait outright (`Retry-After`). Anything vaguer
    // stays on the old retry path.
    let wait = match (signal.exhausted, signal.reset_in) {
        (true, Some(reset)) if !reset.is_zero() => reset,
        _ => crate::agent::recovery::retry_after_from_error_msg(error_msg)
            .filter(|d| !d.is_zero())?,
    };
    note(endpoint, wait, signal.scope);
    Some(wait)
}

/// Latch from a real `HeaderMap` — the non-streaming path, where the
/// response reaches us intact instead of being flattened into rig's
/// status+body error string.
///
/// This is the only route by which providers that send their rate-limit
/// state ONLY as headers (Anthropic, OpenAI, Groq) can be honoured at all;
/// rig's streaming path discards headers before we see them. Renders the
/// relevant headers back into text so all the parsing in
/// [`crate::agent::recovery`] applies unchanged.
pub(crate) fn note_from_headers(endpoint: &str, headers: &http::HeaderMap) -> Option<Duration> {
    let mut text = String::from("429 Too Many Requests");
    for (name, value) in headers.iter() {
        let name = name.as_str();
        let relevant = name.starts_with("x-ratelimit")
            || name.starts_with("anthropic-ratelimit")
            || name == "retry-after"
            || name == "retry-after-ms";
        if relevant && let Ok(value) = value.to_str() {
            text.push('\n');
            text.push_str(name);
            text.push_str(": ");
            text.push_str(value);
        }
    }
    note_from_error(endpoint, &text)
}

/// How much longer `endpoint` is throttled, with the window name if the
/// provider gave one. `None` once the deadline has passed.
pub(crate) fn remaining(endpoint: &str) -> Option<(Duration, Option<String>)> {
    let mut gate = GATE.lock_ignore_poison();
    let entry = gate.get(endpoint)?;
    let now = Instant::now();
    if entry.until <= now {
        gate.remove(endpoint);
        return None;
    }
    Some((entry.until - now, entry.scope.clone()))
}

/// Drop any throttle recorded for `endpoint`. Called when a request to it
/// succeeds — the window evidently rolled early (or the limit was on a
/// different key than we assumed).
pub(crate) fn clear(endpoint: &str) {
    GATE.lock_ignore_poison().remove(endpoint);
}

/// The 429 we synthesize in place of a request we refused to send.
///
/// Shaped to round-trip through `classify_error` and
/// `retry_after_from_error_msg` so downstream behaviour is identical to
/// having actually received the provider's 429 — minus the wasted request.
pub(crate) fn suppressed_error_message(wait: Duration, scope: Option<&str>) -> String {
    let window = scope.map(|s| format!(" (window: {s})")).unwrap_or_default();
    format!(
        "429 Too Many Requests — dirge did not send this request: the provider's rate limit \
         is still in effect{window} and retrying before it resets cannot succeed. \
         Retry-After: {}",
        wait.as_secs().max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::recovery::{ErrorKind, classify_error};

    /// Each test uses a distinct endpoint key — the latch is process-global
    /// and the suite runs in parallel.
    fn endpoint(name: &str) -> String {
        format!("test-{name}.invalid")
    }

    #[test]
    fn unknown_endpoint_is_not_throttled() {
        assert!(remaining(&endpoint("unknown")).is_none());
    }

    #[test]
    fn noting_a_wait_throttles_the_endpoint() {
        let ep = endpoint("basic");
        note(&ep, Duration::from_secs(60), Some("per-min".into()));
        let (left, scope) = remaining(&ep).expect("endpoint should be throttled");
        assert!(left <= Duration::from_secs(60) && left > Duration::from_secs(55));
        assert_eq!(scope.as_deref(), Some("per-min"));
    }

    #[test]
    fn a_lapsed_throttle_reports_clear() {
        let ep = endpoint("lapsed");
        note(&ep, Duration::from_millis(1), None);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            remaining(&ep).is_none(),
            "throttle should expire on its own"
        );
    }

    #[test]
    fn a_zero_wait_records_nothing() {
        let ep = endpoint("zero");
        note(&ep, Duration::ZERO, None);
        assert!(remaining(&ep).is_none());
    }

    /// Concurrent 429s must not shorten an existing deadline.
    #[test]
    fn a_later_deadline_wins_over_an_earlier_one() {
        let ep = endpoint("extend");
        note(&ep, Duration::from_secs(300), None);
        note(&ep, Duration::from_secs(5), None);
        let (left, _) = remaining(&ep).expect("still throttled");
        assert!(
            left > Duration::from_secs(60),
            "a shorter later report must not shrink the window, got {left:?}",
        );
    }

    #[test]
    fn success_clears_the_throttle() {
        let ep = endpoint("clear");
        note(&ep, Duration::from_secs(300), None);
        clear(&ep);
        assert!(remaining(&ep).is_none());
    }

    #[test]
    fn an_absurd_wait_is_capped() {
        let ep = endpoint("absurd");
        note(&ep, Duration::from_secs(400 * 24 * 3600), None);
        let (left, _) = remaining(&ep).expect("still throttled");
        assert!(left <= MAX_THROTTLE, "must clamp, got {left:?}");
    }

    /// The reporter's per-day 429 (GH #718): remaining 0 with a far-future
    /// reset must latch the endpoint.
    #[test]
    fn openrouter_per_day_429_latches_the_endpoint() {
        let ep = endpoint("openrouter-day");
        let reset = (chrono::Utc::now() + chrono::Duration::hours(14)).timestamp_millis();
        let msg = format!(
            r#"Invalid status code 429 Too Many Requests with message: {{"error":{{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day","code":429,"metadata":{{"headers":{{"X-RateLimit-Limit":"50","X-RateLimit-Remaining":"0","X-RateLimit-Reset":"{reset}"}}}}}}}}"#
        );
        let wait = note_from_error(&ep, &msg).expect("definitive signal should latch");
        assert!(wait > Duration::from_secs(13 * 3600));
        let (_, scope) = remaining(&ep).expect("throttled");
        assert_eq!(scope.as_deref(), Some("free-models-per-day"));
    }

    /// A bare 429 with no reset data is NOT latched — we were told nothing
    /// definitive, so the old exponential-backoff path should still own it.
    #[test]
    fn a_bare_429_does_not_latch() {
        let ep = endpoint("bare");
        assert!(note_from_error(&ep, "HTTP 429 Too Many Requests").is_none());
        assert!(remaining(&ep).is_none());
    }

    /// A non-rate-limit failure must never latch the endpoint.
    #[test]
    fn a_network_error_does_not_latch() {
        let ep = endpoint("network");
        assert!(note_from_error(&ep, "connection reset by peer").is_none());
        assert!(remaining(&ep).is_none());
    }

    /// Real headers on the non-streaming path: Groq/OpenAI report the
    /// reset as a Go duration, split per dimension.
    #[test]
    fn header_map_with_an_exhausted_dimension_latches() {
        let ep = endpoint("headers-groq");
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-requests", "2m59.56s".parse().unwrap());
        headers.insert("x-ratelimit-remaining-tokens", "12000".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "7.66s".parse().unwrap());
        let wait = note_from_headers(&ep, &headers).expect("exhausted requests dimension latches");
        assert_eq!(
            wait,
            Duration::from_millis(179_560),
            "must wait on the exhausted dimension, not the healthy one",
        );
    }

    /// Anthropic sends `retry-after` alongside RFC 3339 resets; the
    /// explicit instruction wins.
    #[test]
    fn header_map_prefers_retry_after() {
        let ep = endpoint("headers-anthropic");
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after", "12".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-requests-reset",
            (chrono::Utc::now() + chrono::Duration::seconds(600))
                .to_rfc3339()
                .parse()
                .unwrap(),
        );
        assert_eq!(
            note_from_headers(&ep, &headers),
            Some(Duration::from_secs(12)),
        );
    }

    /// Headers with nothing rate-limit-shaped must not latch.
    #[test]
    fn header_map_without_rate_limit_info_does_not_latch() {
        let ep = endpoint("headers-empty");
        let mut headers = http::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        assert!(note_from_headers(&ep, &headers).is_none());
        assert!(remaining(&ep).is_none());
    }

    /// An explicit `Retry-After` is itself definitive.
    #[test]
    fn an_explicit_retry_after_latches() {
        let ep = endpoint("retry-after");
        let wait = note_from_error(&ep, "429 Too Many Requests; Retry-After: 30")
            .expect("Retry-After is definitive");
        assert_eq!(wait, Duration::from_secs(30));
    }

    /// The synthesized 429 must round-trip: a SHORT wait stays a retryable
    /// rate limit, and the backoff picks up the remaining wait rather than
    /// restarting the exponential schedule.
    #[test]
    fn suppressed_message_round_trips_as_a_retryable_rate_limit() {
        let msg = suppressed_error_message(Duration::from_secs(42), Some("free-models-per-min"));
        assert_eq!(classify_error(&msg), ErrorKind::RateLimit);
        let policy = crate::agent::recovery::RecoveryPolicy::default();
        let backoff = policy.backoff_duration_for_msg(0, &msg);
        assert!(
            backoff >= Duration::from_secs(42) && backoff <= Duration::from_secs(45),
            "backoff should track the remaining wait, got {backoff:?}",
        );
    }

    /// ...and a LONG wait promotes to a non-retryable usage cap, so the run
    /// stops cleanly instead of grinding through its retry budget.
    #[test]
    fn suppressed_message_round_trips_as_a_usage_cap_when_long() {
        let msg =
            suppressed_error_message(Duration::from_secs(14 * 3600), Some("free-models-per-day"));
        assert_eq!(classify_error(&msg), ErrorKind::UsageCap);
        assert!(
            !crate::agent::recovery::RecoveryPolicy::default()
                .should_retry(0, classify_error(&msg))
        );
    }
}
