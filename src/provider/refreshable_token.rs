//! A bearer that renews itself part-way through a long session.
//!
//! Three transports need this — Anthropic, Kimi and ChatGPT/Codex — and
//! until now each carried its own copy. The copies had already drifted:
//! `dirge-iki5` added a refresh MARGIN (renew shortly before the token
//! dies, because a request that passes the freshness check with
//! milliseconds to spare can still arrive after expiry, and that 401 is
//! non-retryable) to Kimi alone. One type, with the margin as a field,
//! makes the three sit side by side where a difference between them is a
//! visible decision rather than an oversight.
//!
//! ## Why the refresh goes off-thread (dirge-bz0a)
//!
//! The refresher is a synchronous call that spawns an OS thread, builds
//! a tokio runtime and `block_on`s an HTTP exchange — that shape exists
//! so it works whether or not a runtime is already present. It used to
//! be called from `normalized_request`, on the per-request path, on
//! dirge's single runtime thread (`#[tokio::main(flavor =
//! "current_thread")]`). While it ran, nothing painted, no keystroke was
//! read, and no timer could fire — including any timeout meant to bound
//! whatever was stuck. It read as a hang, it was bounded only by the
//! refresh's own 30s-per-attempt budget, and on Kimi (15-minute access
//! tokens) it recurred all session.
//!
//! So resolving a bearer is async now: [`bearer`] answers from the lock
//! when no renewal is due — the overwhelmingly common case, and no
//! thread hop — and hands the rare renewal to `spawn_blocking`, where
//! blocking is what the thread is for.
//!
//! The lock IS held across the renewal, deliberately. On a blocking
//! thread that costs nothing, and it means concurrent requests that
//! arrive during a renewal wait for the one in flight instead of each
//! starting their own.

use std::sync::{Arc, Mutex};

use crate::provider::auth::RefreshedAuth;

/// Re-resolves the bearer (and its expiry) when the frozen one expires
/// mid-session. Boxed so tests can inject a fake; the live seams wrap
/// the per-provider `load_fresh_*` helpers, which refresh and persist.
pub(crate) type RefreshFn = Arc<dyn Fn() -> anyhow::Result<RefreshedAuth> + Send + Sync>;

struct TokenState {
    bearer: String,
    /// `None` means "never refresh" — an API-key / env / legacy-file
    /// token with no refresh grant that dirge does not manage.
    expires_at_ms: Option<i64>,
}

pub(crate) struct RefreshableToken {
    state: Mutex<TokenState>,
    refresher: RefreshFn,
    /// Renew this many ms BEFORE the token actually dies (dirge-iki5).
    ///
    /// Per-provider on purpose. Kimi runs a margin because a 15-minute
    /// token crosses the boundary every 15 minutes of an active session;
    /// Anthropic and ChatGPT/Codex sit at zero today because their
    /// long-lived tokens cross it rarely. Same failure is reachable for
    /// all three, so the difference is a tuning call, not a rule — see
    /// the follow-up on `dirge-bz0a`.
    margin_ms: i64,
    /// Named in the log line when a renewal fails.
    provider: &'static str,
}

impl RefreshableToken {
    /// A bearer dirge cannot renew. With no expiry the refresh branch is
    /// unreachable, but the field still needs a value, so one code path
    /// serves both constructors.
    pub(crate) fn fixed(bearer: String) -> Self {
        RefreshableToken {
            state: Mutex::new(TokenState {
                bearer,
                expires_at_ms: None,
            }),
            refresher: Arc::new(|| Err(anyhow::anyhow!("this bearer cannot be refreshed"))),
            margin_ms: 0,
            provider: "unknown",
        }
    }

    pub(crate) fn renewable(
        bearer: String,
        expires_at_ms: Option<i64>,
        refresher: RefreshFn,
        margin_ms: i64,
        provider: &'static str,
    ) -> Self {
        RefreshableToken {
            state: Mutex::new(TokenState {
                bearer,
                expires_at_ms,
            }),
            refresher,
            margin_ms,
            provider,
        }
    }

