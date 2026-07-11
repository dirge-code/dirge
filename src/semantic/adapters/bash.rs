#[cfg(all(test, feature = "semantic-bash"))]
mod tests {
    use crate::semantic::adapters::bash::{parse_bash_segments, parse_bash_segments_full};

    #[test]
    fn test_simple_command() {
        let segments = parse_bash_segments("cargo test --all");
        assert_eq!(segments, vec!["cargo test --all"]);
    }

    #[test]
    fn test_double_ampersand_splits() {
        let segments = parse_bash_segments("cargo test && echo done");
        assert_eq!(segments, vec!["cargo test", "echo done"]);
    }

    #[test]
    fn test_semicolon_splits() {
        let segments = parse_bash_segments("echo a; echo b");
        assert_eq!(segments, vec!["echo a", "echo b"]);
    }

    #[test]
    fn test_pipe_splits() {
        let segments = parse_bash_segments("cat file | grep foo | wc -l");
        assert_eq!(segments, vec!["cat file", "grep foo", "wc -l"]);
    }

    #[test]
    fn test_mixed_separators() {
        let segments = parse_bash_segments("a && b | c");
        assert_eq!(segments, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_command_substitution_is_complex() {
        let (segments, complex) = parse_bash_segments_full("echo $(rm -rf /)").unwrap();
        assert!(complex, "command substitution should be marked complex");
        assert_eq!(segments.len(), 1);
    }

    #[test]
    fn test_single_quotes_are_safe() {
        let (segments, complex) = parse_bash_segments_full("echo 'safe $(not expanded)'").unwrap();
        assert!(!complex, "single quotes should not trigger complex");
        assert_eq!(segments.len(), 1);
    }

    #[test]
    fn test_double_quotes_are_complex() {
        let (_segments, complex) =
            parse_bash_segments_full("echo \"dangerous $(expanded)\"").unwrap();
        assert!(complex, "double quotes with substitution should be complex");
    }

    #[test]
    fn test_git_commands_parse() {
        let segments = parse_bash_segments("git diff --staged && git status");
        assert_eq!(segments, vec!["git diff --staged", "git status"]);
    }

    #[test]
    fn test_parse_error_fallback() {
        let segments = parse_bash_segments("for i in");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0], "for i in");
    }

    // --- C3: compound-form recursion ---------------------------

    /// Brace groups recurse — each contained command lands as its
    /// own segment so per-command rules fire. Previously the whole
    /// `{ ... }` was pushed verbatim and matched no rule.
    #[test]
    fn test_brace_group_recurses_into_commands() {
        let segments = parse_bash_segments("{ echo a; rm -rf /tmp/x; }");
        assert!(
            segments.iter().any(|s| s.starts_with("echo")),
            "got: {segments:?}"
        );
        assert!(
            segments.iter().any(|s| s.starts_with("rm")),
            "got: {segments:?}"
        );
        // The literal "{ ... }" wrapper should NOT appear as a segment.
        assert!(
            !segments.iter().any(|s| s.contains("{ echo")),
            "got: {segments:?}"
        );
    }

    /// if/then/fi recurses into its body — the inner commands get
    /// individual permission checks.
    #[test]
    fn test_if_statement_recurses_into_body() {
        let segments = parse_bash_segments("if true; then rm /tmp/x; echo done; fi");
        assert!(
            segments.iter().any(|s| s.starts_with("rm")),
            "got: {segments:?}"
        );
        assert!(
            segments.iter().any(|s| s.starts_with("echo")),
            "got: {segments:?}"
        );
    }

    /// while loops same.
    #[test]
    fn test_while_loop_recurses() {
        let segments = parse_bash_segments("while true; do rm -rf /tmp/x; done");
        assert!(
            segments.iter().any(|s| s.starts_with("rm")),
            "got: {segments:?}"
        );
    }

    /// for loops same.
    #[test]
    fn test_for_loop_recurses() {
        let segments = parse_bash_segments("for f in a b c; do rm $f; done");
        assert!(
            segments.iter().any(|s| s.starts_with("rm")),
            "got: {segments:?}"
        );
    }

