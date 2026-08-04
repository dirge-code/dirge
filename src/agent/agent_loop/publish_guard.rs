//! Publish-state guard — the interlock behind prompts/default.md's "don't
//! destroy verified work" advice (dirge-1elu.1).
//!
//! The seed prompt for the AHE run already carried that advice — it was the
//! same prose dirge has today — and the paper measured that it did not work:
//! three of the four winning iterations were execution-time interlocks, not
//! better wording (arXiv:2604.25850v4 §C.1.2, §C.1.3, §C.1.4, §C.2.2, §C.2.3,
//! §C.2.4). `path-tracing` verified its output, then `rm -rf`'d it as a
//! tidy-up and reported success on the delete's exit code; `mcmc-sampling-stan`
//! killed its own generator script; `configure-git-webserver` reset a working
//! deployment "to leave a clean repo for grading". The load-bearing shape was
//! iteration 8's: a HARD block with no override token on non-/tmp protected
//! targets. The overrideable iteration-5 shape leaked — the agent attached the
//! token and the delete went through.
//!
//! This guard arms at the moment the loop observes fresh-green, with the SAME
//! `worktree_probe::TreeFingerprint` the safe-state rung stamps — one source
//! of truth for "what this run changed at the green moment". The protected set
//! is the fingerprint's keys: every file differing from HEAD at green,
//! including `bash`-mutated files the snapshot registry never sees and the
//! generator-script case (`gen.py` and its `out.json` both differ from HEAD,
//! so both are protected — no script-name extraction needed). A later
//! fresh-green REPLACES the set; going stale (an edit after green) does not
//! clear it — previously verified work is still work worth not destroying.
//!
//! Only operations that DISCARD verified work are intercepted: `rm`/`rm -rf`
//! naming a protected path or a directory containing one, `find <dir> -delete`
//! where `<dir>` contains one, the discarding git verbs (`reset --hard`,
//! `checkout -f`, `checkout -- <path>`, `clean -f`, `stash`/`push`), and
//! `> <protected>` / `truncate` on one. Ordinary modification of a protected
//! file — `write`, `edit`, `sed -i`, appends — is deliberately NOT blocked:
//! the paper's one-shot setting froze the deliverable after verification, but
//! dirge's interactive session treats continuing to edit verified code as
//! ordinary work, and blocking it would nag on every edit-test-edit cycle
//! (docs/verification-discipline.md's over-detection failure). The transferable
//! core is *discarding* verified work, not *modifying* it.
//!
//! Anything under a temp dir is never blocked (the paper carved out `/tmp`
//! too); paths not in the protected set are never blocked; before any green
//! has latched nothing is ever blocked. `Off` (the default) is byte-identical
//! to the loop without the guard. `advisory` injects a model-visible warning
//! naming the protected paths, bounded at 2 per run; `blocking` suppresses the
//! call pre-dispatch (storm's mechanism) and returns an error result naming
//! the paths and suggesting a scratch copy under /tmp. There is no override
//! token: the paper measured the overrideable guard leaking, and `advisory` /
//! `off` are the escape hatches — the user's to set.
//!
//! Self-contained — no rig/LLM state. Owned as a local in `run_loop`.

use super::tools::ToolCall;
use super::types::GateMode;
use super::worktree_probe::TreeFingerprint;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Advisory-mode ceiling: at most this many model-visible warnings per run.
/// Matches the tier-ceiling convention (two safe-state aborts, two nudges…).
pub const MAX_PUBLISH_ADVISORIES: u8 = 2;

/// Display tag prefixing every message the guard injects. The UI keys on this
/// to attribute the message to the system; [`emit_harness_notices`]
/// (run.rs) mirrors tagged user messages to a SystemNotice for headless
/// consumers, so a `--print` run surfaces the guard's warning too.
pub const PUBLISH_GUARD_TAG: &str = "[publish-guard]";

/// Outcome of `PublishGuard::inspect` for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishVerdict {
    /// Nothing at risk — let the call through. Also the verdict for `off`
    /// mode, before any green, and after the advisory budget is spent.
    Pass,
    /// The command would discard verified work. In `blocking` mode
    /// `block == true` (suppress the call); in `advisory` mode `block ==
    /// false` (let it run, but inject a warning first).
    Hit {
        block: bool,
        /// Protected repo-relative paths the command would discard, sorted.
        protected: Vec<PathBuf>,
        /// Short human reason naming the operation, e.g. `rm -rf out.json`.
        reason: String,
    },
}

