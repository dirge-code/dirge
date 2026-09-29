//! Panel feed: a generic Server-Sent Events subscription that drives
//! the external panels in the left side panel and posts one-line
//! notifications, plus a small reply channel back to the producer.
//!
//! Off by default. Enabled by the `panel_feed` config block (see
//! [`discovery::PanelFeedConfig`]); the wire format is documented in
//! `docs/panel-feed.md`.
//!
//! Layout:
//! - [`sse`] — pure incremental event-stream parser;
//! - [`ops`] — pure op decoding into [`ops::FeedEffect`] plus the
//!   [`ops::FeedSink`] port (production: [`ops::UiSink`]);
//! - [`discovery`] — config -> endpoint, private-file checks;
//! - [`client`] — the HTTP boundary (subscription loop, reply POST).

pub mod client;
pub mod discovery;
pub mod ops;
pub mod sse;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::watch;

use crate::sync_util::LockExt;
use crate::ui::notifications::Notification;
use client::{FeedOptions, ReplyError};
use discovery::{PanelFeedConfig, Source};

/// The source of the running feed, so replies re-resolve the
/// endpoint (a restarted producer has a new port and token).
static ACTIVE: Mutex<Option<Source>> = Mutex::new(None);

/// One reply the user can send back to the producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyAction {
    /// Focus the item `target` (an id the producer showed).
    Focus(String),
    /// Leave the focused view.
    Unfocus,
    NextTab,
    PrevTab,
    /// Ask the producer to repaint everything it shows.
    Refresh,
}

/// Usage line for the reply verbs of `/panel`.
pub const REPLY_USAGE: &str = "usage: /panel next|prev|refresh|unfocus|focus <id>";

impl ReplyAction {
    /// Parse the words after `/panel` into a reply (pure). `Err`
    /// carries a user-facing usage message.
    pub fn parse(args: &[&str]) -> Result<Self, String> {
        let verb = args.first().map(|s| s.trim()).unwrap_or("");
        let rest = &args[args.len().min(1)..];
        let action = match verb {
            "next" | "next-tab" => Self::NextTab,
            "prev" | "prev-tab" => Self::PrevTab,
            "refresh" => Self::Refresh,
            "unfocus" => Self::Unfocus,
            "focus" => {
                return match rest {
                    [id] if !id.trim().is_empty() => Ok(Self::Focus(id.trim().to_string())),
                    [] => Err(format!("/panel focus needs an item id ({REPLY_USAGE})")),
                    _ => Err(format!("/panel focus takes one id ({REPLY_USAGE})")),
                };
            }
            "" => return Err(REPLY_USAGE.to_string()),
            other => return Err(format!("unknown /panel action '{other}' ({REPLY_USAGE})")),
        };
        if rest.is_empty() {
            Ok(action)
        } else {
            Err(format!("/panel {verb} takes no argument ({REPLY_USAGE})"))
        }
    }

    /// The wire name of the action.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Focus(_) => "focus",
            Self::Unfocus => "unfocus",
            Self::NextTab => "next-tab",
            Self::PrevTab => "prev-tab",
            Self::Refresh => "refresh",
        }
    }

    /// The JSON body POSTed to `<url>/reply` (pure).
    pub fn to_json(&self) -> String {
        let value = match self {
            Self::Focus(target) => json!({"action": "focus", "target": target}),
            Self::Unfocus => json!({"action": "unfocus"}),
            Self::NextTab => json!({"action": "next-tab"}),
            Self::PrevTab => json!({"action": "prev-tab"}),
            Self::Refresh => json!({"action": "refresh"}),
        };
        value.to_string()
    }
}

/// Send `action` to the running feed's producer.
pub async fn reply(action: ReplyAction) -> Result<(), ReplyError> {
    let source = ACTIVE
        .lock_ignore_poison()
        .clone()
        .ok_or(ReplyError::NotRunning)?;
    reply_to(&source, &action).await
}

/// Where replies are sent. Production: [`LiveFeed`]; tests record.
pub trait ReplyTransport: Send + Sync {
    fn send(&self, action: &ReplyAction) -> impl Future<Output = Result<(), ReplyError>> + Send;
}

/// The running feed's producer (re-resolved on every reply).
pub struct LiveFeed;

impl ReplyTransport for LiveFeed {
    async fn send(&self, action: &ReplyAction) -> Result<(), ReplyError> {
        reply(action.clone()).await
    }
}

/// The notification a failed reply surfaces (pure). No running feed
/// is a warning; a refused or broken request is an error.
pub fn failure_notice(action: &ReplyAction, err: &ReplyError) -> Notification {
    let message = crate::ui::ansi::strip_escapes(
        &format!("panel reply '{}' failed: {err}", action.name()),
        crate::ui::ansi::StripPolicy::STRICT,
    );
    match err {
        ReplyError::NotRunning => Notification::Warn(message),
        _ => Notification::Error(message),
    }
}

/// Send `action` through `transport`; a failure comes back as the
/// notification to show.
pub async fn send_reply<T: ReplyTransport>(
    transport: &T,
    action: ReplyAction,
) -> Option<Notification> {
    match transport.send(&action).await {
        Ok(()) => None,
        Err(err) => Some(failure_notice(&action, &err)),
    }
}

/// Fire `action` at the running feed without blocking the caller; a
/// failure is posted on the notification channel. Must be called
/// inside a tokio runtime.
pub fn spawn_reply(action: ReplyAction) {
    tokio::spawn(async move {
        if let Some(notice) = send_reply(&LiveFeed, action).await {
            crate::ui::notifications::notify_send(notice);
        }
    });
}

/// Send `action` to the producer behind `source`.
pub async fn reply_to(source: &Source, action: &ReplyAction) -> Result<(), ReplyError> {
    let ep = discovery::resolve(source)?;
    client::post_reply(&ep, action.to_json()).await
}

/// A running feed. Dropping it stops the subscription loop and
/// forgets the reply target.
pub struct FeedHandle {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl FeedHandle {
    /// Stop the loop and wait for it to finish.
    #[allow(dead_code)] // production relies on Drop; tests await it
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for FeedHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        *ACTIVE.lock_ignore_poison() = None;
    }
}

/// The directory a relative `discovery_dir` resolves under:
/// `$XDG_RUNTIME_DIR`, else the system temp dir.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Start the feed described by `cfg`, or `None` when it is disabled
/// or has no source. Must be called inside a tokio runtime.
pub fn start(cfg: Option<&PanelFeedConfig>) -> Option<FeedHandle> {
    let source = cfg?.source(Some(&runtime_dir()))?;
    Some(spawn(source, Arc::new(ops::UiSink), FeedOptions::default()))
}

/// Spawn the subscription loop for `source` into `sink`.
pub fn spawn(source: Source, sink: Arc<dyn ops::FeedSink>, opts: FeedOptions) -> FeedHandle {
    *ACTIVE.lock_ignore_poison() = Some(source.clone());
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(client::run(source, sink, opts, rx));
    FeedHandle {
        stop,
        task: Some(task),
    }
}

#[cfg(test)]
mod tests;
