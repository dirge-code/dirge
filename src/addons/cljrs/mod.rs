//! Everything that touches cljrs types.
//!
//! - [`bridge`]: JSON <-> Clojure values.
//! - [`harness`]: the `dirge.harness` namespace addon code calls.
//! - [`isolate`]: the thread that owns the runtime.
//! - `host.cljc`: the Clojure half of the host, embedded in the binary.

pub mod bridge;
pub mod harness;
pub mod isolate;

pub use isolate::Isolate;
