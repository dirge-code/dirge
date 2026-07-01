//! Slash command implementations — one file per command or sub-command
//! directory when there are 3+ behaviors and > 80 lines.
//!
//! Each file exports `pub(crate)` function(s); `mod.rs` in parent `slash/`
//! delegates to them via one-line match arms.

pub(crate) mod agent;
pub(crate) mod allow;
#[cfg(feature = "dap")]
pub(crate) mod debug;
pub(crate) mod loop_cmd;
pub(crate) mod prompt;
#[cfg(unix)]
pub(crate) mod sandbox;
pub(crate) mod sessions;

pub(crate) mod agents;
pub(crate) mod btw;
pub(crate) mod cache;
pub(crate) mod cd;
pub(crate) mod clear;
pub(crate) mod clone;
pub(crate) mod code_review;
pub(crate) mod fork;
pub(crate) mod graph;
pub(crate) mod help;
pub(crate) mod issues;
pub(crate) mod kill;
pub(crate) mod learn;
#[cfg(feature = "mcp")]
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod mode;
pub(crate) mod model;
pub(crate) mod panel;
pub(crate) mod plan;
pub(crate) mod plugins;
pub(crate) mod quit;
pub(crate) mod regen;
pub(crate) mod retry;
pub(crate) mod spec;
pub(crate) mod tasks;
pub(crate) mod toggle;
pub(crate) mod tree;
pub(crate) mod undo;
#[cfg(feature = "git-worktree")]
pub(crate) mod worktree;
