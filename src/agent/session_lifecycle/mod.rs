//! Session lifecycle: [`SessionStart`] when a session's first run in this
//! process opens, [`SessionEnd`] when dirge exits or another session takes
//! its place. The start's answers reach that run's first turn as a
//! reminder; the end is announced before the MCP servers close.
//!
//! Strata:
//! - [`domain`]: the events and what they carry;
//! - [`collect`]: facts read where they live (cwd, connected MCP servers);
//! - [`policy`]: pure decisions (the running session, end reasons, the
//!   reminder);
//! - [`port`]: [`SessionLifecycle`], whoever hears the events;
//! - [`boundary`]: [`Lifecycle`], which announces them off the caller's
//!   thread within [`Budgets`], and the calls exit paths make.
//!
//! Only the addon host listens today, so a build without `addons` installs
//! no lifecycle and every announcement is a no-op.

pub mod boundary;
pub mod collect;
pub mod domain;
pub mod policy;
pub mod port;
#[cfg(test)]
mod tests;

pub use boundary::{end, end_then, installed};
pub use domain::EndCause;
// What a listener is built from.
#[cfg_attr(not(feature = "addons"), allow(unused_imports))]
pub use boundary::{Budgets, Lifecycle, install};
#[cfg_attr(not(feature = "addons"), allow(unused_imports))]
pub use domain::{LifecycleHook, SessionEnd, SessionStart};
#[cfg_attr(not(feature = "addons"), allow(unused_imports))]
pub use port::SessionLifecycle;
