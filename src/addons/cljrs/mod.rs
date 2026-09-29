//! Everything that touches cljrs types.
//!
//! - [`bridge`]: JSON <-> Clojure values.
//! - [`harness`]: the `dirge.harness` namespace addon code calls.
//! - [`isolate`]: the thread that owns the runtime.
//! - `host.cljc`: the Clojure half of the host, embedded in the binary.
//! - [`view_isolate`]: the view seam's reducer, `view.cljc`, on a runtime
//!   and thread of its own.

pub mod bridge;
pub mod harness;
pub mod isolate;
pub mod view_isolate;

pub use isolate::Isolate;
