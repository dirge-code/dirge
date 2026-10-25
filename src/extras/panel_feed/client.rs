//! HTTP boundary: the event-stream subscription loop and the reply
//! POST. Everything decided here is fed by the pure pieces
//! ([`super::sse`], [`super::ops`], [`super::discovery`]).
//!
//! The token travels as a `token` query parameter, so request URLs
//! are secret: every `reqwest::Error` is stripped of its URL before
//! it can reach a log line.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::watch;

use super::discovery::{self, DiscoveryError, Endpoint, Source};
use super::ops::{self, FeedSink};
use super::sse::{SseItem, SseParser};
use crate::ui::panels_ext::PanelOp;

const LOG: &str = "dirge::panel_feed";

/// Loop tuning. Production uses [`FeedOptions::default`]; tests
/// shrink the timings.
#[derive(Debug, Clone)]
pub struct FeedOptions {
    /// Reconnect base delay until the producer sends `retry:`.
    pub initial_retry: Duration,
    /// Ceiling of the exponential backoff.
    pub max_backoff: Duration,
    /// A connection silent for this long (no event, no heartbeat) is
    /// considered dead and re-established.
    pub idle_timeout: Duration,
    /// Connect timeout for one attempt.
    pub connect_timeout: Duration,
    /// Features advertised on the subscription (`features=a,b`), so the
    /// producer only sends ops this client acts on (`loop`: loop ops).
    pub features: Vec<String>,
}

impl Default for FeedOptions {
    fn default() -> Self {
        Self {
            initial_retry: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(5),
            features: Vec::new(),
        }
    }
}

/// Smallest and largest `retry:` honoured, so a producer can neither
/// spin the client nor park it for hours.
const MIN_RETRY_MS: u64 = 100;
const MAX_RETRY_MS: u64 = 60_000;

/// Exponential backoff with jitter (pure). `unit` is a uniform draw
/// in `[0, 1)`: the delay is `exp/2 + unit * exp/2` where
/// `exp = min(max, base * 2^attempt)`, so reconnecting clients
/// spread out while each wait stays within a factor of two.
pub fn backoff_delay(base: Duration, attempt: u32, max: Duration, unit: f64) -> Duration {
    let exp = base
        .saturating_mul(1u32 << attempt.min(16))
        .min(max)
        .max(Duration::from_millis(1));
    let half = exp / 2;
    half + half.mul_f64(unit.clamp(0.0, 1.0))
}

