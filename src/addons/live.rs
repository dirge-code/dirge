//! The signal that the addons changed in place: a REPL evaluation or
//! `dirge.harness/refresh!` re-read their tools, hooks and commands without
//! a reload. The runtime raises it from its own thread; the event loop waits
//! on it and hands the change to the running agent.

use std::sync::LazyLock;

use tokio::sync::Notify;

static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Say the addons changed. Callable from any thread; a signal raised while
/// nobody waits is kept for the next wait.
pub fn notify() {
    CHANGED.notify_one();
}

/// Resolves once the addons changed since the last wait returned.
pub async fn changed() {
    CHANGED.notified().await;
}
