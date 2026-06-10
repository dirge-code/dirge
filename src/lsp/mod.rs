//! Language Server Protocol support.

pub mod client;
pub mod diagnostic;
#[cfg(feature = "plugin")]
pub mod harness;
pub mod init;
pub mod language;
pub mod manager;
pub mod query;
pub mod rpc;
pub mod server;
pub mod spawn;
pub mod uri;