    /// case statements: each case-clause body is recursed into.
    #[test]
    fn test_case_statement_recurses() {
        let segments = parse_bash_segments("case $x in foo) rm /tmp/x;; bar) echo b;; esac");
        assert!(
            segments.iter().any(|s| s.starts_with("rm")),
            "got: {segments:?}"
        );
        assert!(
            segments.iter().any(|s| s.starts_with("echo")),
            "got: {segments:?}"
        );
    }

    // --- C4: redirect-target extraction ------------------------

    use crate::semantic::adapters::bash::extract_redirect_targets;

    #[test]
    fn extract_redirect_targets_output_redirect() {
        let t = extract_redirect_targets("echo pwned > /etc/something");
        assert_eq!(t, vec!["/etc/something".to_string()]);
    }

    #[test]
    fn extract_redirect_targets_append() {
        let t = extract_redirect_targets("echo line >> /var/log/foo");
        assert_eq!(t, vec!["/var/log/foo".to_string()]);
    }

    #[test]
    fn extract_redirect_targets_multiple() {
        // `cmd > a 2> b` writes to BOTH a and b — both should be
        // checked by the path gate.
        let t = extract_redirect_targets("rustc src.rs > out.log 2> err.log");
        assert!(t.contains(&"out.log".to_string()), "got: {t:?}");
        assert!(t.contains(&"err.log".to_string()), "got: {t:?}");
    }

    #[test]
    fn extract_redirect_targets_strips_quotes() {
        let t = extract_redirect_targets("echo x > \"/tmp/with spaces\"");
        assert_eq!(t, vec!["/tmp/with spaces".to_string()]);
    }

    #[test]
    fn extract_redirect_targets_no_redirects() {
        assert!(extract_redirect_targets("echo hello").is_empty());
        assert!(extract_redirect_targets("cargo test --all").is_empty());
    }

    #[test]
    fn extract_redirect_targets_heredoc_skipped() {
        // <<EOF has no file target — skip.
        let t = extract_redirect_targets("cat <<EOF\nhi\nEOF");
        assert!(t.is_empty(), "got: {t:?}");
    }

    /// fd-duplication redirects (`2>&1`, `>&2`) target file
    /// descriptors, not files. They must NOT trigger a write
    /// permission check — the check against `validate_path` would
    /// reject the bare number as a "numeric path," and even if it
    /// passed, there's no file being written.
    #[test]
    fn extract_redirect_targets_skips_fd_duplication() {
        assert!(extract_redirect_targets("cargo test 2>&1").is_empty());
        assert!(extract_redirect_targets("cmd >&2").is_empty());
        assert!(extract_redirect_targets("cmd 1>&2").is_empty());
    }

    /// Redirected statement: the inner command is checked (redirect
    /// operands handled by C4 separately, not surfaced here).
    #[test]
    fn test_redirected_statement_recurses_to_inner_command() {
        let segments = parse_bash_segments("echo pwned > /etc/something");
        assert!(
            segments.iter().any(|s| s.starts_with("echo")),
            "got: {segments:?}"
        );
        // Old behaviour pushed the whole `echo pwned > /etc/something`;
        // now the segment is just the command without the redirect.
        assert!(
            !segments.iter().any(|s| s.contains("/etc/something")),
            "segment should NOT include the redirect target; got: {segments:?}"
        );
    }

    #[test]
    fn mutation_paths_see_through_env_assignment() {
        // dirge-8zem: a leading `FOO=1` makes the command node's first
        // named child a `variable_assignment`, so the old head-derivation
        // treated `FOO=1` as the head, `rm` was skipped, and `/etc/passwd`
        // never routed through the Edit gate.
        let p = crate::semantic::adapters::bash::extract_mutation_paths("FOO=1 rm -rf /etc/passwd");
        assert!(
            p.contains(&"/etc/passwd".to_string()),
            "env-prefixed rm must still surface its operand; got: {p:?}"
        );
    }