    /// Is a renewal due right now?
    fn renewal_due(&self, state: &TokenState) -> bool {
        state.expires_at_ms.is_some_and(|expires_at| {
            crate::auth::file_store::epoch_ms_is_expired_within(
                expires_at,
                chrono::Utc::now().timestamp_millis(),
                self.margin_ms,
            )
        })
    }

    /// The bearer when no renewal is due. `None` is the caller's cue to
    /// go off-thread, and it is the only path that can block.
    fn if_fresh(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if self.renewal_due(&state) {
            return None;
        }
        Some(state.bearer.clone())
    }

    /// Renew if due, then return the bearer.
    ///
    /// BLOCKS — the refresher joins an OS thread running its own runtime.
    /// Call it from `spawn_blocking`, never from the runtime thread.
    ///
    /// A failed renewal keeps the stale token so the request fails
    /// exactly as it would have without this seam rather than wedging
    /// the client; the next request tries again.
    fn renewed_blocking(&self) -> String {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Re-checked under the lock: another request may have renewed
        // while this one waited for its blocking thread.
        if self.renewal_due(&state) {
            match (self.refresher)() {
                Ok(fresh) => {
                    state.bearer = fresh.bearer_token;
                    state.expires_at_ms = fresh.expires_at_ms;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::provider",
                        provider = %self.provider,
                        error = %e,
                        "OAuth token expired and refresh failed; sending the stale token",
                    );
                }
            }
        }
        state.bearer.clone()
    }
}

