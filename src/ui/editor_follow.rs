//! Opt-in "editor follow-along": when `config.editor_open_command` is
//! set, dirge opens files it reads or edits in an external GUI editor
//! (detached, non-blocking) so the editor follows along like Zed's AI
//! panel.
//!
//! This is fire-and-forget — it never blocks the agent loop, never
//! returns errors to the caller, and is a no-op when the config key
//! is absent.

/// Split `template` on whitespace into argv tokens, replacing `{path}`
/// and `{line}` in each token. Returns an empty vec when `template` is
/// blank.
pub fn build_editor_open_argv(template: &str, abs_path: &str, line: Option<usize>) -> Vec<String> {
    let template = template.trim();
    if template.is_empty() {
        return Vec::new();
    }
    let line_str = line.map(|n| n.to_string()).unwrap_or_default();
    template
        .split_whitespace()
        .map(|token| {
            token
                .replace("{path}", abs_path)
                .replace("{line}", &line_str)
        })
        .collect()
}

/// Return `(path, optional line)` for file-touching tools, or `None`
/// for tools that don't touch a file.
pub fn file_target_for_tool(
    name: &str,
    args: &serde_json::Value,
) -> Option<(String, Option<usize>)> {
    match name {
        "read" => {
            let path = args.get("path")?.as_str()?.to_string();
            let line = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            Some((path, line))
        }
        "write" => {
            let path = args.get("path")?.as_str()?.to_string();
            Some((path, None))
        }
        "edit" => {
            let path = args.get("path")?.as_str()?.to_string();
            Some((path, None))
        }
        "edit_lines" => {
            let path = args.get("path")?.as_str()?.to_string();
            let line = args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            Some((path, line))
        }
        "edit_minified" => {
            let path = args.get("path")?.as_str()?.to_string();
            Some((path, None))
        }
        "apply_patch" => {
            let ops = args.get("operations")?.as_array()?;
            let first_with_path = ops.iter().find_map(|op| op.get("path")?.as_str())?;
            Some((first_with_path.to_string(), None))
        }
        _ => None,
    }
}

