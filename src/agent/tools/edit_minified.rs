//! `edit_minified` — edit a file by matching against its MINIFIED form.
//!
//! The companion to `read_minified`: when the model has read a file's minified
//! view, it can edit against that compact text without re-reading the full
//! file. The match is mapped back to the original source byte range and spliced
//! in place via [`crate::semantic::minify::apply_minified_edit`] — no formatter,
//! surrounding formatting untouched. Layers of safety: read-before-edit gate,
//! unique + token-aligned match, and a tree-sitter syntax check of the result
//! BEFORE writing, so a bad edit errors instead of corrupting the file.

#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, EditArgs, PermCheck, ToolError, require_and_resolve};
#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;
use crate::semantic::minify::{MinifiedEditError, apply_minified_edit};

/// Cap on the file size we'll read+minify for an edit (mirrors `edit`).
const MAX_EDIT_BYTES: u64 = 100 * 1024 * 1024;

pub struct EditMinifiedTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
}

impl EditMinifiedTool {
    #[allow(dead_code)] // constructed via with_cache in production
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission,
            ask_tx,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        // edit_minified doesn't surface LSP diagnostics (v1); the tree-sitter
        // syntax check is the safety floor. Accept+ignore the handle so the
        // call site matches the sibling tools.
        #[cfg(feature = "lsp")] _lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            cache: Some(cache),
        }
    }
}

impl Tool for EditMinifiedTool {
    const NAME: &'static str = "edit_minified";

    type Error = ToolError;
    type Args = EditArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "edit_minified".to_string(),
            description: with_contract_hint(
                "edit_minified",
                "Edit a file by replacing text matched against its MINIFIED form (as shown by read_minified). `old_text` must be the minified text — copy it from a prior read_minified of the same file — and must be unique and align to whole tokens. The change is mapped back to the original source and applied in place, preserving the file's formatting; the result is syntax-checked before writing. For normal (non-minified) edits use `edit`.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to edit (must be absolute, not relative)" },
                    "old_text": { "type": "string", "description": "Exact text to find in the file's MINIFIED form (from read_minified). Must be unique and align to whole tokens." },
                    "new_text": { "type": "string", "description": "Replacement text (written into the original source verbatim)" },
                    "reason": { "type": "string", "description": "Why you're making this edit and how it serves the task." }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    async fn call(&self, args: EditArgs) -> Result<String, ToolError> {
        if args.old_text.is_empty() {
            return Err(ToolError::Msg(
                "old_text must not be empty. Provide the exact minified text to replace.".into(),
            ));
        }
        let resolved = require_and_resolve(
            &self.permission,
            &self.ask_tx,
            "edit",
            &args.path,
            "the edit path",
        )
        .await?;

        // Read-before-edit gate (shared with `edit`): the model must have read
        // the file (read_minified satisfies it) so the match is against content
        // it actually saw.
        if let Some(ref cache) = self.cache
            && !cache.has_been_read(std::path::Path::new(&resolved))
        {
            return Err(ToolError::Msg(format!(
                "edit_minified was blocked because \"{}\" has not been read this session. \
                 Call read_minified on it first.",
                args.path
            )));
        }

        if let Ok(meta) = tokio::fs::metadata(&resolved).await
            && meta.len() > MAX_EDIT_BYTES
        {
            return Err(ToolError::Msg(format!(
                "file too large for edit_minified: {} bytes (cap {})",
                meta.len(),
                MAX_EDIT_BYTES
            )));
        }

        let source = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
            ToolError::Msg(format!("could not read {} as UTF-8 text: {e}", args.path))
        })?;
        let ext = std::path::Path::new(&resolved)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let candidate = apply_minified_edit(ext, &source, &args.old_text, &args.new_text)
            .map_err(|e| ToolError::Msg(minified_edit_error_message(e, &args.path)))?;

        // Same pre-write gate as `edit` (via the shared choke point): refuse a
        // syntactically-broken edit, or mechanically close a purely-unclosed
        // delimiter imbalance and report it (dirge-p5fu).
        let (new_source, syntax_note) =
            crate::agent::tools::syntax_gate(std::path::Path::new(&resolved), &candidate)
                .map_err(ToolError::Msg)?;

        crate::fs_atomic::atomic_write(std::path::Path::new(&resolved), new_source.as_bytes())
            .await?;
        crate::agent::tools::modified::mark_modified(std::path::Path::new(&resolved));
        if let Some(ref cache) = self.cache {
            cache.clear();
            cache.mark_read(std::path::Path::new(&resolved));
        }

        let mut msg = format!(
            "Applied minified edit to {} ({} bytes)",
            args.path,
            new_source.len()
        );
        crate::agent::tools::append_repair_note(&mut msg, syntax_note);
        Ok(msg)
    }
}

fn minified_edit_error_message(e: MinifiedEditError, path: &str) -> String {
    match e {
        MinifiedEditError::Unsupported => format!(
            "edit_minified isn't available for {path} (its language isn't minifiable or it doesn't parse cleanly). Use the `edit` tool instead."
        ),
        MinifiedEditError::NotFound => format!(
            "old_text was not found in the minified form of {path}. Re-run read_minified to see the current minified content and copy the exact text."
        ),
        MinifiedEditError::NotUnique => format!(
            "old_text matches multiple locations in the minified form of {path}. Add more surrounding context to make it unique."
        ),
        MinifiedEditError::NotAligned => format!(
            "old_text in {path} doesn't align to whole tokens (it starts or ends mid-token, or at an inserted separator). Extend the match to complete tokens / more context."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with_cache(cache: ToolCache) -> EditMinifiedTool {
        EditMinifiedTool::with_cache(
            None,
            None,
            cache,
            #[cfg(feature = "lsp")]
            None,
        )
    }

    #[cfg(feature = "semantic-rust")]
    #[tokio::test]
    async fn edits_via_minified_match_preserving_formatting() {
        let dir = std::env::temp_dir().join(format!("dirge-editmin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        let src = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        std::fs::write(&path, src).unwrap();
        let abs = path.to_string_lossy().to_string();
        let resolved = crate::agent::tools::check_perm_path_resolve(&None, &None, "read", &abs)
            .await
            .unwrap();

        let cache = ToolCache::new();
        cache.mark_read(std::path::Path::new(&resolved)); // simulate prior read_minified
        let tool = tool_with_cache(cache);

        let out = tool
            .call(EditArgs {
                path: abs,
                old_text: "let x=1".into(), // minified form
                new_text: "let x = 42".into(),
                replace_all: None,
            })
            .await
            .unwrap();
        assert!(out.contains("minified edit"), "{out}");
        // Original formatting preserved; only the matched region changed.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    let x = 42;\n    let y = 2;\n}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "semantic-rust")]
    #[tokio::test]
    async fn blocked_until_read() {
        let dir = std::env::temp_dir().join(format!("dirge-editmin-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn main(){let x=1;}\n").unwrap();
        let abs = path.to_string_lossy().to_string();

        let tool = tool_with_cache(ToolCache::new()); // nothing marked read
        let err = tool
            .call(EditArgs {
                path: abs,
                old_text: "let x=1".into(),
                new_text: "let x=2".into(),
                replace_all: None,
            })
            .await
            .expect_err("must be gated before read");
        assert!(err.to_string().contains("has not been read"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
