#[cfg(feature = "loop")]
pub mod r#loop;

#[cfg(feature = "git-worktree")]
pub mod git_worktree;

#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "mcp-server")]
pub mod mcp_server;

#[cfg(feature = "acp")]
pub mod acp;

#[cfg(feature = "experimental-graph-search")]
pub mod entity_compress;
#[cfg(feature = "experimental-graph-search")]
pub mod entity_db;
#[cfg(feature = "experimental-graph-search")]
pub mod entity_router;
#[cfg(feature = "experimental-graph-search")]
pub mod entity_search;

pub mod content_guard;
pub mod curator_clock;
pub mod dirge_paths;
pub mod fts;
pub mod issue_db;
pub mod memory_curator;
pub mod memory_db;
pub mod memory_graduation;
pub mod memory_hybrid;
pub mod memory_provider;
#[cfg(test)]
mod memory_retrieval_eval;
pub mod salience;
pub mod session_db;
pub mod session_search;
pub mod skill_db;
pub mod skills;
pub mod spec_db;
