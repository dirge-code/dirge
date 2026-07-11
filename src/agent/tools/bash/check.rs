//! Command-parsing and permission-checking layer for the bash tool.
//! Split out of `agent/tools/bash.rs` (dirge-4y4l stage 9b): turns a raw
//! command string into permission claims ([`check_bash_segments`]) and
//! records filesystem mutations after a successful run
//! ([`mark_bash_mutations`]).
//!
//! Two parsing paths: with the `semantic-bash` feature, tree-sitter via
//! `crate::semantic::adapters::bash` does the splitting + extraction;
//! without it, the coarse quote-aware splitter here is used.

use crate::agent::tools::{AskSender, PermCheck, ToolError, enforce_request};
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// dirge-sb2n: paths a bash command mutates — output-redirect targets
/// (`> f`, `cat > f <<'EOF'`) plus the positional args of file-mutating
/// commands (`rm`/`mv`/`cp`/`touch`/…). Reuses the same tree-sitter
/// extractors the permission layer runs (`extract_redirect_targets` +
/// `extract_mutation_paths`) so there's no second parser to keep in sync.
#[cfg(feature = "semantic-bash")]
pub(super) fn bash_mutation_targets(command: &str) -> Vec<String> {
    let mut targets = bash::extract_redirect_targets(command);
    targets.extend(bash::extract_mutation_paths(command));
    targets
}

/// dirge-sb2n: record each path a successful bash command touched into
/// the shared modified-files tracker so it shows up in the MODIFIED
/// panel, the same way write/edit/apply_patch do. Relative paths are
/// resolved against the agent's working dir (from the permission
/// checker) so they canonicalize to the same absolute path the other
/// tools record. `/dev/*` and `/proc/*` redirect targets are skipped —
/// they're not real edits.
#[cfg(feature = "semantic-bash")]
pub(super) fn mark_bash_mutations(permission: Option<&PermCheck>, command: &str) {
    let base = permission.map(|p| {
        let g = p.lock_ignore_poison();
        std::path::PathBuf::from(g.working_dir())
    });
    for target in bash_mutation_targets(command) {
        if target.starts_with("/dev/") || target.starts_with("/proc/") {
            continue;
        }
        let p = std::path::Path::new(&target);
        let abs = match &base {
            Some(b) if p.is_relative() => b.join(p),
            _ => p.to_path_buf(),
        };
        crate::agent::tools::modified::mark_modified(&abs);
    }
}

