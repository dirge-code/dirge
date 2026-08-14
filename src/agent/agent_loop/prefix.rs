//! What the cached request prefix is, and what moved when it changes.
//!
//! Anthropic (and every other prefix-caching provider) caches on a strict
//! byte prefix of `tools → system → messages`. Anything that shifts a byte in
//! the head invalidates the cache from that point on, and the whole
//! conversation is re-billed at write price rather than read price.
//!
//! dirge already notices the *symptom*: `is_silent_cache_miss` in `run.rs`
//! flags a turn deep into a session that wrote a cache entry and read none.
//! What it cannot say is *why*, so its warning has to guess out loud —
//! "suspect the 20-block lookback window or concurrent subagent fan-out".
//! This module supplies the missing half: a fingerprint of the head, taken
//! per request, so a miss can name the component that actually moved.
//!
//! ## Two things the previous telemetry could not see
//!
//! The `prompt_cache_prefix` event this replaces hashed the sorted tool
//! *names*. That misses both of the changes most likely to happen:
//!
//! - **Schema drift.** A tool keeps its name while its description or
//!   parameter schema changes — an MCP server re-registering, a description
//!   built from live state. Identical name hash, different wire bytes, dead
//!   cache.
//! - **Order.** The names were sorted before hashing, explicitly to suppress
//!   "spurious" iteration-order differences. But the wire carries
//!   `outgoing_tools` in its actual order, so a reorder is not spurious at
//!   all — it is exactly what breaks the cache, and sorting made it the one
//!   thing the telemetry structurally could not report.
//!
//! So the fingerprint hashes the full schema in wire order. It is a
//! `DefaultHasher` (SipHash 1-3) digest: cheap, stable within a process,
//! and not cryptographic — this is telemetry, not integrity.

use rig::completion::ToolDefinition;
use std::hash::{Hash, Hasher};

/// A digest of the cacheable head of one request, kept per component so a
/// change can be attributed rather than merely detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixFingerprint {
    /// The system prompt / preamble, verbatim.
    system: u64,
    /// Every tool's name, description and parameter schema, in wire order.
    tools: u64,
}

/// Which components of the prefix differ between two requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixChange {
    pub(crate) system: bool,
    pub(crate) tools: bool,
}

impl PrefixChange {
    /// Did anything move at all?
    pub(crate) fn any(&self) -> bool {
        self.system || self.tools
    }

    /// The component list, for a log line. Stable order so two reports of
    /// the same change read identically.
    pub(crate) fn describe(&self) -> &'static str {
        match (self.system, self.tools) {
            (true, true) => "system prompt and tool schemas",
            (true, false) => "system prompt",
            (false, true) => "tool schemas",
            (false, false) => "nothing",
        }
    }
}

impl PrefixFingerprint {
    /// Fingerprint the head of a request.
    ///
    /// `tools` must be in the order they will be sent, because that is the
    /// order the provider hashes its cache key over.
    pub(crate) fn of(system: &str, tools: &[ToolDefinition]) -> Self {
        let mut h_system = std::collections::hash_map::DefaultHasher::new();
        system.hash(&mut h_system);

        let mut h_tools = std::collections::hash_map::DefaultHasher::new();
        for (i, tool) in tools.iter().enumerate() {
            // Position is part of the digest: a reorder must register, since
            // the provider's cache key is order-sensitive.
            i.hash(&mut h_tools);
            tool.name.hash(&mut h_tools);
            tool.description.hash(&mut h_tools);
            // `parameters` is a serde_json::Value. Its Display is stable for
            // a given value because serde_json preserves object key order as
            // parsed (and dirge builds these from ordered literals), which is
            // also exactly what goes on the wire.
            tool.parameters.to_string().hash(&mut h_tools);
            // Field separator, so ("ab","c") and ("a","bc") differ.
            0xffu8.hash(&mut h_tools);
        }

        PrefixFingerprint {
            system: h_system.finish(),
            tools: h_tools.finish(),
        }
    }

    /// What differs between this request's head and the previous one's.
    pub(crate) fn changes_from(&self, prev: &PrefixFingerprint) -> PrefixChange {
        PrefixChange {
            system: self.system != prev.system,
            tools: self.tools != prev.tools,
        }
    }

    /// The tool digest alone, for a log line that wants to show the value.
    pub(crate) fn tools_hash(&self) -> u64 {
        self.tools
    }

