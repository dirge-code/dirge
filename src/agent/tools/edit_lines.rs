//! Hash-anchored line editing.
//!
//! `edit_lines` replaces a line *range* in a file, guarded by the
//! per-line content hashes the model saw via `read(line_hashes=true)`.
//! Instead of reproducing the old text (as `edit` requires), the
//! model passes `start_line`, `end_line`, the `expected_hashes` for
//! that range, and the `new_text`. The tool recomputes the hashes
//! from disk; if any line drifted since the read, the edit is
//! rejected with a per-line diff instead of silently clobbering
//! changed content.
//!
//! Why: on cheaper models this cuts retries and output tokens
//! sharply — the model never re-emits the old block, only line
//! numbers + tiny hashes + the replacement. The hash is a staleness
//! guard, not a locator (lines are addressed by number); see
//! [`crate::agent::tools::line_hash`].

#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::line_hash::line_hash;
use crate::agent::tools::{AskSender, EditLinesArgs, PermCheck, ToolError, require_and_resolve};
#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;

pub struct EditLinesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
    #[cfg(feature = "lsp")]
    #[allow(dead_code)]
    lsp_manager: Option<Arc<LspManager>>,
}

impl EditLinesTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        EditLinesTool {
            permission,
            ask_tx,
            cache: None,
            #[cfg(feature = "lsp")]
            lsp_manager: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        #[cfg(feature = "lsp")] lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        EditLinesTool {
            permission,
            ask_tx,
            cache: Some(cache),
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }
}

/// Apply a hash-anchored line replacement to `content` (already
/// LF-normalized, no CR). Returns the new content on success, or a
/// model-facing error string on a hash mismatch / bad range.
///
/// Pure and synchronous so it can be unit-tested without touching the
/// filesystem or the permission layer.
pub(crate) fn apply_line_edit(
    content: &str,
    start_line: usize,
    end_line: usize,
    expected_hashes: &[String],
    new_text: &str,
) -> Result<String, String> {
    if start_line == 0 {
        return Err("start_line is 1-indexed and must be >= 1".to_string());
    }
    if end_line < start_line {
        return Err(format!(
            "end_line ({end_line}) must be >= start_line ({start_line})"
        ));
    }
    // `lines()` matches the read tool's numbering: splits on '\n',
    // strips a trailing '\r', and yields no trailing empty element
    // for a file ending in a newline.
    let lines: Vec<&str> = content.lines().collect();
    if end_line > lines.len() {
        return Err(format!(
            "end_line ({end_line}) is past the end of the file ({} lines). \
             Re-read with line_hashes to get current line numbers.",
            lines.len()
        ));
    }
    let span = end_line - start_line + 1;
    if expected_hashes.len() != span {
        return Err(format!(
            "expected_hashes has {} entries but the range {start_line}..={end_line} \
             covers {span} lines — pass exactly one hash per line in the range.",
            expected_hashes.len()
        ));
    }

    // Verify every line in the range still hashes to what the model
    // saw. Collect ALL mismatches so the model fixes them in one shot.
    let mut mismatches = Vec::new();
    for (offset, expected) in expected_hashes.iter().enumerate() {
        let line_no = start_line + offset;
        let actual = line_hash(lines[line_no - 1]);
        if &actual != expected {
            mismatches.push(format!(
                "  line {line_no}: expected hash `{expected}`, found `{actual}` — \
                 current content: {:?}",
                lines[line_no - 1]
            ));
        }
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "edit_lines rejected: {} line(s) changed since you read them. \
             Re-read with line_hashes and retry.\n{}",
            mismatches.len(),
            mismatches.join("\n")
        ));
    }

    // Build the new content. `new_text` is the replacement block; an
    // empty block deletes the range. A single trailing newline is
    // dropped so the block contributes exactly its own lines (the
    // join re-inserts separators) — pass content, not formatting.
    let nt = new_text.replace("\r\n", "\n");
    let nt = nt.strip_suffix('\n').unwrap_or(&nt);
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    out.extend_from_slice(&lines[..start_line - 1]);
    if !new_text.is_empty() {
        out.extend(nt.split('\n'));
    }
    out.extend_from_slice(&lines[end_line..]);

    let mut output = out.join("\n");
    // Preserve the file's trailing-newline state.
    if content.ends_with('\n') && !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