    #[test]
    fn mutation_paths_see_through_exec_wrappers() {
        for (cmd, want) in [
            ("nohup rm -rf /etc/x", "/etc/x"),
            ("time rm /etc/y", "/etc/y"),
            ("nice -n 5 rm /etc/z", "/etc/z"),
            ("timeout 10 rm -rf /etc/w", "/etc/w"),
            ("env FOO=1 rm /etc/v", "/etc/v"),
        ] {
            let p = crate::semantic::adapters::bash::extract_mutation_paths(cmd);
            assert!(
                p.contains(&want.to_string()),
                "wrapper-prefixed mutator {cmd:?} must surface {want:?}; got: {p:?}"
            );
        }
    }

    /// dirge-3yak: the semantic mutator list had drifted from the coarse
    /// one — `truncate`, `install`, `shred` were missing, so their path
    /// operands never routed through the Edit gate in the default build.
    /// Both extractors now share one list.
    #[test]
    fn mutation_paths_cover_truncate_install_shred() {
        for (cmd, want) in [
            ("truncate -s 0 /etc/passwd", "/etc/passwd"),
            ("shred /etc/shadow", "/etc/shadow"),
            ("install /dev/null /etc/hosts", "/etc/hosts"),
        ] {
            let p = crate::semantic::adapters::bash::extract_mutation_paths(cmd);
            assert!(
                p.contains(&want.to_string()),
                "mutator {cmd:?} must surface {want:?}; got: {p:?}"
            );
        }
    }
}

/// C4 (audit fix): extract redirect target paths from a bash
/// command so the caller can route each through the path permission
/// gate. Previously `echo pwned > /etc/something` matched the safe
/// `echo **` rule and wrote to the destination without any path
/// check — the redirect target was invisible to the permission
/// system.
///
/// Returns destination paths for: `>` `>>` `&>` `&>>` `>&` `<` `<<<`
/// and combined forms (`1>file`, `2>file`, etc.). Heredocs (`<<EOF`)
/// have no file target so they're skipped. Empty when no redirects.
///
/// Returns `Vec::new()` on parse error or when the `semantic-bash`
/// feature is disabled — the caller still gets the normal segment
/// checks, just no extra path gate for the redirect destination.
#[allow(dead_code)]
pub fn extract_redirect_targets(command: &str) -> Vec<String> {
    #[cfg(feature = "semantic-bash")]
    {
        use tree_sitter::Parser;
        let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        let mut parser = Parser::new();
        if parser.set_language(&lang).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(command, None) else {
            return Vec::new();
        };
        if tree.root_node().has_error() {
            return Vec::new();
        }
        let mut targets = Vec::new();
        collect_redirect_targets(tree.root_node(), command.as_bytes(), &mut targets);
        targets
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        let _ = command;
        Vec::new()
    }
}

/// F1 (dirge-dvy): extract positional path arguments to file-mutating
/// commands so the permission layer can route each through the write
/// rules — independent of the bash command-pattern check.
///
/// Concrete bypass this closes: a user adds `bash: { "rm *": "allow" }`
/// for convenience. The bash command-pattern check allows
/// `rm /etc/passwd` because it matches `rm *`. Without this extractor,
/// the path `/etc/passwd` never reaches the write rules → silently
/// deleted. With it, every mutation path routes through
/// `enforce(tool="write", Scope::PathResolve(arg))` and write's deny
/// rules apply.
///
/// Ported from opencode's `packages/opencode/src/tool/shell.ts:30-51`
/// (`FILES` set) and `:191-221` (`pathArgs` filter logic). Restricted
/// to commands that semantically WRITE files; omits `cd`/`pushd`/etc
/// (no mutation) and `cat`/`get-content`/etc (read-only). Adds `ln`,
/// `tee`, `dd` which opencode doesn't explicitly list but
/// semantically mutate.
///
/// chmod / chown special-case: their FIRST positional arg is the
/// mode (`777`, `u+x`) or owner spec (`user:group`), not a path.
/// Skip arg index 0 for those commands.
#[cfg(feature = "semantic-bash")]
pub fn extract_mutation_paths(command: &str) -> Vec<String> {
    // Single source of truth shared with the coarse fallback (dirge-3yak).
    use crate::permission::engine::types::FILE_MUTATORS;

    use tree_sitter::Parser;
    let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(command, None) else {
        return Vec::new();
    };
    if tree.root_node().has_error() {
        return Vec::new();
    }
    let mut paths = Vec::new();
    collect_mutation_paths(
        tree.root_node(),
        command.as_bytes(),
        FILE_MUTATORS,
        &mut paths,
    );
    paths
}