    /// The system digest alone, for a log line that wants to show the value.
    pub(crate) fn system_hash(&self) -> u64 {
        self.system
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, params: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters: params,
        }
    }

    fn simple(name: &str) -> ToolDefinition {
        tool(name, "does a thing", serde_json::json!({"type": "object"}))
    }

    /// The baseline: identical heads fingerprint identically, or every
    /// request would report a change and the signal would be worthless.
    #[test]
    fn an_unchanged_head_is_unchanged() {
        let tools = [simple("read"), simple("write")];
        let a = PrefixFingerprint::of("you are a helpful agent", &tools);
        let b = PrefixFingerprint::of("you are a helpful agent", &tools);
        assert_eq!(a, b);
        assert!(!a.changes_from(&b).any());
        assert_eq!(a.changes_from(&b).describe(), "nothing");
    }

    /// A tool that keeps its name while its DESCRIPTION changes still moves
    /// the wire bytes and still kills the cache.
    ///
    /// The previous telemetry hashed names only, so this was invisible —
    /// which matters because it is the common case: an MCP server
    /// re-registering, or a description built from live state.
    #[test]
    fn a_tool_description_change_registers() {
        let before = [tool("read", "read a file", serde_json::json!({}))];
        let after = [tool("read", "read a file or a URL", serde_json::json!({}))];
        let a = PrefixFingerprint::of("sys", &before);
        let b = PrefixFingerprint::of("sys", &after);
        assert_ne!(a, b, "a description change moves the wire bytes");
        let change = b.changes_from(&a);
        assert!(change.tools, "it must be attributed to the tools");
        assert!(!change.system, "the system prompt did not move");
    }

    /// Same for the parameter schema — a new optional field, a changed enum.
    #[test]
    fn a_tool_parameter_schema_change_registers() {
        let before = [tool(
            "read",
            "d",
            serde_json::json!({"properties": {"path": {}}}),
        )];
        let after = [tool(
            "read",
            "d",
            serde_json::json!({"properties": {"path": {}, "offset": {}}}),
        )];
        assert_ne!(
            PrefixFingerprint::of("sys", &before),
            PrefixFingerprint::of("sys", &after),
            "a parameter schema change moves the wire bytes"
        );
    }

    /// REORDERING the tool list changes the cache key.
    ///
    /// The previous telemetry sorted names before hashing, specifically to
    /// suppress this. But the provider hashes the list in the order it is
    /// sent, so a reorder is not spurious drift — it is a total prefix
    /// invalidation, and it was the one thing the old event could not report.
    #[test]
    fn reordering_the_tool_list_registers() {
        let forward = [simple("read"), simple("write")];
        let backward = [simple("write"), simple("read")];
        let a = PrefixFingerprint::of("sys", &forward);
        let b = PrefixFingerprint::of("sys", &backward);
        assert_ne!(a, b, "the provider caches over the order actually sent");
        assert!(b.changes_from(&a).tools);
    }

    /// A changed system prompt is attributed to the system prompt, and does
    /// not smear onto the tools.
    #[test]
    fn a_system_prompt_change_is_attributed_to_the_system_prompt() {
        let tools = [simple("read")];
        let a = PrefixFingerprint::of("you are a helpful agent", &tools);
        let b = PrefixFingerprint::of("you are a terse agent", &tools);
        let change = b.changes_from(&a);
        assert!(change.system);
        assert!(!change.tools, "the tools did not move");
        assert_eq!(change.describe(), "system prompt");
    }

    /// Both moving at once reports both — a preamble rebuild that also
    /// re-registers tools (`/agent switch`) is one event, not two half-truths.
    #[test]
    fn both_moving_reports_both() {
        let a = PrefixFingerprint::of("sys one", &[simple("read")]);
        let b = PrefixFingerprint::of("sys two", &[simple("read"), simple("write")]);
        let change = b.changes_from(&a);
        assert!(change.system && change.tools);
        assert_eq!(change.describe(), "system prompt and tool schemas");
    }

    /// Adding a tool and removing a different one must not collide just
    /// because the count stayed the same.
    #[test]
    fn a_swap_that_preserves_the_count_registers() {
        let a = PrefixFingerprint::of("sys", &[simple("read"), simple("write")]);
        let b = PrefixFingerprint::of("sys", &[simple("read"), simple("grep")]);
        assert_ne!(a, b);
    }

    /// Field separation: the digest must not be defeated by shifting a
    /// character across a field boundary.
    #[test]
    fn adjacent_fields_do_not_run_together() {
        let a = PrefixFingerprint::of("sys", &[tool("ab", "c", serde_json::json!({}))]);
        let b = PrefixFingerprint::of("sys", &[tool("a", "bc", serde_json::json!({}))]);
        assert_ne!(
            a, b,
            "name and description must not concatenate ambiguously"
        );
    }
}