/// Spawn the editor command fully detached. No-op on empty argv.
/// Uses `setsid()` on unix so the editor survives dirge's exit.
pub fn spawn_editor_follow(argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    match cmd.spawn() {
        Ok(mut child) => {
            // Reap on exit in a detached thread so the launched editor CLI —
            // which for a GUI editor (zed/code/…) signals its running instance
            // and returns immediately — doesn't linger as a zombie in dirge
            // across a long session. The thread blocks on wait() then exits.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::warn!(
                target: "dirge::editor_follow",
                "failed to spawn editor follow-along: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_editor_open_argv ──────────────────────────────────────

    #[test]
    fn argv_zed_with_line() {
        let argv = build_editor_open_argv("zed {path}:{line}", "/foo/bar.rs", Some(42));
        assert_eq!(argv, vec!["zed", "/foo/bar.rs:42"]);
    }

    #[test]
    fn argv_code_goto_with_line() {
        let argv = build_editor_open_argv("code --goto {path}:{line}", "/a/b.rs", Some(99));
        assert_eq!(argv, vec!["code", "--goto", "/a/b.rs:99"]);
    }

    #[test]
    fn argv_short_flag() {
        let argv = build_editor_open_argv("code -g {path}:{line}", "/x/y.rs", Some(7));
        assert_eq!(argv, vec!["code", "-g", "/x/y.rs:7"]);
    }

    #[test]
    fn argv_line_none_becomes_empty() {
        let argv = build_editor_open_argv("zed {path}:{line}", "/foo/bar.rs", None);
        assert_eq!(argv, vec!["zed", "/foo/bar.rs:"]);
    }

    #[test]
    fn argv_no_line_placeholder() {
        let argv = build_editor_open_argv("hx {path}", "/a/b.rs", Some(10));
        assert_eq!(argv, vec!["hx", "/a/b.rs"]);
    }

    #[test]
    fn argv_blank_template() {
        let argv = build_editor_open_argv("   ", "/a/b.rs", Some(1));
        assert!(argv.is_empty());
    }

    #[test]
    fn argv_empty_template() {
        let argv = build_editor_open_argv("", "/a/b.rs", Some(1));
        assert!(argv.is_empty());
    }

    #[test]
    fn argv_path_with_spaces() {
        // Template splitting on whitespace means a path with spaces
        // will be a single token — the argv is built from the template
        // tokens after substitution, so the result is correct if the
        // resolved path contains no spaces (which is the normal case).
        let argv = build_editor_open_argv("code -g {path}:{line}", "/path/no spaces.rs", Some(5));
        assert_eq!(argv, vec!["code", "-g", "/path/no spaces.rs:5"]);
    }

    // ── file_target_for_tool ────────────────────────────────────────

    fn args_json(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn target_read() {
        let args = args_json(r#"{"path": "/f.rs", "offset": 10}"#);
        let (path, line) = file_target_for_tool("read", &args).unwrap();
        assert_eq!(path, "/f.rs");
        assert_eq!(line, Some(10));
    }

    #[test]
    fn target_read_no_offset() {
        let args = args_json(r#"{"path": "/f.rs"}"#);
        let (path, line) = file_target_for_tool("read", &args).unwrap();
        assert_eq!(path, "/f.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_write() {
        let args = args_json(r#"{"path": "/w.rs"}"#);
        let (path, line) = file_target_for_tool("write", &args).unwrap();
        assert_eq!(path, "/w.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_edit() {
        let args = args_json(r#"{"path": "/e.rs", "old_text": "x", "new_text": "y"}"#);
        let (path, line) = file_target_for_tool("edit", &args).unwrap();
        assert_eq!(path, "/e.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_edit_lines() {
        let args = args_json(r#"{"path": "/el.rs", "start_line": 15, "end_line": 20}"#);
        let (path, line) = file_target_for_tool("edit_lines", &args).unwrap();
        assert_eq!(path, "/el.rs");
        assert_eq!(line, Some(15));
    }

    #[test]
    fn target_edit_lines_no_start_line() {
        let args = args_json(r#"{"path": "/el.rs", "end_line": 20}"#);
        let (path, line) = file_target_for_tool("edit_lines", &args).unwrap();
        assert_eq!(path, "/el.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_edit_minified() {
        let args = args_json(r#"{"path": "/em.rs", "old_text": "x", "new_text": "y"}"#);
        let (path, line) = file_target_for_tool("edit_minified", &args).unwrap();
        assert_eq!(path, "/em.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_apply_patch() {
        let args = args_json(
            r#"{"operations": [{"action": "update", "path": "/p.rs", "old_text": "a", "new_text": "b"}]}"#,
        );
        let (path, line) = file_target_for_tool("apply_patch", &args).unwrap();
        assert_eq!(path, "/p.rs");
        assert_eq!(line, None);
    }

    #[test]
    fn target_apply_patch_no_ops() {
        let args = args_json(r#"{"operations": []}"#);
        assert!(file_target_for_tool("apply_patch", &args).is_none());
    }

    #[test]
    fn target_unknown_tool() {
        let args = args_json(r#"{"path": "/x.rs"}"#);
        assert!(file_target_for_tool("bash", &args).is_none());
    }

    #[test]
    fn target_missing_path() {
        let args = args_json(r#"{"offset": 10}"#);
        assert!(file_target_for_tool("read", &args).is_none());
    }

    #[test]
    fn target_non_string_path() {
        let args = args_json(r#"{"path": 42}"#);
        assert!(file_target_for_tool("read", &args).is_none());
    }

    #[test]
    fn target_question_tool() {
        // question tool has no path — should return None
        let args = args_json(r#"{"questions": [{"question": "q", "options": []}]}"#);
        assert!(file_target_for_tool("question", &args).is_none());
    }

    #[test]
    fn target_task_tool() {
        let args = args_json(r#"{"prompt": "do something"}"#);
        assert!(file_target_for_tool("task", &args).is_none());
    }
}