#[cfg(feature = "semantic-bash")]
fn collect_mutation_paths(
    node: tree_sitter::Node,
    source: &[u8],
    mutators: &[&str],
    out: &mut Vec<String>,
) {
    if node.kind() == "command" {
        // Collect head + positional args. The tree-sitter-bash
        // grammar emits the head as `command_name`, then positional
        // args as `word` / `string` / `raw_string` / `concatenation`.
        // Skip anything else (redirections, variable assignments,
        // etc. — those have their own node kinds and aren't paths
        // here).
        // Gather the ordered token texts (head + positional args +
        // leading `variable_assignment` nodes), skipping redirections
        // (handled by the redirect_targets extractor — including them
        // here would double-prompt).
        let mut tokens: Vec<String> = Vec::new();
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            if child.kind() == "file_redirect"
                || child.kind() == "heredoc_redirect"
                || child.kind() == "herestring_redirect"
            {
                continue;
            }
            let text = match child.utf8_text(source) {
                Ok(t) => t.trim().to_string(),
                Err(_) => continue,
            };
            if text.is_empty() {
                continue;
            }
            tokens.push(text);
        }

        // Skip leading env assignments (`FOO=1`) and exec wrappers
        // (`nohup`/`time`/`nice`/`timeout`/`env`) so the operand of the
        // REAL command still routes through the Edit gate (dirge-8zem).
        let refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
        let start = crate::permission::engine::types::exec_head_index(&refs);
        if let Some(h) = tokens.get(start) {
            // Strip path prefix on the head so absolute paths like
            // `/bin/rm` still match the basename rule.
            let basename = std::path::Path::new(h)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(h.as_str());
            if mutators.contains(&basename) {
                let mut args: Vec<String> = Vec::new();
                for text in &tokens[start + 1..] {
                    // Skip flag args (`-r`, `--recursive`).
                    if text.starts_with('-') {
                        continue;
                    }
                    // Skip chmod permission specs (`+x`, `u+x`).
                    if basename == "chmod" && text.starts_with('+') {
                        continue;
                    }
                    args.push(unquote_simple(text));
                }
                // chmod / chown: drop the first positional arg
                // (mode spec / owner spec — not a path).
                let path_args: &[String] = if matches!(basename, "chmod" | "chown") {
                    args.get(1..).unwrap_or(&[])
                } else {
                    &args
                };
                for p in path_args {
                    if !p.is_empty() {
                        out.push(p.clone());
                    }
                }
            }
        }
        // Don't recurse into `command` children — done.
        return;
    }
    // Recurse on non-command nodes (program, list, pipeline, etc.).
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_mutation_paths(child, source, mutators, out);
        }
    }
}

#[cfg(feature = "semantic-bash")]
fn collect_redirect_targets(node: tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        // `file_redirect` is the tree-sitter-bash node for `> file`,
        // `>> file`, `&> file`, `1> file`, `2> file`, etc. It has the
        // operator as an anonymous child + a `word`/`string`/etc.
        // named child carrying the destination.
        "file_redirect" => {
            // Find the destination — typically the last named child.
            for i in (0..node.named_child_count()).rev() {
                if let Some(child) = node.named_child(i) {
                    if let Ok(text) = child.utf8_text(source) {
                        let trimmed = unquote_simple(text.trim());
                        if !trimmed.is_empty()
                            && !trimmed.starts_with("&")
                            && !trimmed.chars().all(|c| c.is_ascii_digit())
                        {
                            out.push(trimmed);
                        }
                    }
                    break;
                }
            }
        }
        // `herestring_redirect` (<<<) is followed by a value, not a
        // file target — skip. Heredoc (<<EOF) similarly has no path.
        "heredoc_redirect" | "herestring_redirect" => {}
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    collect_redirect_targets(child, source, out);
                }
            }
        }
    }
}

