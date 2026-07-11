//! Permission classification helpers.
//!
//! Pure functions for classifying tools (is this a path tool? is it
//! high-risk?) and building `Pattern` values with the right `*`
//! semantics for the tool category. Extracted from `checker.rs` so
//! they can be unit-tested independently of the `PermissionChecker`
//! struct's configuration wiring.
//!
//! Used by `checker.rs` (through thin delegating methods) and by
//! `allowlist.rs` (for `pattern_for_tool`).

use crate::permission::pattern::Pattern;

/// Tool names where the input is a filesystem path. For these, `*` keeps
/// classic glob semantics (one segment, doesn't cross `/`). Everything else
/// is treated as shell/text where `*` means "any chars including /".
pub fn is_path_tool_name(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "write"
            | "edit"
            | "list_dir"
            | "apply_patch"
            | "lsp"
            // grep / find_files / glob now also receive path-side
            // checks (the search-root path), so their rules use
            // path-glob semantics.
            | "grep"
            | "find_files"
            | "glob"
            // Semantic tools whose primary arg is a file path.
            | "list_symbols"
            | "get_symbol_body"
            | "find_definition"
            | "find_callers"
            | "find_callees"
            // #1 fix: repo_overview's arg is a directory path; user
            // rules like `"/etc/**": "deny"` need path-glob semantics
            // for `**` to span subpaths. Was missed when the tool
            // was added.
            | "repo_overview"
    )
}

/// Build a Pattern with the right `*` semantics for the given tool.
pub fn pattern_for_tool(tool: &str, pat: &str) -> Pattern {
    if is_path_tool_name(tool) {
        Pattern::new(pat)
    } else {
        Pattern::new_command(pat)
    }
}
