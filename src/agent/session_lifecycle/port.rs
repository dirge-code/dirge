//! The port the session lifecycle announces its events through.

use super::domain::{LifecycleHook, SessionEnd, SessionStart};

/// Whoever hears a session start and end. Calls block until every listener
/// has answered; [`super::Lifecycle`] makes them on a blocking thread,
/// bounds them, and logs what does not come back, so an implementor only
/// answers and logs its own listeners' failures.
pub trait SessionLifecycle: Send + Sync + 'static {
    /// True when some listener hears `hook`, so the announcement can be
    /// skipped otherwise.
    fn listens(&self, hook: LifecycleHook) -> bool;

    /// The texts the listeners answered `event` with, in their order.
    fn session_start(&self, event: &SessionStart) -> Vec<String>;

    /// Tell every listener the session ended.
    fn session_end(&self, event: &SessionEnd);
}
