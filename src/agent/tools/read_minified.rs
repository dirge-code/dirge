//! `read_minified` — token-efficient file read via tree-sitter minification.
//!
//! Ported from vix `read_minified_file`. For a whole-file read of a
//! collapse-safe, grammar-supported language ([`crate::semantic::minify`]) it
//! returns the source with comments + redundant whitespace stripped. For
//! everything else — ranged reads, unsupported languages, binary files,
//! oversized files, or sources that don't parse cleanly — it transparently
//! falls back to a full-fidelity plain [`ReadTool`] read (which handles line
//! numbering, streaming, LSP warmup, and the read-before-edit gate). It never
//! returns a corrupted / half-minified result.

#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{
    AskSender, PermCheck, ReadArgs, ReadTool, ToolError, check_perm_path_resolve,
    require_absolute_path,
};
#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;

/// Above this size we don't attempt to parse+minify (minify reads the whole
/// file into memory and parses it); fall back to the streaming plain read.
const MAX_MINIFY_BYTES: u64 = 1024 * 1024;

pub struct ReadMinifiedTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
    /// Full-fidelity reader used for every non-minify path (the bulk of the
    /// robustness lives here: binary detection, ranges, line numbers, cache,
    /// LSP warmup, read-gate marking).
    inner: ReadTool,
}

impl ReadMinifiedTool {
    #[allow(dead_code)] // constructed via with_cache in production
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission: permission.clone(),
            ask_tx: ask_tx.clone(),
            cache: None,
            inner: ReadTool::new(permission, ask_tx),
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        #[cfg(feature = "lsp")] lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        Self {
            permission: permission.clone(),
            ask_tx: ask_tx.clone(),
            cache: Some(cache.clone()),
            inner: ReadTool::with_cache(
                permission,
                ask_tx,
                cache,
                #[cfg(feature = "lsp")]
                lsp_manager,
            ),
        }
    }
}

impl Tool for ReadMinifiedTool {
    const NAME: &'static str = "read_minified";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read_minified".to_string(),
            description: with_contract_hint(
                "read_minified",
                "Read a source file with comments and redundant whitespace stripped (via tree-sitter) for token efficiency. Supported languages: Rust, C, C++, Java. Other languages, ranged reads (offset/limit), or unparseable files transparently fall back to a normal read. Use plain `read` when you need exact line numbers.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to read (must be absolute, not relative)" },
                    "offset": { "type": "integer", "description": "Line number to start from (1-indexed). Ranged reads skip minification and read normally." },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read. Ranged reads skip minification." },
                    "reason": { "type": "string", "description": "Why you're reading this file: what you expect to learn and how it serves the current task. Be specific and targeted." }
                },
                "required": ["path", "reason"]
            }),
        }
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        // Minify only a whole-file read of a grammar-supported, collapse-safe
        // language. Anything else falls through to the plain reader.
        let whole_file = args.offset.is_none() && args.limit.is_none();
        let ext = std::path::Path::new(&args.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        if whole_file && crate::semantic::minify::language_for_ext(ext).is_some() {
            require_absolute_path(&args.path, "the read path").map_err(ToolError::Msg)?;
            // Single permission resolve for the common (minify-success) path.
            let resolved =
                check_perm_path_resolve(&self.permission, &self.ask_tx, "read", &args.path).await?;

            let small_enough = tokio::fs::metadata(&resolved)
                .await
                .map(|m| m.len() <= MAX_MINIFY_BYTES)
                .unwrap_or(false);

            if small_enough
                && let Ok(content) = tokio::fs::read_to_string(&resolved).await
                && let Some(minified) = crate::semantic::minify::minify(ext, &content)
            {
                // The model has now seen the file's content → satisfy the
                // read-before-edit gate.
                if let Some(cache) = &self.cache {
                    cache.mark_read(std::path::Path::new(&resolved));
                }
                let saved = content.len().saturating_sub(minified.len());
                return Ok(format!(
                    "{} (minified — comments + redundant whitespace stripped, ~{saved} bytes saved; use `read` for exact line numbers)\n\n{minified}",
                    args.path
                ));
            }
            // Fell through (binary / non-UTF8 / parse error / oversized):
            // delegate to the plain reader (it re-resolves; benign).
        }

        self.inner.call(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "semantic-rust")]
    #[tokio::test]
    async fn minifies_supported_language_and_marks_read() {
        let dir = std::env::temp_dir().join(format!("dirge-readmin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "// header\nfn  main ( ) {\n    let  x = 1 ;\n}\n").unwrap();
        let abs = path.to_string_lossy().to_string();
        let resolved = check_perm_path_resolve(&None, &None, "read", &abs)
            .await
            .unwrap();

        let cache = ToolCache::new();
        let tool = ReadMinifiedTool::with_cache(
            None,
            None,
            cache.clone(),
            #[cfg(feature = "lsp")]
            None,
        );
        let out = tool
            .call(ReadArgs {
                path: abs.clone(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(out.contains("minified"), "header present: {out}");
        assert!(!out.contains("header"), "comment stripped: {out}");
        assert!(out.contains("fn main"), "word boundary kept: {out}");
        // Read-gate satisfied so a follow-up edit isn't blocked.
        assert!(cache.has_been_read(std::path::Path::new(&resolved)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsupported_language_falls_back_to_plain_read() {
        let dir = std::env::temp_dir().join(format!("dirge-readmin-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.md");
        std::fs::write(&path, "# Title\n\nplain markdown body\n").unwrap();
        let abs = path.to_string_lossy().to_string();

        let cache = ToolCache::new();
        let tool = ReadMinifiedTool::with_cache(
            None,
            None,
            cache,
            #[cfg(feature = "lsp")]
            None,
        );
        let out = tool
            .call(ReadArgs {
                path: abs,
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        // Fell back to plain read → markdown content present verbatim, not minified.
        assert!(out.contains("plain markdown body"), "plain content: {out}");
        assert!(!out.contains("minified"), "no minify header: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
