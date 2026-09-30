//! The view seam's ports. Narrow on purpose (ISP): the UI loop only
//! submits events, an engine only folds them.

use tokio::sync::mpsc::UnboundedSender;

use super::domain::{ViewEvent, ViewUpdate};

/// Folds view events into updates. Pure in spirit: an implementation
/// keeps its own view state and touches nothing else. Implementations
/// are interchangeable (the native reducer, the cljrs `dirge.view`).
pub trait Reducer {
    fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String>;
}

/// Where the UI loop sends view events. `submit` never waits.
pub trait ViewEngine: Send + Sync {
    fn submit(&self, event: ViewEvent);
}

/// Where an engine sends its updates: the UI loop's `select!`.
pub type UpdateSink = UnboundedSender<ViewUpdate>;