impl Tool for EditLinesTool {
    const NAME: &'static str = "edit_lines";

    type Error = ToolError;
    type Args = EditLinesArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "edit_lines".to_string(),
            description: with_contract_hint(
                "edit_lines",
                "Replace a range of lines by line number, guarded by per-line content hashes. \
                 First read the file with line_hashes=true to get `N hhh: ...` lines, then call \
                 edit_lines with start_line/end_line (1-indexed, inclusive), expected_hashes (one \
                 per line in the range, in order), and new_text (the replacement block; empty \
                 deletes the range). Cheaper than `edit` for large blocks — you don't retype the \
                 old text. The edit is rejected if any line changed since you read it.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to edit (must be absolute, not relative)", "dirge-hints": {"semantic": "absolute_path"} },
                    "start_line": { "type": "integer", "description": "First line to replace (1-indexed, inclusive)" },
                    "end_line": { "type": "integer", "description": "Last line to replace (1-indexed, inclusive)" },
                    "expected_hashes": { "type": "array", "items": {"type": "string"}, "description": "The 3-char content hash for each line in [start_line, end_line], in order, exactly as shown by read(line_hashes=true)" },
                    "new_text": { "type": "string", "description": "Replacement text for the range. Empty string deletes the lines." }
                },
                "required": ["path", "start_line", "end_line", "expected_hashes", "new_text"]
            }),
        }
    }

    async fn call(&self, args: EditLinesArgs) -> Result<String, ToolError> {
        let resolved_path = require_and_resolve(
            &self.permission,
            &self.ask_tx,
            "edit",
            &args.path,
            "the edit path",
        )
        .await?;

        // Read-before-edit gate: the hashes only mean something if
        // the model read this file's current contents this session.
        if let Some(ref cache) = self.cache
            && !cache.has_been_read(std::path::Path::new(&resolved_path))
        {
            return Err(ToolError::Msg(format!(
                "edit_lines was blocked because \"{}\" has not been read in this session yet. \
                 Call read(line_hashes=true) on this path first.",
                args.path
            )));
        }

        const MAX_EDIT_BYTES: u64 = 100 * 1024 * 1024;
        if let Ok(meta) = tokio::fs::metadata(&resolved_path).await
            && meta.len() > MAX_EDIT_BYTES
        {
            return Err(ToolError::Msg(format!(
                "file too large for edit_lines: {} bytes (cap {} bytes)",
                meta.len(),
                MAX_EDIT_BYTES,
            )));
        }

        let bytes = tokio::fs::read(&resolved_path).await?;
        let has_crlf = bytes.windows(2).any(|w| w == b"\r\n");
        let content = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");

        let new_content = apply_line_edit(
            &content,
            args.start_line,
            args.end_line,
            &args.expected_hashes,
            &args.new_text,
        )
        .map_err(ToolError::Msg)?;

        // Re-apply CRLF if the file used it, so we don't silently
        // rewrite line endings.
        let candidate = if has_crlf {
            new_content.replace('\n', "\r\n")
        } else {
            new_content
        };

        // Tree-sitter pre-write validation: refuse syntactically
        // broken results so the model sees the error this turn.
        // dirge-p5fu: a purely unclosed-delimiter imbalance is
        // mechanically closed (parity with the JSON truncation repair)
        // and reported, rather than bounced back to the model.
        let (output, syntax_note) =
            crate::agent::tools::syntax_gate(std::path::Path::new(&resolved_path), &candidate)
                .map_err(ToolError::Msg)?;

        // Snapshot pre-edit content for /rewind before mutating, reusing
        // the bytes already read above instead of re-reading from disk.
        crate::agent::tools::snapshots::capture_bytes(std::path::Path::new(&resolved_path), &bytes);
        crate::fs_atomic::atomic_write(std::path::Path::new(&resolved_path), output.as_bytes())
            .await?;
        crate::agent::tools::modified::mark_modified(std::path::Path::new(&resolved_path));
        if let Some(ref cache) = self.cache {
            cache.clear();
            cache.mark_read(std::path::Path::new(&resolved_path));
        }

        let new_span = if args.new_text.is_empty() {
            0
        } else {
            args.new_text
                .replace("\r\n", "\n")
                .trim_end_matches('\n')
                .split('\n')
                .count()
        };
        let mut msg = format!(
            "Replaced lines {}-{} ({} line(s) → {} line(s)).",
            args.start_line,
            args.end_line,
            args.end_line - args.start_line + 1,
            new_span,
        );
        crate::agent::tools::append_repair_note(&mut msg, syntax_note);
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::line_hash::line_hash;

    fn hashes_for(content: &str, start: usize, end: usize) -> Vec<String> {
        content
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .map(line_hash)
            .collect()
    }

    #[test]
    fn replaces_single_line_on_matching_hash() {
        let c = "a\nb\nc\n";
        let h = hashes_for(c, 2, 2);
        let out = apply_line_edit(c, 2, 2, &h, "B").unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn replaces_multi_line_range_with_more_lines() {
        let c = "one\ntwo\nthree\n";
        let h = hashes_for(c, 1, 2);
        let out = apply_line_edit(c, 1, 2, &h, "X\nY\nZ").unwrap();
        assert_eq!(out, "X\nY\nZ\nthree\n");
    }

    #[test]
    fn empty_new_text_deletes_the_range() {
        let c = "keep1\ndrop\nkeep2\n";
        let h = hashes_for(c, 2, 2);
        let out = apply_line_edit(c, 2, 2, &h, "").unwrap();
        assert_eq!(out, "keep1\nkeep2\n");
    }

    #[test]
    fn rejects_on_hash_mismatch_without_mutating() {
        let c = "a\nb\nc\n";
        // Pretend the model read a different content for line 2.
        let stale = vec![line_hash("OLD_b")];
        let err = apply_line_edit(c, 2, 2, &stale, "B").unwrap_err();
        assert!(err.contains("line 2"), "got: {err}");
        assert!(err.contains("changed since you read"), "got: {err}");
    }

    #[test]
    fn reports_every_drifted_line() {
        let c = "a\nb\nc\n";
        // Both line 1 and line 3 hashes are wrong; line 2 correct.
        let mixed = vec![line_hash("WRONG"), line_hash("b"), line_hash("ALSO_WRONG")];
        let err = apply_line_edit(c, 1, 3, &mixed, "x").unwrap_err();
        assert!(err.contains("line 1"), "got: {err}");
        assert!(err.contains("line 3"), "got: {err}");
        assert!(!err.contains("line 2"), "line 2 matched; got: {err}");
    }

    #[test]
    fn rejects_wrong_hash_count() {
        let c = "a\nb\nc\n";
        let err = apply_line_edit(c, 1, 3, &[line_hash("a")], "x").unwrap_err();
        assert!(err.contains("one hash per line"), "got: {err}");
    }

    #[test]
    fn rejects_out_of_range() {
        let c = "a\nb\n";
        let err = apply_line_edit(c, 1, 9, &vec!["x".into(); 9], "z").unwrap_err();
        assert!(err.contains("past the end"), "got: {err}");
    }

    #[test]
    fn preserves_missing_trailing_newline() {
        let c = "a\nb"; // no trailing newline
        let h = hashes_for(c, 2, 2);
        let out = apply_line_edit(c, 2, 2, &h, "B").unwrap();
        assert_eq!(out, "a\nB");
    }

    /// End-to-end through `call()`: a real file round-trips, and a
    /// CRLF file keeps its CRLF endings after the edit.
    #[tokio::test]
    async fn call_round_trips_and_preserves_crlf() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("dirge_edit_lines_{}.txt", std::process::id()));
        std::fs::write(&path, "a\r\nb\r\nc\r\n").unwrap();

        // Hashes are computed on LF-normalized content (as the read
        // tool shows them).
        let normalized = "a\nb\nc\n";
        let h = hashes_for(normalized, 2, 2);

        let tool = EditLinesTool::new(None, None); // no cache → gate skipped
        let out = tool
            .call(EditLinesArgs {
                path: path.to_string_lossy().into_owned(),
                start_line: 2,
                end_line: 2,
                expected_hashes: h,
                new_text: "B".to_string(),
            })
            .await
            .expect("edit_lines call succeeds");
        assert!(out.contains("Replaced lines 2-2"), "summary: {out}");

        let after = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(after, "a\r\nB\r\nc\r\n", "CRLF must be preserved");
    }
}