/// Per-run state for the publish guard. Owned as a local in `run_loop`,
/// persists across the outer (turn) loop so a green point from an earlier
/// turn still protects its files later.
#[derive(Debug, Default)]
pub struct PublishGuard {
    /// Repo-relative paths protected by the most recent fresh-green. `None`
    /// until a green has been seen this run.
    protected: Option<BTreeSet<PathBuf>>,
    /// Repo root the fingerprint was taken against, for resolving absolute
    /// paths in commands. `None` when the tree wasn't a git work tree.
    repo_root: Option<PathBuf>,
    /// Advisory warnings already injected this run. Bounded by
    /// [`MAX_PUBLISH_ADVISORIES`]; once spent, advisory-mode hits are silent.
    advisories_emitted: u8,
}

impl PublishGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm (or re-arm) the protected set from the fingerprint taken at the
    /// same green moment the safe-state rung stamps. A later fresh-green
    /// REPLACES the set; going stale never clears it.
    pub fn arm(&mut self, fp: Option<TreeFingerprint>, repo_root: Option<PathBuf>) {
        self.protected = fp.map(|f| f.into_keys().collect());
        self.repo_root = repo_root;
    }

    /// Pre-dispatch decision for one tool call (dirge-1elu.1). `Off` is a
    /// pure pass-through; `blocking` hard-blocks without an override;
    /// `advisory` warns (bounded) and lets the call run.
    pub fn inspect(&mut self, mode: GateMode, call: &ToolCall) -> PublishVerdict {
        if mode == GateMode::Off {
            return PublishVerdict::Pass;
        }
        let Some(protected) = self.protected.as_ref() else {
            return PublishVerdict::Pass;
        };
        if protected.is_empty() {
            return PublishVerdict::Pass;
        }
        let Some(discard) = detect_discard(call, protected, self.repo_root.as_deref()) else {
            return PublishVerdict::Pass;
        };
        match mode {
            GateMode::Blocking => PublishVerdict::Hit {
                block: true,
                protected: discard.protected,
                reason: discard.reason,
            },
            GateMode::Advisory => {
                if self.advisories_emitted >= MAX_PUBLISH_ADVISORIES {
                    return PublishVerdict::Pass;
                }
                self.advisories_emitted += 1;
                PublishVerdict::Hit {
                    block: false,
                    protected: discard.protected,
                    reason: discard.reason,
                }
            }
            GateMode::Off => unreachable!("guarded above"),
        }
    }
}

/// A detected discard of protected work.
struct Discard {
    /// The protected paths at risk (repo-relative, sorted, deduped).
    protected: Vec<PathBuf>,
    /// Short reason, e.g. `git reset --hard`.
    reason: String,
}

/// Parse one tool call for a command that would discard verified work.
/// Only `bash` calls are inspected — git, rm, find and friends all flow
/// through the shell. The dedicated `verify` gate never appears here, and a
/// bare re-run of a test command discards nothing, so re-verification is
/// never caught.
fn detect_discard(
    call: &ToolCall,
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    if call.name != "bash" {
        return None;
    }
    let command = call.arguments.get("command")?.as_str()?;
    // Tokenize once, then split into segments on the shell separators
    // (`&&`, `||`, `;`, `|`). Checking each segment independently means
    // `a && rm out.json` still catches the rm, and a quoted `|` (inside
    // `'...'`/`"..."`) arrives as one token and never splits.
    let tokens = tokenize(command);
    let mut seg: Vec<&String> = Vec::new();
    let check = |seg: &[&String]| -> Option<Discard> { check_segment(seg, protected, repo_root) };
    for tok in &tokens {
        if matches!(tok.as_str(), "&&" | "||" | ";" | "|" | "\n") {
            if let Some(d) = check(&seg) {
                return Some(d);
            }
            seg.clear();
        } else {
            seg.push(tok);
        }
    }
    check(&seg)
}