/// dirge-7l5i: lexically resolve `.`/`..`/`.` path components without
/// touching the filesystem (so it works for not-yet-created targets).
#[cfg(feature = "semantic-bash")]
fn normalize_lexical(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// dirge-7l5i: fold the targets of leading `cd`/`pushd` segments onto
/// `base` to get the effective cwd a later relative target is written
/// against. Best-effort, quote-trimming; conservatively applies ALL `cd`s
/// in the compound (so the effective dir is the last one).
#[cfg(feature = "semantic-bash")]
fn fold_cd_dirs(base: &str, segments: &[String]) -> String {
    let mut dir = std::path::PathBuf::from(base);
    for seg in segments {
        let mut it = seg.split_whitespace();
        let head = it.next().unwrap_or("");
        if (head == "cd" || head == "pushd")
            && let Some(target) = it.find(|a| !a.starts_with('-'))
        {
            let t = target.trim_matches(['"', '\'']);
            if t.is_empty() {
                continue;
            }
            let tp = std::path::Path::new(t);
            if tp.is_absolute() {
                dir = tp.to_path_buf();
            } else {
                dir = normalize_lexical(&dir.join(tp));
            }
        }
    }
    dir.to_string_lossy().into_owned()
}

/// dirge-7l5i: resolve a redirect/mutation target to an absolute path
/// against the (cd-adjusted) effective dir; absolute targets pass through.
#[cfg(feature = "semantic-bash")]
fn resolve_target(effective_dir: &str, target: &str) -> String {
    let p = std::path::Path::new(target);
    if p.is_absolute() {
        target.to_string()
    } else {
        normalize_lexical(&std::path::Path::new(effective_dir).join(p))
            .to_string_lossy()
            .into_owned()
    }
}

/// dirge-k0oa: whether a redirect/mutation TARGET is one the shell will
/// EXPAND before it writes — a leading `~` (tilde / `~user` / `~+` home
/// expansion), an embedded `$` (parameter, command, or arithmetic
/// substitution), or a backtick (command substitution).
///
/// Permission classification resolves targets LEXICALLY against the cwd
/// (bash is never run), so `echo x > $HOME/.bashrc` otherwise resolves to
/// the in-cwd literal `<cwd>/$HOME/.bashrc`, classifies `in_cwd`, and
/// auto-allows — while bash expands `$HOME` and writes the real
/// `~/.bashrc` OUTSIDE the project. That is a permission-containment
/// bypass (`echo`/`cp`/… match a default allow, so the target's
/// classification is the only remaining gate). Callers force any flagged
/// target out of the in-cwd fast path so the external-dir gate confirms
/// the write instead of silently allowing it.
///
/// Only a LEADING `~` expands (bash does no tilde expansion on a mid-word
/// `~`, e.g. `file~backup`). `$` and backtick are checked anywhere. Errs
/// toward prompting on a single-quoted literal like `'$x'` (rare) — the
/// safe direction.
pub(super) fn target_expands_outside_cwd(target: &str) -> bool {
    target.starts_with('~') || target.contains('$') || target.contains('`')
}

pub(super) async fn check_bash_segments(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    command: &str,
) -> Result<(), ToolError> {
    // ATOMIC bash authorization (Phase 3): one bash invocation becomes
    // ONE multi-claim AccessRequest — an Execute claim per command
    // segment plus an Edit claim per redirect target / mutation path —
    // authorized as a unit so the whole command is allowed/denied/
    // prompted ONCE, not gate-by-gate. (Replaces the old per-call
    // `enforce` loop that could fire several sequential prompts.)
    //
    // Semantics preserved by the engine, not bespoke code here:
    //   - each compound segment is checked, so `git diff && rm -rf /`
    //     denies on the `rm` segment (Execute deny rule);
    //   - redirect/mutation targets route through Edit (the write rules
    //     + external-dir gate apply, closing the C4 audit gap);
    //   - `/dev/null` targets are auto-allowed by BuiltinAllow on the
    //     Edit claim — but the command itself still needs Execute
    //     permission, so an UNFAMILIAR `cmd > /dev/null` still prompts
    //     (more correct than the old blanket command soft-allow).
    let Some(perm) = permission else {
        return Ok(()); // no checker (ACP / --no-tools) → pass through
    };
    let mode = {
        let g = perm.lock_ignore_poison();
        g.mode()
    };
    use crate::permission::engine::types::{AccessRequest, Claim, Operation, Resource};
    let cmd_claim = |seg: &str| Claim::new(Operation::Execute, Resource::command(seg));
    // dirge-g9qj: a whole-command claim the splitter refused to
    // decompose (substitution / subshell) must be marked complex so the
    // allow deciders won't silently allow it on a matching head.
    let complex_cmd_claim =
        |seg: &str| Claim::new(Operation::Execute, Resource::command_complex(seg));
    let mut claims: Vec<Claim> = Vec::new();

    #[cfg(feature = "semantic-bash")]
    {
        let working_dir = {
            let g = perm.lock_ignore_poison();
            g.working_dir().to_string()
        };
        // /dev/null detection lives solely in `classify_path` (the Path
        // resource's `dev_null` field, consulted by BuiltinAllow) — so
        // we just split into plain segments here. The old
        // `parse_bash_segments_with_dev_null` computed a parallel
        // per-segment flag that was discarded (dirge-v0b6).
        let (segments, complex) = bash::parse_bash_segments_full(command)
            .unwrap_or_else(|_| (vec![command.to_string()], false));
        // dirge-7l5i: a leading `cd`/`pushd` changes the cwd BEFORE a later
        // RELATIVE redirect/mutation target is written. Resolve relative
        // targets against that cd'd directory, then classify the resulting
        // ABSOLUTE path against the project root. Without this,
        // `cd /etc && echo x > passwd` classified `passwd` as
        // `<project>/passwd` (in-tree → auto-allowed) while bash actually
        // wrote `/etc/passwd` — an out-of-tree write with no prompt.
        // Conservative: all cd targets fold to one effective dir, so a
        // write-then-cd ordering may over-prompt (safe direction).
        let effective_dir = fold_cd_dirs(&working_dir, &segments);
        let path_claim = |target: &str| {
            let resolved = resolve_target(&effective_dir, target);
            let mut resource = crate::permission::engine::classify_path(&resolved, &working_dir);
            // dirge-k0oa: a target bash EXPANDS (`~`, `$VAR`, backtick) was
            // resolved lexically here, not by the shell — so its "in-cwd"
            // classification is untrustworthy (`$HOME/.bashrc` looks in-tree
            // but writes the real home dir). Force it off the in-cwd/dev-null
            // fast path so the external-dir gate confirms the write.
            if target_expands_outside_cwd(target)
                && let Resource::Path {
                    in_cwd, dev_null, ..
                } = &mut resource
            {
                *in_cwd = false;
                *dev_null = false;
            }
            Claim::new(Operation::Edit, resource)
        };
        if complex {
            // Subshell / command substitution / etc.: tree-sitter
            // declined to split — check the whole command as one
            // Execute claim so the user confirms the unfamiliar shape.
            // Marked complex so a matching allow rule (`echo **`) can't
            // silently allow it (dirge-g9qj).
            claims.push(complex_cmd_claim(command));
        } else {
            for segment in &segments {
                claims.push(cmd_claim(segment));
            }
        }
        // PERM-6 / C4 / F1: redirect targets AND file-mutating command
        // path args (rm/cp/mv/mkdir/touch/chmod/…) both route through
        // Edit so write deny-lists + the external-dir gate govern them.
        for target in bash::extract_redirect_targets(command) {
            claims.push(path_claim(&target));
        }
        for path in bash::extract_mutation_paths(command) {
            claims.push(path_claim(&path));
        }
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        // Coarse, quote-aware split when tree-sitter isn't compiled in;
        // command-substitution / heredoc / ANSI-C quoting are checked as
        // one whole-command claim. Shared with the `/why` explainer
        // (dirge-p3vf) so enforcement and explanation can't drift.
        let has_substitution = bash::coarse_complex_syntax(command);
        if has_substitution {
            claims.push(complex_cmd_claim(command));
        } else {
            for segment in quote_aware_split(command) {
                claims.push(cmd_claim(segment));
            }
            // dirge-9bqy: route redirect/mutation targets through Edit so
            // the write deny-lists + external-dir gate govern them, same as
            // the semantic-bash path. Relative targets classify against the
            // project root (no `cd`-folding here — that refinement is
            // semantic-only; absolute out-of-tree writes are the gap that
            // matters and are caught). Skipped on `has_substitution` since
            // a whole-command claim already forces confirmation there.
            let working_dir = {
                let g = perm.lock_ignore_poison();
                g.working_dir().to_string()
            };
            let path_claim = |target: &str| {
                let mut resource = crate::permission::engine::classify_path(target, &working_dir);
                // dirge-k0oa: same shell-expansion guard as the semantic path
                // — a `$VAR`/`~` target (which does NOT trip `has_substitution`)
                // must not ride the in-cwd fast path, since bash expands it to
                // a location we didn't resolve.
                if target_expands_outside_cwd(target)
                    && let Resource::Path {
                        in_cwd, dev_null, ..
                    } = &mut resource
                {
                    *in_cwd = false;
                    *dev_null = false;
                }
                Claim::new(Operation::Edit, resource)
            };
            for target in coarse_redirect_targets(command) {
                claims.push(path_claim(&target));
            }
            for path in coarse_mutation_paths(command) {
                claims.push(path_claim(&path));
            }
        }
    }

    if claims.is_empty() {
        claims.push(cmd_claim(command));
    }

    let req = AccessRequest {
        tool: "bash".to_string(),
        claims,
        mode,
        display_input: command.to_string(),
    };
    enforce_request(permission, ask_tx, req).await
}

/// Split a shell command on `;`, `&&`, `||` separators that appear
/// OUTSIDE single quotes, double quotes, or backslash escapes.
/// Used only on the no-`semantic-bash` build path — the
/// tree-sitter path delegates to the real bash grammar in
/// `semantic::adapters::bash` and doesn't need this.
///
/// Edge cases:
/// - `echo "; rm"` → one segment (the `;` is quoted).
/// - `echo 'a&&b'` → one segment.
/// - `echo \; ls` → one segment (the `;` is escaped).
/// - `cmd1; cmd2 && cmd3` → three segments, trimmed.
/// - Empty / whitespace-only segments dropped.
#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
pub(super) fn quote_aware_split(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    while i < bytes.len() {
        let b = bytes[i];

        if prev_backslash {
            prev_backslash = false;
            i += 1;
            continue;
        }

        if b == b'\\' && !in_single {
            // Inside single quotes, backslash is literal; otherwise it
            // escapes the next byte.
            prev_backslash = true;
            i += 1;
            continue;
        }

        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }

        if !in_single && !in_double {
            // Check for `&&` and `||` (2-byte) BEFORE single-byte `;`/`|`/`&`.
            if i + 1 < bytes.len()
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                push_segment(command, start, i, &mut segments);
                i += 2;
                start = i;
                continue;
            }
            if b == b';' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // Pipe `|` (single-byte) — must be checked AFTER `||`
            // above. Without this, a command like `safe_cmd | rm
            // -rf /` was treated as one segment and only `safe_cmd`'s
            // permission rule applied; the destructive RHS rode in
            // unchecked. The semantic-bash tree-sitter path correctly
            // splits pipelines; this fallback didn't.
            if b == b'|' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // B3-6 (audit fix): background `&` (single-byte) — must
            // be checked AFTER `&&` above. Without this,
            // `safe_cmd & rm -rf /` rode through with only the LHS
            // matching a permission rule; the backgrounded LHS plus
            // unchecked RHS would both execute.
            if b == b'&' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    push_segment(command, start, bytes.len(), &mut segments);
    segments
}

#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn push_segment<'a>(command: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    if end <= start {
        return;
    }
    let s = command[start..end].trim();
    if !s.is_empty() {
        out.push(s);
    }
}