#[cfg(feature = "semantic-bash")]
fn unquote_simple(s: &str) -> String {
    // Tree-sitter `word` nodes come without quotes; `string` nodes
    // include them. Strip a single matched pair of leading/trailing
    // quotes so the path matches what the shell would resolve.
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

#[allow(dead_code)]
pub fn parse_bash_segments(command: &str) -> Vec<String> {
    parse_bash_segments_full(command)
        .map(|(segs, _)| segs)
        .unwrap_or_else(|_| vec![command.to_string()])
}

pub fn parse_bash_segments_full(command: &str) -> Result<(Vec<String>, bool), String> {
    #[cfg(feature = "semantic-bash")]
    {
        use tree_sitter::Parser;

        let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set bash language: {e}"))?;

        let tree = parser
            .parse(command, None)
            .ok_or("Failed to parse bash command")?;

        let root = tree.root_node();
        let source = command.as_bytes();

        let mut segments = Vec::new();
        let mut is_complex = false;

        if has_complex_constructs(root) {
            is_complex = true;
            segments.push(command.to_string());
            return Ok((segments, is_complex));
        }

        if root.has_error() {
            segments.push(command.to_string());
            return Ok((segments, is_complex));
        }

        collect_segments(root, source, &mut segments);
        if segments.is_empty() {
            segments.push(command.to_string());
        }

        Ok((segments, is_complex))
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        Ok((vec![command.to_string()], false))
    }
}

/// dirge-p3vf: coarse, string-level test for shell constructs that must be
/// checked as one opaque command rather than split into per-command allow
/// claims — command/process substitution, ANSI-C quoting, and heredocs.
///
/// Pure and feature-independent. Used by the non-`semantic-bash` enforcement
/// split AND, via [`command_is_complex`], the `/why` explainer — so the two
/// can't drift. They used to disagree on heredocs (`<<`): enforcement flagged
/// them, `/why` didn't, so `/why` explained a heredoc as a head-rule allow
/// when the runtime actually prompts.
// Under `semantic-bash` the enforcement split and `command_is_complex` both
// take the tree-sitter path, so this coarse scan is only reached in the
// non-semantic build (and unit tests).
#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
pub fn coarse_complex_syntax(command: &str) -> bool {
    command.contains("$(")
        || command.contains('`')
        || command.contains("<(")
        || command.contains(">(")
        || command.contains("$'")
        || command.contains("<<")
}

/// Whether `command` is "complex" — the splitter can't safely decompose it
/// into per-command claims, so it's checked as one whole-command `Execute`
/// claim and allow rules must not silently match its head (dirge-g9qj).
///
/// Mirrors exactly what the enforcement splitter decides in THIS build: the
/// tree-sitter constructs under `semantic-bash`, the coarse syntax scan
/// otherwise. The `/why` explainer calls this so its explanation matches what
/// enforcement will actually do (dirge-p3vf).
pub fn command_is_complex(command: &str) -> bool {
    #[cfg(feature = "semantic-bash")]
    {
        // Parse failure ⇒ complex (checked whole), matching the enforcement
        // path which pushes the entire command as one claim on error.
        parse_bash_segments_full(command)
            .map(|(_, complex)| complex)
            .unwrap_or(true)
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        coarse_complex_syntax(command)
    }
}

#[cfg(feature = "semantic-bash")]
fn has_complex_constructs(node: tree_sitter::Node) -> bool {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            match child.kind() {
                "command_substitution"
                | "process_substitution"
                | "subshell"
                | "arithmetic_expansion" => return true,
                _ => {
                    if has_complex_constructs(child) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(feature = "semantic-bash")]
fn collect_segments(node: tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    // C3 (audit fix): compound forms (`{ ... }`, `if`, `while`, `for`,
    // `case`, function bodies) previously pushed the whole construct
    // as one opaque segment that matched no per-command rule — so
    // `{ rm -rf /tmp/foo; }` and `if cond; then rm; fi` bypassed
    // every bash permission rule. Opencode's `shell.ts` recurses via
    // `descendantsOfType("command")`; we mirror that by recursing
    // into compound forms so each contained `command` node lands as
    // its own segment.
    match node.kind() {
        "program" | "list" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    collect_segments(child, source, out);
                }
            }
        }
        "pipeline" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    // Each side of a pipe is checked separately —
                    // preserve the leaf-text behaviour here (a
                    // pipeline element is a single command, possibly
                    // redirected; recursing once into a redirected
                    // pipeline element collects the inner command).
                    if child.kind() == "redirected_statement"
                        || child.kind() == "compound_statement"
                        || child.kind() == "if_statement"
                        || child.kind() == "while_statement"
                        || child.kind() == "for_statement"
                        || child.kind() == "case_statement"
                        || child.kind() == "function_definition"
                        || child.kind() == "c_style_for_statement"
                    {
                        collect_segments(child, source, out);
                    } else {
                        let text = child.utf8_text(source).unwrap_or("").trim().to_string();
                        if !text.is_empty() {
                            out.push(text);
                        }
                    }
                }
            }
        }
        // Compound forms — recurse so each inner `command` lands as
        // its own segment.
        "compound_statement"
        | "if_statement"
        | "while_statement"
        | "for_statement"
        | "case_statement"
        | "function_definition"
        | "c_style_for_statement"
        | "case_item"
        | "elif_clause"
        | "else_clause"
        | "do_group" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    collect_segments(child, source, out);
                }
            }
        }
        // Redirected command — recurse so the wrapped command is
        // checked. C4 (a follow-on fix) will additionally check the
        // redirect target through the path gate; for now the
        // segment text used by command-pattern rules is the inner
        // command without its redirections, matching how opencode
        // separates the two concerns.
        "redirected_statement" => {
            // Find the inner command/pipeline; the redirect operands
            // are leaf nodes (file_redirect, heredoc_redirect, etc.)
            // that don't carry shell-command text.
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    match child.kind() {
                        "command" | "pipeline" | "compound_statement" | "subshell" => {
                            collect_segments(child, source, out);
                        }
                        _ => {} // redirect operands — handled by C4
                    }
                }
            }
        }
        "command" => {
            let text = node.utf8_text(source).unwrap_or("").trim().to_string();
            if !text.is_empty() {
                out.push(text);
            }
        }
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    collect_segments(child, source, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod complexity_tests {
    use super::coarse_complex_syntax;
    #[cfg(not(feature = "semantic-bash"))]
    use super::command_is_complex;

    // dirge-p3vf: the coarse classifier flags every construct the runtime
    // checks as one opaque command, INCLUDING heredocs — which the old `/why`
    // string-match omitted.
    #[test]
    fn coarse_flags_heredoc_and_substitutions() {
        assert!(coarse_complex_syntax("cat <<EOF"));
        assert!(coarse_complex_syntax("echo $(whoami)"));
        assert!(coarse_complex_syntax("echo `id`"));
        assert!(coarse_complex_syntax("diff <(a) <(b)"));
        assert!(coarse_complex_syntax("tee >(cat)"));
        assert!(coarse_complex_syntax("printf $'\\n'"));
        // Plain pipelines / lists are decomposable, not complex.
        assert!(!coarse_complex_syntax("ls -la | grep foo"));
        assert!(!coarse_complex_syntax("echo a && echo b"));
    }

    // In the default (non-semantic-bash) build, enforcement and the `/why`
    // explainer both route through the coarse scan — they can't disagree.
    #[cfg(not(feature = "semantic-bash"))]
    #[test]
    fn command_is_complex_matches_coarse_without_semantic() {
        for cmd in ["cat <<EOF", "echo $(id)", "ls -la", "echo hi && pwd"] {
            assert_eq!(
                command_is_complex(cmd),
                coarse_complex_syntax(cmd),
                "cmd: {cmd}"
            );
        }
        assert!(command_is_complex("cat <<EOF"));
        assert!(!command_is_complex("ls -la"));
    }
}