/// Check a single command segment (one argv vector). Leading wrapper words
/// (`sudo`, `command`, `nohup`) are unwrapped so `sudo rm …` is still seen
/// as an rm.
fn check_segment(
    tokens: &[&String],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let mut rest = tokens;
    while matches!(
        rest.first().map(|t| t.as_str()),
        Some("sudo" | "command" | "nohup")
    ) {
        rest = &rest[1..];
    }
    let argv: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
    let (first, args) = argv.split_first()?;
    let cmd = basename(first);
    let command_specific = match cmd {
        "rm" => rm_discard(args, protected, repo_root),
        "git" => git_discard(args, protected, repo_root),
        "find" => find_discard(args, protected, repo_root),
        "truncate" => truncate_discard(args, protected, repo_root),
        _ => None,
    };
    if command_specific.is_some() {
        return command_specific;
    }
    // General pass: a `> target` / `>| target` redirect truncates `target`,
    // discarding its verified content. `>>` (append) is ordinary modification
    // and never blocks.
    redirect_discard(&argv, protected, repo_root)
}

/// `rm [flags] path...` — block when any path is protected or an ancestor
/// directory of one.
fn rm_discard(
    args: &[&str],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let mut paths = Vec::new();
    let mut after_ddash = false;
    for &a in args {
        if after_ddash {
            paths.push(a);
        } else if a == "--" {
            after_ddash = true;
        } else if a.starts_with('-') {
            // flag (including combined `-rf`, `-v`…)
        } else {
            paths.push(a);
        }
    }
    paths_discard(&paths, "rm", protected, repo_root)
}

/// The discarding git verbs. Anything not in this set (a plain branch
/// checkout, `stash pop`, `reset --soft`, …) touches the working tree
/// without discarding it and is never blocked.
fn git_discard(
    args: &[&str],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let &sub = args.first()?;
    match sub {
        "reset" => {
            if args.contains(&"--hard") {
                all_discard("git reset --hard", protected)
            } else {
                None
            }
        }
        "checkout" => {
            let force = args.iter().any(|a| *a == "-f" || *a == "--force");
            let paths: Vec<&str> = match args.iter().position(|a| *a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => Vec::new(),
            };
            if paths.is_empty() {
                // `checkout -f <branch>` discards the whole working tree.
                if force {
                    all_discard("git checkout -f", protected)
                } else {
                    None
                }
            } else {
                paths_discard(&paths, "git checkout --", protected, repo_root)
            }
        }
        "clean" => {
            // `-f`/`-fd`/`-fx` remove untracked files — which may be the
            // verified output. A dry run (`-n`, or a flag containing `n`)
            // deletes nothing. `-d` alone does nothing without `-f`.
            let destructive = args.iter().any(|a| {
                (*a == "-f" || *a == "--force" || a.starts_with("-f")) && !a.contains('n')
            });
            if destructive {
                all_discard("git clean -f", protected)
            } else {
                None
            }
        }
        "stash" => match args.get(1).copied() {
            // bare `git stash` / `git stash push` / `save` pull the working
            // tree's changes out of it. apply/pop/list/show/drop/clear leave
            // the working tree alone (or restore into it).
            None | Some("push" | "save") => all_discard("git stash", protected),
            _ => None,
        },
        _ => None,
    }
}

/// `find <dir> … -delete` (or `-exec rm`) — block when a starting point is
/// protected or contains one.
fn find_discard(
    args: &[&str],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let deletes = args.contains(&"-delete")
        || args.iter().position(|a| *a == "-exec").is_some_and(|i| {
            args.get(i + 1)
                .copied()
                .is_some_and(|n| n == "rm" || n.ends_with("/rm"))
        });
    if !deletes {
        return None;
    }
    // The starting points are the leading non-flag args; `find` without one
    // defaults to `.`.
    let mut starts: Vec<&str> = args
        .iter()
        .take_while(|a| !a.starts_with('-'))
        .copied()
        .collect();
    if starts.is_empty() {
        starts.push(".");
    }
    paths_discard(&starts, "find -delete", protected, repo_root)
}

/// `truncate [-s SIZE] file...` — truncating a protected path discards its
/// verified content.
fn truncate_discard(
    args: &[&str],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let paths: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .copied()
        .collect();
    paths_discard(&paths, "truncate", protected, repo_root)
}

