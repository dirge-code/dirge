//! Skill system — procedural memory for coding patterns.
//!
//! Port of Hermes's skill system (skill_manager_tool.py,
//! skill_usage.py, skill_provenance.py, skills_guard.py).
//!
//! Skills are stored per-project in `.dirge/skills/<name>/SKILL.md`.
//! The existing `crate::skill` module handles discovery from
//! global + project directories; this module adds CRUD operations
//! (create, edit, patch, delete) with security scanning and
//! atomic writes.

pub mod curator;
pub mod format;
pub mod guard;
pub mod manager;