/// dirge-9bqy: coarse redirect-target scan for the no-`semantic-bash`
/// build. Without tree-sitter we still must not let `echo x > /etc/passwd`
/// write outside the project ungated. Walks the command outside single/
/// double quotes and, on a `>`/`>>` operator (a leading fd digit or `&`
/// has already been consumed as a normal byte), captures the next
/// whitespace-delimited token as a write target. Quote-aware so a literal
/// `>` inside a string is not treated as a redirect. Exotic forms
/// (process substitution, `{fd}>`) never reach here — the caller routes
/// `$(`/`` ` ``/`<(`/`>(`/`$'`/`<<` to a whole-command claim first.
#[cfg(not(feature = "semantic-bash"))]
pub(super) fn coarse_redirect_targets(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut targets = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\\' => i += 2, // skip the escaped byte
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'>' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'>' {
                    i += 1; // append `>>`
                }
                if i < bytes.len() && bytes[i] == b'|' {
                    i += 1; // clobber `>|`
                }
                while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                    i += 1;
                }
                let start = i;
                while i < bytes.len() {
                    let t = bytes[i];
                    if (t as char).is_whitespace()
                        || matches!(t, b';' | b'|' | b'&' | b'>' | b'<' | b'(' | b')')
                    {
                        break;
                    }
                    i += 1;
                }
                if i > start {
                    let tok = command[start..i].trim_matches(['"', '\'']);
                    if !tok.is_empty() {
                        targets.push(tok.to_string());
                    }
                }
            }
            _ => i += 1,
        }
    }
    targets
}