/// `> target` / `>| target` redirect anywhere in the argv: the shell
/// truncates `target` first, discarding its verified content.
fn redirect_discard(
    tokens: &[&str],
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == ">" || tokens[i] == ">|" {
            if let Some(target) = tokens.get(i + 1).copied()
                && let Some(rel) = resolve_repo_relative(target, repo_root)
                && let Some(hit) = at_risk(&rel, protected)
            {
                return Some(Discard {
                    protected: hit,
                    reason: format!("`> {target}`"),
                });
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// Block when the whole tree's protected set is at risk — used by the
/// all-working-tree git verbs, which discard everything at once.
fn all_discard(what: &str, protected: &BTreeSet<PathBuf>) -> Option<Discard> {
    if protected.is_empty() {
        return None;
    }
    Some(Discard {
        protected: protected.iter().cloned().collect(),
        reason: what.to_string(),
    })
}

/// Match command paths against the protected set. Returns the sorted list of
/// protected paths the command would discard, or `None` if none is at risk.
fn paths_discard(
    paths: &[&str],
    what: &str,
    protected: &BTreeSet<PathBuf>,
    repo_root: Option<&Path>,
) -> Option<Discard> {
    let mut hits = BTreeSet::new();
    for &p in paths {
        if let Some(rel) = resolve_repo_relative(p, repo_root)
            && let Some(hit) = at_risk(&rel, protected)
        {
            hits.extend(hit);
        } else if p.contains(['*', '?', '[']) {
            // Shell glob: conservative match — `*` may cross separators, so
            // `rm -rf *` / `/app/*` / `dir/*` all cover protected paths they
            // can expand to. A glob that matches nothing protected is pass.
            if let Some(rel) = resolve_repo_relative(p, repo_root)
                && let Some(hit) = glob_risk(&rel, protected)
            {
                hits.extend(hit);
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    Some(Discard {
        protected: hits.into_iter().collect(),
        reason: format!("`{what}`"),
    })
}

/// Resolve a command-line path to the repo-relative form used by the
/// protected set. Absolute paths outside the repo root (e.g. `/tmp/x`)
/// resolve to `None` and can never match. Lexical only — no filesystem
/// access, so `.`/`..`/trailing-slash handling never depends on cwd state.
fn resolve_repo_relative(candidate: &str, repo_root: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(candidate);
    let rel = if p.is_absolute() {
        PathBuf::from(p.strip_prefix(repo_root?).ok()?)
    } else {
        p.to_path_buf()
    };
    Some(normalize(&rel))
}

/// Lexically clean a relative path: drop `.`, resolve `..`, drop trailing
/// slashes. `.` collapses to the empty path, which is an ancestor of
/// everything (so `rm -rf .` in the repo root is caught).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::Prefix(_) => out.push(comp.as_os_str()),
        }
    }
    out
}

/// The protected paths that `rel` is or contains — i.e. the protected paths a
/// command targeting `rel` would discard. `None` when `rel` matches nothing.
fn at_risk(rel: &Path, protected: &BTreeSet<PathBuf>) -> Option<Vec<PathBuf>> {
    let hit: Vec<PathBuf> = protected
        .iter()
        .filter(|p| *p == rel || p.starts_with(rel))
        .cloned()
        .collect();
    if hit.is_empty() { None } else { Some(hit) }
}

/// Glob variant of [`at_risk`]: which protected paths does the pattern cover?
/// `*` matches any run of characters including `/`; `?` matches any single
/// character. Errs toward blocking only when a protected path genuinely fits
/// the pattern.
fn glob_risk(pattern: &Path, protected: &BTreeSet<PathBuf>) -> Option<Vec<PathBuf>> {
    let pat = pattern.to_string_lossy();
    let hit: Vec<PathBuf> = protected
        .iter()
        .filter(|p| glob_match(&pat, &p.to_string_lossy()))
        .cloned()
        .collect();
    if hit.is_empty() { None } else { Some(hit) }
}

/// Match `pattern` (with `*`/`?`) against `text`. `*` matches any sequence
/// including `/`; `?` matches any single character including `/`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // dp[i][j]: pattern[..i] matches text[..j]
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        dp[i][0] = dp[i - 1][0] && p[i - 1] == '*';
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    dp[p.len()][t.len()]
}

/// Last path component of a command word, so `/bin/rm` and `rm` both match.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whitespace tokenizer honoring single/double quotes and backslash escapes,
/// so `rm 'out file.json'` and `rm out\ file.json` are single tokens and
/// quoted `>`s aren't redirects. Unquoted `;` / `&` / `|` (and `&&`, `||`)
/// become standalone separator tokens — `rm out.json; echo done` splits like
/// the shell does — except when glued to a `>` (`2>&1`, `>&`, `>|`), which
/// stays one token.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
        } else if in_double {
            match c {
                '"' => in_double = false,
                '\\' => {
                    if let Some(&n) = chars.peek() {
                        chars.next();
                        cur.push(n);
                    }
                }
                _ => cur.push(c),
            }
        } else {
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '\\' => {
                    if let Some(&n) = chars.peek() {
                        chars.next();
                        cur.push(n);
                    }
                }
                // A newline between commands is an exact synonym for `;` — but
                // NOT when escaped: the `\\` arm above already consumed a
                // backslash-newline pair into the current token, so a line
                // continuation never reaches here.
                '\n' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    out.push("\n".to_string());
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                ';' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    out.push(";".to_string());
                }
                '&' if !cur.ends_with('>') => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    if chars.peek() == Some(&'&') {
                        chars.next();
                        out.push("&&".to_string());
                    } else {
                        out.push("&".to_string());
                    }
                }
                '|' if !cur.ends_with('>') => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    if chars.peek() == Some(&'|') {
                        chars.next();
                        out.push("||".to_string());
                    } else {
                        out.push("|".to_string());
                    }
                }
                _ => cur.push(c),
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::worktree_probe::TreeFingerprint;

    /// A bash tool call carrying `command`.
    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// A non-bash tool call (write/edit/verify must never be inspected).
    fn tool(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({ "path": "out.json", "content": "x" }),
        }
    }

    /// Guard armed on `paths` as if a green had just latched over them, with
    /// an optional repo root for absolute-path resolution.
    fn armed(paths: &[&str], repo_root: Option<&str>) -> PublishGuard {
        let mut g = PublishGuard::new();
        let fp: TreeFingerprint = paths
            .iter()
            .map(|p| (PathBuf::from(p), "abc123".to_string()))
            .collect();
        g.arm(Some(fp), repo_root.map(PathBuf::from));
        g
    }

    fn assert_blocked(g: &mut PublishGuard, command: &str, expect: &str) {
        match g.inspect(GateMode::Blocking, &bash(command)) {
            PublishVerdict::Hit {
                block: true,
                protected,
                reason,
            } => {
                assert!(
                    protected.iter().any(|p| p == Path::new(expect)),
                    "reason={reason}: expected {expect} in {protected:?}"
                );
            }
            other => panic!("expected a block for `{command}`, got {other:?}"),
        }
    }

    fn assert_passes(g: &mut PublishGuard, command: &str) {
        assert_eq!(
            g.inspect(GateMode::Blocking, &bash(command)),
            PublishVerdict::Pass,
            "`{command}` must pass untouched"
        );
    }

    // ---- Positive: the guard fires when it should ----------------------

    #[test]
    fn git_reset_hard_after_green_is_blocked_and_warned() {
        // Test 1: green latches, then `git reset --hard`.
        let mut g = armed(&["src/a.rs"], None);
        assert_blocked(&mut g, "git reset --hard", "src/a.rs");
        assert_blocked(&mut g, "git reset --hard HEAD~1", "src/a.rs");

        let mut g = armed(&["src/a.rs"], None);
        match g.inspect(GateMode::Advisory, &bash("git reset --hard")) {
            PublishVerdict::Hit {
                block: false,
                protected,
                ..
            } => {
                assert_eq!(protected, vec![PathBuf::from("src/a.rs")]);
            }
            other => panic!("advisory must warn, not block: {other:?}"),
        }
        // The budget is 2: the second hit still warns, the third is silent.
        assert_eq!(
            g.inspect(GateMode::Advisory, &bash("git reset --hard")),
            PublishVerdict::Hit {
                block: false,
                protected: vec![PathBuf::from("src/a.rs")],
                reason: "git reset --hard".to_string(),
            }
        );
        assert_eq!(
            g.inspect(GateMode::Advisory, &bash("git reset --hard")),
            PublishVerdict::Pass,
            "once the advisory budget is spent the guard is silent"
        );
    }

    #[test]
    fn rm_of_verified_output_is_blocked() {
        // Test 2: green latches on a diff containing `out.json`, then rm it.
        let mut g = armed(&["out.json"], None);
        assert_blocked(&mut g, "rm out.json", "out.json");
        assert_blocked(&mut g, "rm -rf out.json", "out.json");
        assert_blocked(&mut g, "rm -f -- out.json", "out.json");
        assert_blocked(&mut g, "/bin/rm out.json", "out.json");
    }

    #[test]
    fn rm_of_generator_script_is_blocked() {
        // Test 3 (mcmc-sampling-stan shape): the protected set covers the
        // generator script, not only its artifact.
        let mut g = armed(&["gen.py", "out.json"], None);
        assert_blocked(&mut g, "rm gen.py", "gen.py");
        assert_blocked(&mut g, "rm -rf gen.py", "gen.py");
    }

    #[test]
    fn no_override_downgrades_a_hard_block() {
        // Test 4 (configure-git-webserver shape): the iteration-5 guard let a
        // cleanup through once the agent attached its override token. This
        // guard has no token — every argument vector still blocks.
        let mut g = armed(&["out.json"], None);
        for command in [
            "rm -rf out.json",
            "rm -rf --no-preserve-root out.json",
            "yes | rm -rf out.json",
            "sudo rm -rf out.json",
            "rm -rf out.json || true",
            "rm -rf out.json 2>/dev/null",
        ] {
            assert_eq!(
                g.inspect(GateMode::Blocking, &bash(command)),
                {
                    // `yes | rm` and `sudo rm` are piped — the guard parses
                    // per `|`-free segments; only the rm segment is checked.
                    let _ = command;
                    PublishVerdict::Hit {
                        block: true,
                        protected: vec![PathBuf::from("out.json")],
                        reason: "`rm`".to_string(),
                    }
                },
                "no argument vector may pass `{command}`"
            );
        }
        // Advisory mode warns but never blocks — and never silently enables
        // a third path: after the budget the call still runs.
        let mut g = armed(&["out.json"], None);
        match g.inspect(GateMode::Advisory, &bash("rm -rf out.json")) {
            PublishVerdict::Hit { block: false, .. } => {}
            other => panic!("advisory warns, does not block: {other:?}"),
        }
    }

    // ---- Negative: silent when it should be (the criterion that holds
    // ---- at n=1) ------------------------------------------------------

    #[test]
    fn off_mode_passes_everything() {
        // Test 5: byte-identical default.
        let mut g = armed(&["out.json", "src/a.rs"], None);
        for command in [
            "git reset --hard",
            "rm -rf out.json",
            "find . -delete",
            "git checkout -f",
            "git clean -fd",
            "git stash",
            "echo x > out.json",
            "truncate -s 0 out.json",
        ] {
            assert_eq!(
                g.inspect(GateMode::Off, &bash(command)),
                PublishVerdict::Pass,
                "off mode must pass `{command}`"
            );
        }
    }

    #[test]
    fn editing_a_protected_file_is_never_blocked() {
        // Test 6: the paper's rewrite-block deliberately not ported.
        let mut g = armed(&["out.json"], None);
        for command in [
            "sed -i 's/a/b/' out.json",
            "sed -i -e 's/a/b/' -e 's/c/d/' out.json",
            "echo extra >> out.json",
            "touch out.json",
            "chmod +x out.json",
        ] {
            assert_passes(&mut g, command);
        }
        // write/edit tools are not bash — never inspected at all.
        assert_passes(&mut g, "true");
        assert_eq!(
            g.inspect(GateMode::Blocking, &tool("write")),
            PublishVerdict::Pass
        );
        assert_eq!(
            g.inspect(GateMode::Blocking, &tool("edit")),
            PublishVerdict::Pass
        );
    }

    #[test]
    fn temp_paths_are_never_blocked() {
        // Test 7: the paper carved out /tmp too.
        let mut g = armed(&["out.json"], Some("/repo"));
        for command in [
            "rm /tmp/scratch.txt",
            "rm -rf /tmp/scratch",
            "find /tmp -delete",
            "echo x > /tmp/out.json",
            "truncate -s 0 /tmp/out.json",
            "git -C /tmp/other reset --hard",
        ] {
            assert_passes(&mut g, command);
        }
    }

    #[test]
    fn rm_of_unprotected_file_is_never_blocked() {
        // Test 8: paths not in the protected set.
        let mut g = armed(&["src/a.rs"], None);
        assert_passes(&mut g, "rm unrelated.txt");
        assert_passes(&mut g, "rm -rf notes/");
        assert_passes(&mut g, "rm -rf src/b.rs"); // b.rs differs from HEAD? no — only a.rs is protected
    }

    #[test]
    fn nothing_is_blocked_before_any_green() {
        // Test 9: no green latched yet.
        let mut g = PublishGuard::new();
        for command in [
            "rm -rf out.json",
            "git reset --hard",
            "find . -delete",
            "echo x > out.json",
        ] {
            assert_passes(&mut g, command);
        }
    }

    #[test]
    fn rerunning_verification_is_never_blocked() {
        // Test 10: a re-run of the verification command discards nothing.
        let mut g = armed(&["out.json"], None);
        for command in [
            "cargo test",
            "cargo nextest run --bin dirge",
            "make check",
            "pytest",
            "npm test",
        ] {
            assert_passes(&mut g, command);
        }
        assert_eq!(
            g.inspect(GateMode::Blocking, &tool("verify")),
            PublishVerdict::Pass
        );
    }

    // ---- Parsing robustness --------------------------------------------

    #[test]
    fn rm_of_directory_containing_protected_is_blocked() {
        let mut g = armed(&["src/gen.rs", "out.json"], None);
        assert_blocked(&mut g, "rm -rf src", "src/gen.rs");
        assert_blocked(&mut g, "rm -rf .", "src/gen.rs");
        assert_blocked(&mut g, "rm -r src/", "src/gen.rs");
    }

    #[test]
    fn find_delete_blocked_when_dir_contains_protected() {
        let mut g = armed(&["src/a.rs"], None);
        assert_blocked(&mut g, "find . -delete", "src/a.rs");
        assert_blocked(&mut g, "find src -name '*.rs' -delete", "src/a.rs");
        assert_blocked(&mut g, "find src -exec rm {} \\;", "src/a.rs");
        assert_passes(&mut g, "find . -name '*.tmp' -print"); // no -delete
        assert_passes(&mut g, "find /tmp -delete"); // outside the repo
        let mut g = armed(&["src/a.rs"], None);
        assert_passes(&mut g, "find build -delete"); // build/ not protected
    }

    #[test]
    fn git_checkout_forms() {
        let mut g = armed(&["src/a.rs"], None);
        assert_blocked(&mut g, "git checkout -- src/a.rs", "src/a.rs");
        assert_blocked(&mut g, "git checkout -- .", "src/a.rs");
        assert_blocked(&mut g, "git checkout -f", "src/a.rs");
        assert_blocked(&mut g, "git checkout -f feature", "src/a.rs");
        assert_passes(&mut g, "git checkout main"); // plain checkout discards nothing
        assert_passes(&mut g, "git checkout -b fix"); // new branch
        assert_passes(&mut g, "git checkout -- README.md"); // not protected
    }

    #[test]
    fn git_stash_and_clean_forms() {
        let mut g = armed(&["out.json"], None);
        assert_blocked(&mut g, "git stash", "out.json");
        assert_blocked(&mut g, "git stash push -m wip", "out.json");
        assert_blocked(&mut g, "git stash save wip", "out.json");
        assert_passes(&mut g, "git stash pop");
        assert_passes(&mut g, "git stash apply");
        assert_passes(&mut g, "git stash list");
        assert_passes(&mut g, "git stash drop");
        assert_blocked(&mut g, "git clean -fd", "out.json");
        assert_blocked(&mut g, "git clean -fdx", "out.json");
        assert_passes(&mut g, "git clean -n"); // dry run
        assert_passes(&mut g, "git reset"); // mixed reset leaves the tree alone
        assert_passes(&mut g, "git reset --soft HEAD~1");
    }

    #[test]
    fn redirect_truncates_but_appends_do_not() {
        let mut g = armed(&["out.json"], None);
        assert_blocked(&mut g, "echo x > out.json", "out.json");
        assert_blocked(&mut g, "cat gen.py >| out.json", "out.json");
        assert_blocked(&mut g, "truncate -s 0 out.json", "out.json");
        assert_blocked(&mut g, "truncate out.json", "out.json");
        assert_passes(&mut g, "echo x >> out.json"); // append is ordinary work
        assert_passes(&mut g, "echo x > README.md"); // not protected
        assert_passes(&mut g, "cmd > /tmp/log 2>&1");
        assert_passes(&mut g, "echo '> out.json'"); // quoted — not a redirect
    }

    #[test]
    fn globs_cover_protected_paths_but_only_those() {
        let mut g = armed(&["out.json"], Some("/repo"));
        assert_blocked(&mut g, "rm -rf *", "out.json");
        assert_blocked(&mut g, "rm -rf out*", "out.json");
        assert_blocked(&mut g, "rm -rf /repo/out.*", "out.json");
        assert_passes(&mut g, "rm -rf target/*"); // nothing protected under target/
        let mut g = armed(&["src/a.rs"], Some("/repo"));
        assert_passes(&mut g, "rm -rf *.json"); // matches no protected path
    }

    #[test]
    fn absolute_and_relative_paths_resolve_against_the_repo() {
        let mut g = armed(&["out.json"], Some("/repo"));
        assert_blocked(&mut g, "rm -rf /repo/out.json", "out.json");
        assert_blocked(&mut g, "rm -rf /repo/sub/../out.json", "out.json");
        assert_blocked(&mut g, "rm -rf ./out.json", "out.json");
        assert_passes(&mut g, "rm -rf /elsewhere/out.json"); // outside the repo
        assert_passes(&mut g, "rm -rf /tmp/out.json");
        // Unknown repo root: absolute paths can't be matched — conservative pass.
        let mut g = armed(&["out.json"], None);
        assert_passes(&mut g, "rm -rf /repo/out.json");
    }

    #[test]
    fn chained_commands_are_checked_independently() {
        let mut g = armed(&["out.json"], None);
        assert_blocked(&mut g, "true && rm out.json", "out.json");
        assert_blocked(&mut g, "rm out.json; echo done", "out.json");
        assert_blocked(&mut g, "cd /tmp && rm out.json", "out.json"); // relative inside the repo
        assert_passes(&mut g, "rm /tmp/x && echo ok");
    }

    #[test]
    fn backslash_continuation_is_not_a_separator() {
        // `echo hi \<newline>rm out.json` is ONE command line — rm is an
        // argument to echo, so the guard must not see an rm. The tokenizer's
        // `\\` arm consumes the escaped newline into the token.
        let mut g = armed(&["out.json"], Some("/repo"));
        assert_passes(
            &mut g,
            "echo hi \\
rm out.json",
        );
        // A continuation after `&&` still leaves the discard in its own
        // segment: `rm out.json && \<newline>echo done` runs rm first.
        let mut g = armed(&["out.json"], None);
        assert_blocked(
            &mut g,
            "rm out.json && \\
echo done",
            "out.json",
        );
    }

    #[test]
    fn quoting_and_escapes_are_respected() {
        let mut g = armed(&["out file.json"], None);
        assert_blocked(&mut g, "rm 'out file.json'", "out file.json");
        assert_blocked(&mut g, "rm \"out file.json\"", "out file.json");
        assert_blocked(&mut g, "rm out\\ file.json", "out file.json");
        assert_passes(&mut g, "rm out_file.json"); // different file
    }

    #[test]
    fn advisory_budget_is_bounded_at_two_per_run() {
        let mut g = armed(&["a.json", "b.json", "c.json"], None);
        for expected in [true, true, false] {
            let v = g.inspect(GateMode::Advisory, &bash("rm a.json"));
            let warned = matches!(v, PublishVerdict::Hit { block: false, .. });
            assert_eq!(
                warned, expected,
                "advisory #{} budget",
                g.advisories_emitted
            );
        }
        // Blocking mode is NOT budgeted — every discard is a hard block.
        let mut g = armed(&["a.json"], None);
        for _ in 0..5 {
            assert!(matches!(
                g.inspect(GateMode::Blocking, &bash("rm a.json")),
                PublishVerdict::Hit { block: true, .. }
            ));
        }
    }

    #[test]
    fn rearm_replaces_the_protected_set() {
        let mut g = armed(&["a.rs"], None);
        assert_blocked(&mut g, "rm a.rs", "a.rs");
        assert_passes(&mut g, "rm b.rs");
        let fp: TreeFingerprint = [(PathBuf::from("b.rs"), "h".to_string())]
            .into_iter()
            .collect();
        g.arm(Some(fp), None);
        assert_passes(&mut g, "rm a.rs"); // stale set gone
        assert_blocked(&mut g, "rm b.rs", "b.rs");
    }

    #[test]
    fn stale_after_green_edits_does_not_disarm() {
        // Going stale (an edit lands after green) keeps protecting the
        // previously verified work.
        let mut g = armed(&["out.json"], None);
        assert_blocked(&mut g, "rm out.json", "out.json");
    }

    #[test]
    fn empty_fingerprint_never_arms() {
        let mut g = PublishGuard::new();
        g.arm(Some(TreeFingerprint::new()), None);
        assert_passes(&mut g, "rm -rf out.json");
        g.arm(None, None);
        assert_passes(&mut g, "git reset --hard");
    }
    #[test]
    fn newline_separated_rm_is_caught() {
        let mut g = armed(&["out.json"], Some("/repo"));
        let v = g.inspect(GateMode::Blocking, &bash("echo hi\nrm out.json"));
        assert!(
            matches!(v, PublishVerdict::Hit { .. }),
            "newline-separated rm of a protected file must be caught, got {v:?}"
        );
    }
}