fn jitter_unit() -> f64 {
    let bits = (uuid::Uuid::new_v4().as_u128() >> 64) as u64;
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// The subscription URL: [`endpoint_url`] plus `features=a,b` when any
/// are advertised.
fn events_url(ep: &Endpoint, features: &[String]) -> Result<reqwest::Url, String> {
    let mut url = endpoint_url(ep, "events")?;
    if !features.is_empty() {
        url.query_pairs_mut()
            .append_pair("features", &features.join(","));
    }
    Ok(url)
}

/// `<base>/<path>?token=<token>` with the token percent-encoded.
fn endpoint_url(ep: &Endpoint, path: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(&format!("{}/{path}", ep.url))
        .map_err(|e| format!("bad feed url: {e}"))?;
    if let Some(token) = &ep.token {
        url.query_pairs_mut().append_pair("token", token);
    }
    Ok(url)
}

/// A `reqwest::Error` rendered without the (token-bearing) URL.
fn redact(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

fn http_client(connect_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        // The feed is a local producer: never route it (and its
        // token) through a system proxy.
        .no_proxy()
        .connect_timeout(connect_timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

enum Outcome {
    /// Shutdown was requested.
    Stopped,
    /// The connection delivered at least one event before ending.
    Delivered,
    /// Nothing useful happened (refused, error status, early drop).
    Failed,
}

/// Run the subscription loop until `stop` flips to `true` (or its
/// sender is dropped). Never returns early on errors: discovery,
/// HTTP and stream failures all back off and retry.
pub async fn run(
    source: Source,
    sink: Arc<dyn FeedSink>,
    opts: FeedOptions,
    mut stop: watch::Receiver<bool>,
) {
    let client = http_client(opts.connect_timeout);
    let mut attempt: u32 = 0;
    let mut base = opts.initial_retry;
    let mut last_error: Option<String> = None;
    loop {
        if *stop.borrow() {
            break;
        }
        let outcome = match discovery::resolve(&source) {
            Ok(ep) => {
                connect_once(
                    &client,
                    &ep,
                    sink.as_ref(),
                    &opts,
                    &mut stop,
                    &mut base,
                    &mut last_error,
                )
                .await
            }
            Err(e) => {
                note_error_as(&mut last_error, &e, e.is_missing());
                Outcome::Failed
            }
        };
        match outcome {
            Outcome::Stopped => break,
            Outcome::Delivered => attempt = 0,
            Outcome::Failed => attempt = attempt.saturating_add(1),
        }
        let delay = backoff_delay(base, attempt, opts.max_backoff, jitter_unit());
        tracing::debug!(target: LOG, ?delay, attempt, "panel feed reconnect scheduled");
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.changed() => break,
        }
    }
    tracing::debug!(target: LOG, "panel feed stopped");
}

/// Log a failure at `warn` the first time it is seen, `debug` while
/// it repeats.
fn note_error(last: &mut Option<String>, err: &dyn std::fmt::Display) {
    note_error_as(last, err, false);
}

/// [`note_error`], but a `quiet` failure (a missing discovery file:
/// the ordinary "producer not running" state) never warns.
fn note_error_as(last: &mut Option<String>, err: &dyn std::fmt::Display, quiet: bool) {
    let text = err.to_string();
    if last.as_deref() == Some(text.as_str()) {
        tracing::debug!(target: LOG, "panel feed: {text}");
        return;
    }
    if quiet {
        tracing::debug!(target: LOG, "panel feed: {text}");
    } else {
        tracing::warn!(target: LOG, "panel feed: {text}");
    }
    *last = Some(text);
}

async fn connect_once(
    client: &reqwest::Client,
    ep: &Endpoint,
    sink: &dyn FeedSink,
    opts: &FeedOptions,
    stop: &mut watch::Receiver<bool>,
    base: &mut Duration,
    last_error: &mut Option<String>,
) -> Outcome {
    let url = match events_url(ep, &opts.features) {
        Ok(u) => u,
        Err(e) => {
            note_error(last_error, &e);
            return Outcome::Failed;
        }
    };
    let request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send();
    let mut resp = tokio::select! {
        r = request => match r {
            Ok(r) => r,
            Err(e) => {
                note_error(last_error, &redact(e));
                return Outcome::Failed;
            }
        },
        _ = stop.changed() => return Outcome::Stopped,
    };
    let status = resp.status();
    if !status.is_success() {
        note_error(last_error, &format!("feed answered HTTP {status}"));
        return Outcome::Failed;
    }
    tracing::debug!(target: LOG, url = %ep.url, "panel feed connected");
    *last_error = None;

    let mut parser = SseParser::new();
    let mut shown: Vec<String> = Vec::new();
    let mut delivered = false;
    let outcome = loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(opts.idle_timeout, resp.chunk()) => c,
            _ = stop.changed() => break Outcome::Stopped,
        };
        let bytes = match chunk {
            Ok(Ok(Some(b))) => b,
            Ok(Ok(None)) => {
                tracing::debug!(target: LOG, "panel feed stream ended");
                break Outcome::Failed;
            }
            Ok(Err(e)) => {
                tracing::debug!(target: LOG, "panel feed stream error: {}", redact(e));
                break Outcome::Failed;
            }
            Err(_) => {
                tracing::debug!(target: LOG, "panel feed idle for {:?}; reconnecting", opts.idle_timeout);
                break Outcome::Failed;
            }
        };
        let items = match parser.feed(&bytes) {
            Ok(items) => items,
            Err(e) => {
                note_error(last_error, &e);
                break Outcome::Failed;
            }
        };
        for item in items {
            match item {
                SseItem::Event(ev) => {
                    delivered = true;
                    if let Some(id) = ops::route(&ev.data, sink)
                        && !shown.contains(&id)
                    {
                        shown.push(id);
                    }
                }
                SseItem::Retry(ms) => {
                    *base = Duration::from_millis(ms.clamp(MIN_RETRY_MS, MAX_RETRY_MS));
                }
                SseItem::Comment(_) => {}
            }
        }
    };
    // The producer is gone (or we are): its panels would otherwise
    // linger as stale state. A reconnect replays what is still live.
    if !matches!(outcome, Outcome::Stopped) {
        for id in shown {
            sink.apply(ops::FeedEffect::Panel(PanelOp::Close { id }));
        }
    }
    match outcome {
        Outcome::Failed if delivered => Outcome::Delivered,
        other => other,
    }
}

/// Why a reply was not accepted.
#[derive(Debug, Error)]
pub enum ReplyError {
    #[error("no panel feed is running")]
    NotRunning,
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("{0}")]
    Http(String),
    #[error("feed answered HTTP {0}")]
    Status(u16),
}

/// POST one raw JSON reply to `<url>/reply`. Success is any 2xx (the
/// protocol answers 204 No Content).
pub async fn post_reply(ep: &Endpoint, body: String) -> Result<(), ReplyError> {
    let url = endpoint_url(ep, "reply").map_err(ReplyError::Http)?;
    let resp = http_client(FeedOptions::default().connect_timeout)
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(10))
        .body(body)
        .send()
        .await
        .map_err(|e| ReplyError::Http(redact(e)))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(ReplyError::Status(status.as_u16()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_caps_and_jitters_within_bounds() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(2);
        assert_eq!(backoff_delay(base, 0, max, 0.0), Duration::from_millis(50));
        assert_eq!(backoff_delay(base, 0, max, 1.0), Duration::from_millis(100));
        assert_eq!(backoff_delay(base, 3, max, 1.0), Duration::from_millis(800));
        assert_eq!(backoff_delay(base, 10, max, 1.0), max);
        assert_eq!(backoff_delay(base, u32::MAX, max, 0.0), max / 2);
        for _ in 0..100 {
            let d = backoff_delay(base, 2, max, jitter_unit());
            assert!(d >= Duration::from_millis(200) && d <= Duration::from_millis(400));
        }
    }

    #[test]
    fn urls_encode_the_token_and_errors_hide_it() {
        let ep = Endpoint {
            url: "http://127.0.0.1:9/feed".into(),
            token: Some("a b&c".into()),
        };
        assert_eq!(
            endpoint_url(&ep, "events").unwrap().as_str(),
            "http://127.0.0.1:9/feed/events?token=a+b%26c"
        );
        let bare = Endpoint {
            url: "http://127.0.0.1:9".into(),
            token: None,
        };
        assert_eq!(
            endpoint_url(&bare, "reply").unwrap().as_str(),
            "http://127.0.0.1:9/reply"
        );
        assert_eq!(
            events_url(&ep, &["loop".into(), "spans".into()])
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9/feed/events?token=a+b%26c&features=loop%2Cspans"
        );
        assert_eq!(
            events_url(&bare, &[]).unwrap().as_str(),
            "http://127.0.0.1:9/events"
        );
    }
}
