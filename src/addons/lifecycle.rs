//! The addon host as a [`SessionLifecycle`]: `:dirge/session-start` and
//! `:dirge/session-end` hear the session lifecycle.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::domain::HookPoint;
use super::host::AddonHost;
use crate::agent::session_lifecycle::{
    Budgets, Lifecycle, LifecycleHook, SessionEnd, SessionLifecycle, SessionStart, collect,
};

/// Where [`AddonLifecycle`] finds the host each time it is asked.
type HostSource = Arc<dyn Fn() -> Option<Arc<AddonHost>> + Send + Sync>;

/// Announces the session lifecycle to the addons of a host.
pub struct AddonLifecycle {
    host: HostSource,
}

impl AddonLifecycle {
    /// The process-wide host, whichever is running when an event comes:
    /// `/addons reload` may start or replace it after boot.
    pub fn live() -> Self {
        Self {
            host: Arc::new(super::global),
        }
    }

    /// `host`, always.
    #[cfg(test)]
    pub fn of(host: Arc<AddonHost>) -> Self {
        Self {
            host: Arc::new(move || Some(host.clone())),
        }
    }
}

impl SessionLifecycle for AddonLifecycle {
    fn listens(&self, hook: LifecycleHook) -> bool {
        (self.host)().is_some_and(|host| host.listens(hook_point(hook)))
    }

    fn session_start(&self, event: &SessionStart) -> Vec<String> {
        (self.host)()
            .map(|host| host.session_start(&start_ctx(event)))
            .unwrap_or_default()
    }

    fn session_end(&self, event: &SessionEnd) {
        if let Some(host) = (self.host)() {
            host.session_end(&end_ctx(event));
        }
    }
}

fn hook_point(hook: LifecycleHook) -> HookPoint {
    match hook {
        LifecycleHook::Start => HookPoint::SessionStart,
        LifecycleHook::End => HookPoint::SessionEnd,
    }
}

/// `:dirge/session-start`'s ctx.
pub fn start_ctx(event: &SessionStart) -> Value {
    json!({
        "session-id": event.session_id,
        "cwd": event.cwd,
        "first-prompt?": event.first_prompt,
        "mcp-servers": event.mcp_servers,
    })
}

/// `:dirge/session-end`'s ctx; the host hands `reason` over as a keyword.
pub fn end_ctx(event: &SessionEnd) -> Value {
    json!({
        "session-id": event.session_id,
        "cwd": event.cwd,
        "reason": event.reason.key(),
    })
}

/// Make the addon host the session lifecycle of this process.
pub fn install(settings: &crate::config::AddonsConfig) {
    crate::agent::session_lifecycle::install(Lifecycle::new(
        Arc::new(AddonLifecycle::live()),
        Arc::default(),
        collect::mcp_servers(),
        budgets(settings),
    ));
}

/// The default budgets, with the timeouts `settings` sets.
fn budgets(settings: &crate::config::AddonsConfig) -> Budgets {
    let default = Budgets::default();
    let secs = |set: Option<u64>, or: Duration| set.map(Duration::from_secs).unwrap_or(or);
    Budgets {
        start: secs(settings.session_start_timeout_secs, default.start),
        end: secs(settings.session_end_timeout_secs, default.end),
        ..default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_lifecycle::domain::SessionEndReason;

    #[test]
    fn the_start_ctx_carries_the_keys_addons_read() {
        let ctx = start_ctx(&SessionStart {
            session_id: Some("s1".into()),
            cwd: "/w".into(),
            first_prompt: true,
            mcp_servers: vec!["hive".into()],
        });
        assert_eq!(
            ctx,
            json!({"session-id": "s1", "cwd": "/w", "first-prompt?": true, "mcp-servers": ["hive"]})
        );
    }

    #[test]
    fn the_end_ctx_names_its_reason() {
        let ctx = end_ctx(&SessionEnd {
            session_id: None,
            cwd: "/w".into(),
            reason: SessionEndReason::Swap,
        });
        assert_eq!(
            ctx,
            json!({"session-id": null, "cwd": "/w", "reason": "swap"})
        );
    }

    #[test]
    fn configured_timeouts_replace_the_default_budgets() {
        let settings = crate::config::AddonsConfig {
            session_start_timeout_secs: Some(180),
            ..Default::default()
        };
        let budgets = budgets(&settings);
        assert_eq!(budgets.start, Duration::from_secs(180));
        assert_eq!(budgets.end, Budgets::default().end);
        assert_eq!(budgets.mcp_wait, Budgets::default().mcp_wait);
        assert_eq!(
            super::budgets(&crate::config::AddonsConfig::default()),
            Budgets::default()
        );
    }

    #[test]
    fn without_a_host_nothing_listens() {
        let none = AddonLifecycle {
            host: Arc::new(|| None),
        };
        assert!(!none.listens(LifecycleHook::Start));
        assert!(!none.listens(LifecycleHook::End));
    }
}