/// Resolve the bearer for one request without blocking the runtime.
///
/// `None` in, `None` out: a transport with no token rewrites no header.
pub(crate) async fn bearer(token: Option<Arc<RefreshableToken>>) -> Option<String> {
    let token = token?;
    // Fast path — no renewal due, so no thread hop on the hot path.
    if let Some(fresh) = token.if_fresh() {
        return Some(fresh);
    }
    match tokio::task::spawn_blocking(move || token.renewed_blocking()).await {
        Ok(bearer) => Some(bearer),
        // The blocking pool is shutting down (or the task panicked, which
        // it cannot: `renewed_blocking` recovers a poisoned lock and
        // swallows refresher errors). Sending no Authorization header is
        // wrong, but so is blocking here to get one; the request fails
        // with a 401 the caller already knows how to report.
        Err(e) => {
            tracing::warn!(
                target: "dirge::provider",
                error = %e,
                "bearer refresh task did not complete; sending the request unauthenticated",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn in_ms(ms: i64) -> Option<i64> {
        Some(chrono::Utc::now().timestamp_millis() + ms)
    }

    fn counting_refresher(calls: Arc<AtomicUsize>, bearer: &'static str) -> RefreshFn {
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(RefreshedAuth {
                bearer_token: bearer.to_string(),
                expires_at_ms: in_ms(3_600_000),
            })
        })
    }

    #[tokio::test]
    async fn a_live_token_is_used_as_is_and_never_renews() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token = RefreshableToken::renewable(
            "live".to_string(),
            in_ms(3_600_000),
            counting_refresher(calls.clone(), "fresh"),
            0,
            "test",
        );
        assert_eq!(bearer(Some(Arc::new(token))).await.as_deref(), Some("live"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expired_token_renews() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token = RefreshableToken::renewable(
            "stale".to_string(),
            in_ms(-1_000),
            counting_refresher(calls.clone(), "fresh"),
            0,
            "test",
        );
        assert_eq!(
            bearer(Some(Arc::new(token))).await.as_deref(),
            Some("fresh")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// dirge-iki5, now available to every provider rather than one: a
    /// token still technically alive but inside the margin renews, so a
    /// request cannot arrive after expiry and take a non-retryable 401.
    #[tokio::test]
    async fn a_token_inside_the_margin_renews_before_it_dies() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token = RefreshableToken::renewable(
            "nearly".to_string(),
            in_ms(30_000), // alive for another 30s…
            counting_refresher(calls.clone(), "fresh"),
            60_000, // …but the margin is 60s
            "test",
        );
        assert_eq!(
            bearer(Some(Arc::new(token))).await.as_deref(),
            Some("fresh")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The discrimination half of the above: with no margin the same
    /// token is used as-is. Without this, a margin that always fired
    /// would look identical.
    #[tokio::test]
    async fn the_same_token_with_no_margin_is_used_as_is() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token = RefreshableToken::renewable(
            "nearly".to_string(),
            in_ms(30_000),
            counting_refresher(calls.clone(), "fresh"),
            0,
            "test",
        );
        assert_eq!(
            bearer(Some(Arc::new(token))).await.as_deref(),
            Some("nearly")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failed_renewal_sends_the_stale_token_rather_than_wedging() {
        let token = RefreshableToken::renewable(
            "stale".to_string(),
            in_ms(-1_000),
            Arc::new(|| anyhow::bail!("network down")),
            0,
            "test",
        );
        assert_eq!(
            bearer(Some(Arc::new(token))).await.as_deref(),
            Some("stale")
        );
    }

    #[tokio::test]
    async fn a_fixed_bearer_never_renews() {
        let token = RefreshableToken::fixed("static".to_string());
        assert_eq!(
            bearer(Some(Arc::new(token))).await.as_deref(),
            Some("static")
        );
    }

    #[tokio::test]
    async fn no_token_means_no_header() {
        assert_eq!(bearer(None).await, None);
    }

    /// dirge-bz0a: the whole point. Resolving a bearer must not block
    /// the runtime thread, so a renewal that takes a while must not stop
    /// other tasks on a single-threaded runtime from making progress.
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_renewal_does_not_stop_the_runtime() {
        let token = Arc::new(RefreshableToken::renewable(
            "stale".to_string(),
            in_ms(-1_000),
            Arc::new(|| {
                // Stands in for the refresher's spawn-a-thread-and-join.
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok(RefreshedAuth {
                    bearer_token: "fresh".to_string(),
                    expires_at_ms: None,
                })
            }),
            0,
            "test",
        ));
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticking = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                for _ in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        assert_eq!(bearer(Some(token)).await.as_deref(), Some("fresh"));
        // The other task ran to completion DURING the renewal. Resolve
        // the bearer on this thread instead and it could not have.
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            10,
            "the runtime stalled while the bearer was being renewed"
        );
        ticking.await.expect("join");
    }
}

#[cfg(test)]
mod fast_path_tests {
    use super::*;
    use crate::provider::auth::RefreshedAuth;

    /// The fast path is not an optimisation detail — `renewed_blocking`
    /// re-checks under the lock, so skipping it would still be CORRECT,
    /// just wrong in cost: every request would hop to the blocking pool
    /// to be told nothing needs doing. That is unobservable from a
    /// return value, so pin it against a pool with no free thread. A
    /// live token must resolve anyway; only a renewal may need one.
    #[test]
    fn a_live_token_resolves_without_a_blocking_thread() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            // Occupy the single blocking slot until the test releases
            // it — a sleep would work but would also make the test wait
            // out its own hog before the runtime could shut down.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                let _ = held.recv();
            });
            tokio::task::yield_now().await;

            let token = Arc::new(RefreshableToken::renewable(
                "live".to_string(),
                Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
                Arc::new(|| {
                    Ok(RefreshedAuth {
                        bearer_token: "unexpected".to_string(),
                        expires_at_ms: None,
                    })
                }),
                0,
                "test",
            ));
            let resolved =
                tokio::time::timeout(std::time::Duration::from_secs(2), bearer(Some(token)))
                    .await
                    .expect("a live token must not wait on the blocking pool");
            assert_eq!(resolved.as_deref(), Some("live"));
            drop(release);
            hog.await.expect("the hog exits once released");
        });
    }
}