/// dirge-9bqy: coarse mutation-path scan for the no-`semantic-bash`
/// build. For each split segment whose command head is a known mutator,
/// treat non-flag operands as write targets so the write rules + external-
/// dir gate apply (matching the semantic path's `extract_mutation_paths`).
/// Conservative: mode/owner operands (`chmod 755 …`) classify in-cwd and
/// are harmless; `dd` only contributes its `of=` operand.
#[cfg(not(feature = "semantic-bash"))]
pub(super) fn coarse_mutation_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for segment in quote_aware_split(command) {
        let toks: Vec<&str> = segment.split_whitespace().collect();
        // Skip leading env assignments / exec wrappers so `FOO=1 rm …`
        // and `nohup rm …` still surface their operands (dirge-8zem).
        let start = crate::permission::engine::types::exec_head_index(&toks);
        let Some(&head) = toks.get(start) else {
            continue;
        };
        let base = head.rsplit('/').next().unwrap_or(head);
        // Single source of truth shared with the semantic extractor (dirge-3yak).
        if !crate::permission::engine::types::FILE_MUTATORS.contains(&base) {
            continue;
        }
        for &t in &toks[start + 1..] {
            if t.starts_with('-') {
                continue; // flag
            }
            if base == "dd" {
                if let Some(rest) = t.strip_prefix("of=") {
                    if !rest.is_empty() {
                        out.push(rest.to_string());
                    }
                }
                continue; // dd uses key=value operands only
            }
            out.push(t.to_string());
        }
    }
    out
}
