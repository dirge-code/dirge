pub(crate) mod apply_patch;
pub(crate) mod background;
pub(crate) mod bash;
pub(crate) mod bg_shell;
pub(crate) mod cache;
#[cfg(feature = "dap")]
pub(crate) mod debug;
pub(crate) mod edit;
pub(crate) mod edit_lines;
#[cfg(feature = "semantic")]
mod edit_minified;
mod find_files;
mod glob;
mod grep;
mod issue;
pub(crate) mod line_hash;
mod list_dir;
#[cfg(feature = "lsp")]
mod lsp;
mod memory;
pub(crate) mod modified;
pub(crate) mod output_relay;
pub(crate) mod plan;
pub(crate) mod question;
mod read;
#[cfg(feature = "semantic")]
mod read_minified;
mod repo_overview;
#[cfg(feature = "semantic")]
pub mod semantic;
mod session_search;
mod skill;
pub(crate) mod snapshots;
mod spec;
pub mod task;
mod task_status;
pub(crate) mod text_io;
pub(crate) mod todo;
pub mod tool_search;
mod webfetch;
mod websearch;
pub(crate) mod write;

#[cfg(feature = "experimental-graph-search")]
mod graph;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use bg_shell::{BashOutputTool, KillShellTool};
pub use cache::ToolCache;
#[cfg(feature = "dap")]
pub use debug::DebugTool;
pub use edit::EditTool;
pub use edit_lines::EditLinesTool;
#[cfg(feature = "semantic")]
pub use edit_minified::EditMinifiedTool;
pub use find_files::FindFilesTool;
pub use glob::GlobTool;
#[cfg(feature = "experimental-graph-search")]
pub use graph::GraphTool;
pub use grep::GrepTool;
pub use issue::IssueTool;
pub use list_dir::ListDirTool;
#[cfg(feature = "lsp")]
pub use lsp::LspTool;
pub use memory::MemoryTool;
pub use plan::{PlanEnterTool, PlanExitTool};
pub use question::QuestionTool;
pub use read::ReadTool;
#[cfg(feature = "semantic")]
pub use read_minified::ReadMinifiedTool;
pub use repo_overview::RepoOverviewTool;
pub use session_search::SessionSearchTool;
pub use skill::SkillTool;
pub use spec::SpecTool;
pub use task::TaskTool;
pub use task_status::TaskStatusTool;
pub use todo::WriteTodoList;
#[allow(unused_imports)]
pub use tool_search::{ALWAYS_ON_TOOLS, TOOL_SEARCH_NAME, ToolMeta, ToolSearchTool};
pub use webfetch::WebFetchTool;
pub use websearch::WebSearchTool;
pub use write::WriteTool;

/// Canonical filesystem boundary for a set of rooted tools.
///
/// A root must exist when it is configured; targets may not, provided their
/// nearest existing ancestor remains inside this root.
#[derive(Clone, Debug)]
pub struct ToolRoot(PathBuf);

impl ToolRoot {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(path.as_ref()).map_err(ToolError::from)?;
        if !root.is_dir() {
            return Err(ToolError::Msg(format!(
                "ToolRoot is not a directory: {}",
                root.display()
            )));
        }
        Ok(Self(root))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Resolve `path` beneath this root, following existing symlinks and
    /// allowing a missing suffix only below an in-root existing ancestor.
    pub fn resolve(&self, path: &str) -> Result<String, ToolError> {
        let input = Path::new(path);
        let joined = if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.0.join(input)
        };
        let normalized = normalize_path(&joined)?;
        let mut ancestor = normalized.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| ToolError::Msg(format!("path escapes ToolRoot: {path}")))?;
        }
        let canonical_ancestor = std::fs::canonicalize(ancestor)?;
        if !canonical_ancestor.starts_with(&self.0) {
            return Err(ToolError::Msg(format!("path escapes ToolRoot: {path}")));
        }
        let suffix = normalized
            .strip_prefix(ancestor)
            .map_err(|_| ToolError::Msg(format!("path escapes ToolRoot: {path}")))?;
        Ok(canonical_ancestor
            .join(suffix)
            .to_string_lossy()
            .into_owned())
    }
}

fn normalize_path(path: &Path) -> Result<PathBuf, ToolError> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if !out.pop() {
                    return Err(ToolError::Msg(format!(
                        "path escapes ToolRoot: {}",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// Resolve a path for an optionally rooted tool. Rootless tools preserve their
/// historical absolute-path contract.
pub fn resolve_tool_path(
    root: Option<&ToolRoot>,
    path: &str,
    subject: &str,
) -> Result<String, ToolError> {
    match root {
        Some(root) => root.resolve(path),
        None => {
            require_absolute_path(path, subject).map_err(ToolError::Msg)?;
            Ok(path.to_string())
        }
    }
}

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::PermCheck;

pub const MAX_GREP_RESULTS: usize = 200;
pub const MAX_FIND_RESULTS: usize = 200;

/// Single source of truth for every built-in tool name dirge ships.
/// Used by:
///   - `agent/builder.rs` MCP collision filter — refuses to register
///     an MCP-exported tool with a colliding name.
///   - `context/prompts.rs` `deny_tools` validation — warns when a
///     prompt's frontmatter names something not in this set.
///
/// Previously these two sites maintained independent lists; review-
/// batch #7 unified them so adding a new tool only requires one edit.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "read_minified",
    "write",
    "edit",
    "edit_lines",
    "edit_minified",
    "bash",
    "grep",
    "find_files",
    "glob",
    "list_dir",
    "write_todo_list",
    "issue",
    "apply_patch",
    "memory",
    "skill",
    "task",
    "task_status",
    "bash_output",
    "kill_shell",
    "tool_search",
    "question",
    "webfetch",
    "websearch",
    "lsp",
    "debug",
    "repo_overview",
    "spec",
    "session_search",
    "search_graph",
    "list_symbols",
    "get_symbol_body",
    "find_definition",
    "find_callers",
    "find_callees",
    // plan_enter / plan_exit are unconditionally added when plan_tx
    // is in scope (they manage the plan mode state via plan_tx). An
    // MCP server exporting either name would shadow them and could
    // disable / hijack plan mode.
    "plan_enter",
    "plan_exit",
    // `mcp_tool` is the umbrella name McpTool calls go through.
    // Including it lets a prompt's `deny_tools: [mcp_tool]` deny
    // every MCP server's tools wholesale; the warn-on-unknown gate
    // in `context/prompts.rs` then accepts that entry. It also makes
    // an agent-profile `allow_tools` list a real cap over MCP (the
    // `Allow→deny` conversion denies every builtin name not allowed,
    // and `mcp_tool` is one such name).
    "mcp_tool",
    // `plugin_tool` is the umbrella for Janet plugin-registered tools
    // (see `JanetLoopTool::execute`, dirge-rfix). Listing it here — like
    // `mcp_tool` — lets `deny_tools: [plugin_tool]` block every plugin
    // tool, and makes `allow_tools` restrict plugin tools too (dirge-74nb)
    // rather than silently leaving them all callable.
    "plugin_tool",
];

/// Whether the built-in named `name` is actually compiled into THIS build.
/// Most built-ins are unconditional; a handful are registered only behind a
/// cargo feature (see the `#[cfg(...)]` gates in
/// `agent/builder/loop_tools.rs`) and are absent when it's off. Names that
/// aren't feature-gated — including anything not a built-in at all — return
/// `true`; callers gate on `BUILTIN_TOOL_NAMES` membership separately (see
/// [`reserves_builtin_name`]).
///
/// Keep this mapping in sync with those registration gates.
// Only reached via `reserves_builtin_name`, whose sole non-test caller is
// the mcp/plugin collision gate; unused in a build with neither feature.
#[cfg_attr(not(any(feature = "mcp", feature = "plugin")), allow(dead_code))]
fn builtin_compiled_in(name: &str) -> bool {
    match name {
        // src/agent/builder/loop_tools.rs: build_graph_tool, cfg experimental-graph-search
        "search_graph" => cfg!(feature = "experimental-graph-search"),
        // semantic_manager.tools(), cfg semantic
        "list_symbols" | "get_symbol_body" | "find_definition" | "find_callers"
        | "find_callees" => cfg!(feature = "semantic"),
        // LspTool, cfg lsp
        "lsp" => cfg!(feature = "lsp"),
        // DebugTool, cfg dap
        "debug" => cfg!(feature = "dap"),
        _ => true,
    }
}

/// Whether `name` is reserved by a built-in tool that is ACTUALLY present in
/// this build. A feature-gated built-in that isn't compiled in does NOT
/// reserve its name — so an MCP server or plugin may export a tool by that
/// name and have it registered instead of silently skipped (issue #702:
/// `search_graph` was reserved even in default builds, where it lives behind
/// `experimental-graph-search` and isn't registered, leaving no such tool
/// available at all).
///
/// This is the collision gate for externally-sourced tools. It stays
/// narrower than [`BUILTIN_TOOL_NAMES`], which remains the full known set
/// for deny/allow-list validation — a config may legitimately name a tool
/// that some build ships even if the current one doesn't.
#[cfg_attr(not(any(feature = "mcp", feature = "plugin")), allow(dead_code))]
pub fn reserves_builtin_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name) && builtin_compiled_in(name)
}

#[cfg(test)]
mod builtin_reservation_tests {
    use super::*;

    /// issue #702: a feature-gated built-in reserves its name only when its
    /// feature is compiled in. Asserted against the build's own cfg so the
    /// invariant holds under any feature set (default or `--no-default-features`).
    #[test]
    fn feature_gated_builtins_reserve_only_when_compiled_in() {
        assert_eq!(
            reserves_builtin_name("search_graph"),
            cfg!(feature = "experimental-graph-search"),
        );
        assert_eq!(reserves_builtin_name("debug"), cfg!(feature = "dap"));
        assert_eq!(reserves_builtin_name("lsp"), cfg!(feature = "lsp"));
        assert_eq!(
            reserves_builtin_name("list_symbols"),
            cfg!(feature = "semantic"),
        );
    }

    /// Unconditional built-ins always reserve; non-built-ins never do.
    #[test]
    fn unconditional_builtins_reserve_and_customs_do_not() {
        assert!(reserves_builtin_name("read"));
        assert!(reserves_builtin_name("bash"));
        assert!(reserves_builtin_name("write"));
        assert!(!reserves_builtin_name("acme_search"));
        assert!(!reserves_builtin_name("totally_custom_tool"));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Msg(String),
}

/// Stable leading marker on every rule/user/non-interactive permission
/// refusal produced by [`enforce`] / [`enforce_request`] / the human-ask
/// path. The failure tracker ([`Outcome::Denied`]) and the critic
/// transcript labeler key off this prefix to tell a *policy* refusal —
/// which the model cannot fix by retrying — apart from a mechanical
/// failure it can. dirge-c7sd: denials carry no typed identity by the
/// time they reach those consumers (they arrive as a result string +
/// `is_error` bool), so the message prefix IS the signal. Keep this and
/// [`AUTO_DENIAL_PREFIX`] in sync with [`is_permission_denial`].
pub const DENIAL_PREFIX: &str = "Permission denied";
/// Leading marker on an `approval_provider` (LLM evaluator) auto-denial.
/// Separate from [`DENIAL_PREFIX`] because the wording differs; both are
/// recognized by [`is_permission_denial`].
pub const AUTO_DENIAL_PREFIX: &str = "Auto-approval denied by approval_provider";

/// True when a tool-result error text is a permission/approval denial: a
/// policy refusal the model cannot resolve by retrying or rephrasing,
/// only the user can (via `/allow` or a prompt). Single source of truth
/// shared by the failure tracker and the critic so neither mistakes a
/// guardrail for a mechanical failure to "try a different approach"
/// around. Keyed on the stable prefixes the `enforce` layer emits.
pub fn is_permission_denial(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with(DENIAL_PREFIX) || t.starts_with(AUTO_DENIAL_PREFIX)
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

pub fn is_skip_dir(name: &str) -> bool {
    matches!(name, "node_modules" | "target")
}

/// Head-truncate `text` to at most `max_bytes` (landing on a UTF-8 char
/// boundary), appending a uniform marker noting how much was dropped.
/// Single source for the per-tool byte caps (dirge-06cp) so the marker
/// is consistent and truncation is never *silent*. Takes ownership and
/// returns the input untouched when it's within the cap (no copy).
/// `what` names the source for the marker (e.g. "bash output").
///
/// NOTE: this is for the in-tool byte ceilings only. The LLM-context cap
/// (head+tail, `compression`), the UI display cap (line-aware), grep's
/// per-line cap, and list_dir's per-item cap are deliberately separate
/// concerns/layers, not folded in here.
pub fn head_cap(text: String, max_bytes: usize, what: &str) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let total = text.len();
    let dropped = total - cut;
    let mut out = text;
    out.truncate(cut);
    out.push_str(&format!(
        "\n…[{what} truncated: dropped {dropped} of {total} bytes; narrow the command (head/grep) to keep context lean]"
    ));
    out
}

/// Extract a required, non-blank string argument for a multiplexer
/// tool's action, with a uniform error message. Replaces the per-action
/// `ok_or_else(|| Msg("X is required for 'Y'"))` checks that memory and
/// skill each hand-rolled with slightly different wording (dirge-8k3k).
///
/// Kept as a call-site helper rather than a schema-driven
/// `validate_and_repair` rule on purpose: a missing field there returns
/// `Err` from the repair layer, which arms model escalation — overkill
/// for a "you forgot a field for this action" error. Same reasoning as
/// [`require_absolute_path`].
pub fn required_nonblank<'a>(
    value: Option<&'a str>,
    field: &str,
    action: &str,
) -> Result<&'a str, ToolError> {
    match value {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(ToolError::Msg(format!(
            "`{field}` is required for action '{action}'"
        ))),
    }
}

/// Enforce that a tool argument is an absolute filesystem path.
///
/// Single source for the check + error message shared by read, write,
/// edit, and apply_patch (dirge-e1r9). These tools all declare
/// `dirge-hints.semantic = "absolute_path"` in their schema and used to
/// each re-implement `Path::is_absolute()` with a slightly different
/// error string. `subject` names the field for the message (e.g.
/// `"read path"`, `"apply_patch rename target"`). Returns the message
/// as a plain `String`; callers wrap it (`.map_err(ToolError::Msg)?`).
pub fn require_absolute_path(path: &str, subject: &str) -> Result<(), String> {
    if std::path::Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{subject} must be an absolute path like '/home/user/project/file.txt', \
             not a relative path or bare filename — got {path:?}"
        ))
    }
}

/// Pre-write syntax gate shared by EVERY content-writing edit tool
/// (write/edit/edit_lines/edit_minified/apply_patch). Validates `content`
/// and, on a purely-unclosed delimiter imbalance, mechanically closes it
/// (dirge-p5fu) — returning the repaired text plus a note to surface on the
/// success result. On an unrepairable imbalance returns `Err(message)` (the
/// formatted reject; file must NOT be written). Returns the content
/// unchanged when the `semantic` feature is off.
///
/// This is the single choke point so a tool can't silently drift out of the
/// repair contract (a per-call-site copy of this was how `edit_minified`
/// got missed). Err is the `String` message; `ToolError` callers wrap with
/// `.map_err(ToolError::Msg)`.
pub(crate) fn syntax_gate<'a>(
    path: &std::path::Path,
    content: &'a str,
    baseline: impl FnOnce() -> Option<String>,
) -> Result<(std::borrow::Cow<'a, str>, Option<GateNote>), String> {
    #[cfg(feature = "semantic")]
    {
        use crate::semantic::syntax_validator::{SyntaxOutcome, check_syntax, validate_or_repair};
        let outcome = validate_or_repair(path, content);
        if matches!(outcome, SyntaxOutcome::Clean) {
            return Ok((std::borrow::Cow::Borrowed(content), None));
        }
        // dirge-ytu1: the gate can only fairly blame an edit for breakage the
        // edit introduced. When the file ALREADY failed the check, blocking
        // strands the agent — every edit, including one meant to fix the file
        // incrementally, is rejected with a location it never touched, and
        // there is no opt-out (`write` goes through the same gate). Files that
        // legitimately carry a lisp extension but never parse — templates with
        // `<% %>` markers, linter fixtures — become permanently uneditable.
        // So: step aside entirely (no reject, and no mechanical repair on top
        // of structure we can't trust) and tell the model why.
        if let Some(before) = baseline()
            && check_syntax(path, &before).is_err()
        {
            return Ok((
                std::borrow::Cow::Borrowed(content),
                Some(GateNote::PreExisting(
                    "this file already failed the syntax check BEFORE your edit, so the pre-write \
                     gate stood down and your text was written verbatim. The pre-existing errors \
                     are still there — fix them deliberately rather than by guessing."
                        .to_string(),
                )),
            ));
        }
        match outcome {
            SyntaxOutcome::Repaired { content, note } => Ok((
                std::borrow::Cow::Owned(content),
                Some(GateNote::Repaired(note)),
            )),
            SyntaxOutcome::Rejected { message } => Err(message),
            SyntaxOutcome::Clean => Ok((std::borrow::Cow::Borrowed(content), None)),
        }
    }
    #[cfg(not(feature = "semantic"))]
    {
        let _ = (path, baseline);
        Ok((std::borrow::Cow::Borrowed(content), None))
    }
}

/// What the syntax gate did, when it did something the model needs to know
/// about. The distinction is load-bearing: only a REPAIR means the bytes on
/// disk differ from the model's text, which is what the LSP-backed rollback
/// (dirge-p1ws) verifies.
pub(crate) enum GateNote {
    /// Delimiters were mechanically closed / trimmed.
    Repaired(String),
    /// The file was already broken before this edit; the gate stood down.
    PreExisting(String),
}

impl GateNote {
    /// True when the written bytes differ from what the tool was given.
    pub(crate) fn is_repair(&self) -> bool {
        matches!(self, GateNote::Repaired(_))
    }

    fn label(&self) -> &'static str {
        match self {
            GateNote::Repaired(_) => "auto-repair",
            GateNote::PreExisting(_) => "syntax",
        }
    }

    fn text(&self) -> &str {
        match self {
            GateNote::Repaired(t) | GateNote::PreExisting(t) => t,
        }
    }
}

/// Append the syntax-gate note (if any) to a tool's success message, in one
/// uniform format across every edit tool.
pub(crate) fn append_repair_note(msg: &mut String, note: Option<GateNote>) {
    if let Some(note) = note {
        msg.push_str(&format!("\n[{}] {}", note.label(), note.text()));
    }
}

/// First line stamped on a tool result the model asked to receive uncompressed.
/// `llmtrim`'s tool-output stage skips any segment starting with it — see
/// `llmtrim::stages::toolout::VERBATIM_PREFIX`, which matches on the literal
/// because the engine is standalone and does not depend on this crate.
pub const VERBATIM_MARKER: &str = "[dirge: verbatim — this output is exempt from compression]";

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// When true, prefix each line with its 3-char content hash
    /// (`  42 a3f: ...`) for hash-anchored editing via `edit_lines`.
    /// Defaults to the plain `  42: ...` numbering.
    pub line_hashes: Option<bool>,
    /// When true, stamp the result with [`VERBATIM_MARKER`] so prompt
    /// compression passes it through untouched. For the case where the
    /// model needs a guarantee that what it sees is what is on disk.
    pub verbatim: Option<bool>,
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub replace_all: Option<bool>,
}

#[derive(Deserialize)]
pub struct EditLinesArgs {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub expected_hashes: Vec<String>,
    pub new_text: String,
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<u64>,
    /// When true, run the command detached: the tool returns immediately
    /// with a shell id and the command's output is delivered later via the
    /// background-completion notification (same channel as background
    /// subagents). Defaults to false (synchronous).
    #[serde(default)]
    pub background: Option<bool>,
}

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_lines: Option<usize>,
    /// Include dotfiles / hidden files in the search. Default
    /// `false` — F2 carryover from find_files/glob/list_dir: grep
    /// also walks the filesystem and should not silently surface
    /// `.env`, `.git/` internals, etc. by default.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct FindFilesArgs {
    pub pattern: String,
    pub path: Option<String>,
    /// Include dotfiles / hidden files (e.g. `.env`, `.gitignore`).
    /// Default `false` — by default the listing skips hidden files
    /// so secrets in `.env` or `.git/` internals don't get pulled
    /// into LLM context inadvertently. Set `true` when the agent
    /// explicitly needs to inspect dotfiles.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    pub path: Option<String>,
    /// Include dotfiles in the listing. See `FindFilesArgs::include_hidden`
    /// for the rationale; default `false` for safety.
    #[serde(default)]
    pub include_hidden: bool,
}

async fn handle_ask_inner(
    ask_tx: &AskSender,
    permission: &PermCheck,
    tool: &str,
    input: &str,
    details: Option<&str>,
    reason: Option<&str>,
) -> Result<(), ToolError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    ask_tx
        .send(AskRequest {
            tool: tool.to_string(),
            input: input.to_string(),
            details: details.map(str::to_string),
            reason: reason.map(str::to_string),
            reply: reply_tx,
        })
        .await
        .map_err(|_| ToolError::Msg("Permission system unavailable".to_string()))?;
    match reply_rx.await {
        Ok(UserDecision::AllowOnce) => Ok(()),
        Ok(UserDecision::AllowAlways(pattern)) => {
            permission
                .lock_ignore_poison()
                .add_session_allowlist(tool.to_string(), &pattern);
            Ok(())
        }
        Ok(UserDecision::Deny { note }) => {
            Err(ToolError::Msg(user_denial_message(note.as_deref())))
        }
        // Reply channel dropped without a decision — treat as a plain deny
        // rather than letting the tool hang or look approved.
        Err(_) => Err(ToolError::Msg(user_denial_message(None))),
    }
}

/// The tool-result error text for a user deny, with the user's redirection
/// note folded in when they typed one (dirge-hzd8).
///
/// A bare "Permission denied" tells the model only that it's blocked; it
/// then guesses at a workaround, often re-attempting a variant of the same
/// call. The note is the user saying what to do INSTEAD, so it goes in the
/// result the model actually reads. [`DENIAL_PREFIX`] stays leading either
/// way — [`is_permission_denial`] keys off it.
pub fn user_denial_message(note: Option<&str>) -> String {
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => format!(
            "{DENIAL_PREFIX} by user. The user did not approve this call and \
             instructs you to do the following instead: {note}"
        ),
        None => format!("{DENIAL_PREFIX} by user"),
    }
}

/// Outcome of the optional LLM auto-approval pass (dirge-0g6i).
enum AutoVerdict {
    /// Evaluator approved — caller proceeds without a human prompt.
    Allow,
    /// Evaluator denied, carrying its reason. dirge-a5ir: this is ADVISORY
    /// — the caller escalates it to the human prompt (the user may approve
    /// and `/allow` it), and only treats it as terminal when there is no
    /// human (non-interactive). The evaluator is not the final authority
    /// on denials, just on the cheap auto-ALLOW path.
    Deny(String),
    /// No evaluator configured, OR the evaluator call errored → the human
    /// decides (fail-open to the prompt, never silently allow).
    Abstain,
}

/// dirge-0g6i: if an `approval_provider` LLM is configured, let it judge
/// an otherwise-`Ask` decision before prompting the human. Shared by both
/// [`enforce`] (single scope) and [`enforce_request`] (multi-claim bash)
/// so the evaluation path isn't duplicated. See [`AutoVerdict`] for how
/// each outcome is handled — note a Deny escalates to the human rather
/// than failing outright (dirge-a5ir).
async fn try_auto_approve(
    perm: &PermCheck,
    tool: &str,
    command: &str,
    resources: Vec<String>,
) -> AutoVerdict {
    use crate::permission::approval::{ApprovalDecision, ApprovalRequest};
    // One lock: pull the evaluator (clone the Arc) + working dir, then
    // drop the lock BEFORE the await so we never hold it across the LLM
    // call. No evaluator → the human decides.
    let (f, working_dir) = {
        let g = perm.lock_ignore_poison();
        match g.approval_fn() {
            Some(f) => (f, g.working_dir().to_string()),
            None => return AutoVerdict::Abstain,
        }
    };
    let req = ApprovalRequest {
        tool: tool.to_string(),
        command: command.to_string(),
        working_dir,
        resources,
    };
    match f(req).await {
        Ok(ApprovalDecision::Allow) => {
            tracing::info!(target: "dirge::permission", tool, command, "auto-approval: ALLOW");
            AutoVerdict::Allow
        }
        Ok(ApprovalDecision::Deny(reason)) => {
            tracing::info!(target: "dirge::permission", tool, command, %reason, "auto-approval: DENY (escalating to human)");
            AutoVerdict::Deny(reason)
        }
        Err(e) => {
            tracing::warn!(target: "dirge::permission", error = %e, "approval_provider call failed; falling back to human prompt");
            AutoVerdict::Abstain
        }
    }
}

/// Shared post-`try_auto_approve` handling for the `Ask` branch of both
/// [`enforce`] and [`enforce_request`] (dirge-a5ir). `Allow` proceeds;
/// `Deny`/`Abstain` both route to the human prompt, differing only in the
/// terminal message when no human is available (non-interactive). Returns
/// `Ok(true)` when auto-approved (caller need not prompt), `Ok(false)`
/// when the human approved, or the denial error.
async fn resolve_auto_verdict(
    verdict: AutoVerdict,
    ask_tx: &Option<AskSender>,
    perm: &PermCheck,
    tool: &str,
    input: &str,
    details: Option<&str>,
) -> Result<bool, ToolError> {
    // Deny and Abstain share one path: prompt the human, terminal only when
    // there's nobody to ask. They differ in the no-human message and in
    // whether there's an evaluator reason to surface in the prompt (r16x).
    let (reason, no_human_msg) = match verdict {
        AutoVerdict::Allow => return Ok(true),
        AutoVerdict::Deny(reason) => {
            let msg = format!("{AUTO_DENIAL_PREFIX}: {reason}");
            (Some(reason), msg)
        }
        AutoVerdict::Abstain => (None, format!("{DENIAL_PREFIX} (non-interactive mode)")),
    };
    let Some(tx) = ask_tx else {
        return Err(ToolError::Msg(no_human_msg));
    };
    handle_ask_inner(tx, perm, tool, input, details, reason.as_deref()).await?;
    Ok(false)
}

/// Scope arg passed to the [`enforce`] chokepoint. Discriminates
/// path-style checks (`Path` / `PathResolve`, route through
/// `PermissionChecker::check_path`, glob with `*` excluding `/`) from
/// raw checks (`Raw`, route through `PermissionChecker::check`, shell-
/// style patterns where `*` matches across `/`).
///
/// `PathResolve` additionally canonicalizes the path (resolving
/// symlinks, normalizing `..`) and returns the resolved path so the
/// calling tool can open EXACTLY the path the user authorized
/// (audit H12 — TOCTOU symlink swap defense).
pub enum Scope<'a> {
    /// Non-path tool input. Examples: a bash command string, an MCP
    /// `server:tool` identifier, a grep pattern, a URL.
    Raw(&'a str),
    /// Filesystem path; check_path-style rule matching.
    Path(&'a str),
    /// Filesystem path with canonical resolution returned in the
    /// `Ok` value of [`enforce`]. Use this from tools that follow
    /// the permission check with a file open (read / write / edit /
    /// apply_patch) — the resolved path pins the file across the
    /// check↔open window.
    PathResolve(&'a str),
}

/// **Single chokepoint for all tool permission decisions in dirge.**
///
/// Ported from maki's `PermissionManager::enforce`
/// (`maki-agent/src/permissions.rs:283-350`): one function, one
/// signature, internal dispatch based on [`Scope`]. The legacy
/// `check_perm` / `check_perm_path` / `check_perm_path_resolve`
/// trio are retained as thin back-compat wrappers that delegate
/// here, so existing call sites continue to compile unchanged.
///
/// Returns the (possibly canonicalized) scope string on success.
/// `Raw` and `Path` scopes echo their input; `PathResolve` returns
/// the canonical path. Callers that don't need the return value
/// can discard with `enforce(...).await?;`.
///
/// Future milestones planning to compose against this chokepoint:
///   - **M2 (dirge-cep)**: replace per-tool `PermissionConfig`
///     fields with a uniform rule schema. `enforce` keeps its
///     signature; only the underlying checker changes.
///   - **M3 (dirge-6ab)**: tree-sitter-parse bash commands inside
///     `enforce` and recurse per-segment so `git diff && rm -rf /`
///     gets BOTH `git` AND `rm` checked. Currently the bash tool
///     does its own segmenting in [`crate::agent::tools::bash`];
///     M3 collapses that into the chokepoint.
///   - **M4 (dirge-ojn)**: flip unmatched-tool default from Allow
///     to Ask. Pure config change inside the underlying checker.
pub async fn enforce(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    scope: Scope<'_>,
) -> Result<String, ToolError> {
    enforce_with_details(permission, ask_tx, tool, scope, None).await
}

/// [`enforce`] plus display-only `details` for the permission prompt.
///
/// dirge-hzd8 (#744): for most tools the match key IS the tool call — a bash
/// command, a path, a URL. For MCP tools it is `mcp_tool:<server>:<tool>`,
/// which tells the user nothing about what the call would DO. `details`
/// carries that (the JSON arguments) to the prompt without touching rule
/// matching or the "allow always" pattern, both of which stay keyed on
/// `scope`.
pub async fn enforce_with_details(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    scope: Scope<'_>,
    details: Option<&str>,
) -> Result<String, ToolError> {
    let raw_scope: &str = match &scope {
        Scope::Raw(s) | Scope::Path(s) | Scope::PathResolve(s) => s,
    };

    let Some(perm) = permission else {
        // No checker installed (e.g. ACP / --no-tools paths). Pass
        // through with the original scope text — matches the legacy
        // `check_perm_path_resolve` fallback. Raw/Path callers
        // discard the return; PathResolve callers see the
        // unchanged input.
        return Ok(raw_scope.to_string());
    };

    // M-engine (Phase 2b): route the decision through the unified
    // authorization engine. The old per-tool F2 write↔edit↔apply_patch
    // aliasing is gone — those tools normalize to `Operation::Edit`,
    // so one rule governs the trio by construction. Path-vs-raw is a
    // property of the resource (built in `authorize_scope`), so there
    // is no Scope-dispatched `check`/`check_path` split here.
    let is_path = matches!(scope, Scope::Path(_) | Scope::PathResolve(_));
    let (effect, reason, resolved) = {
        let mut guard = perm.lock_ignore_poison();
        let decision = guard.authorize_scope(tool, raw_scope, is_path);
        // Only PathResolve callers want the canonicalized path back
        // (to pin the file across the check→open window); Raw/Path
        // callers echo their input, matching the legacy contract.
        let resolved = match scope {
            Scope::PathResolve(_) => decision
                .resolved_paths
                .first()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw_scope.to_string()),
            _ => raw_scope.to_string(),
        };
        (decision.effect, decision.reason(), resolved)
    };

    use crate::permission::engine::types::Effect;
    match effect {
        Effect::Allow => Ok(resolved),
        Effect::Deny => Err(ToolError::Msg(format!("{DENIAL_PREFIX}: {reason}"))),
        Effect::Ask => {
            // dirge-0g6i: optional LLM auto-approval before the human
            // prompt. dirge-a5ir: a Deny escalates to the human, it doesn't
            // short-circuit — handled in `resolve_auto_verdict`.
            let verdict = try_auto_approve(perm, tool, raw_scope, Vec::new()).await;
            resolve_auto_verdict(verdict, ask_tx, perm, tool, raw_scope, details).await?;
            // Approved (auto or by the human) → clear the loop-guard counter
            // so a repeated call the user keeps allowing never trips the
            // doom-loop hard-deny (only repeatedly-denied prompts accumulate).
            perm.lock_ignore_poison()
                .note_allowed_scope(tool, raw_scope, is_path);
            Ok(resolved)
        }
    }
}

/// Authorize a pre-built, possibly multi-claim [`AccessRequest`]
/// atomically: ONE decision, at most ONE prompt. This is the entry
/// point for tools (bash) that decompose a single invocation into
/// several claims (command segments + redirect/mutation targets) — the
/// per-resource effects fold most-restrictive-wins, so the whole
/// command is allowed/denied/prompted as a unit instead of gate-by-gate.
///
/// On `Ask`, the single prompt shows the request's `display_input` (the
/// whole command); "allow always" allowlists that command. In-cwd write
/// targets are builtin-allowed and don't re-prompt; external targets are
/// (correctly) re-scrutinized on the next run.
pub async fn enforce_request(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    req: crate::permission::engine::types::AccessRequest,
) -> Result<(), ToolError> {
    use crate::permission::engine::types::Effect;
    let Some(perm) = permission else {
        return Ok(()); // no checker (ACP / --no-tools) → pass through
    };
    let (effect, reason) = {
        let mut guard = perm.lock_ignore_poison();
        let decision = guard.authorize_request(&req);
        (decision.effect, decision.reason())
    };
    match effect {
        Effect::Allow => Ok(()),
        Effect::Deny => Err(ToolError::Msg(format!("{DENIAL_PREFIX}: {reason}"))),
        Effect::Ask => {
            // dirge-0g6i: optional LLM auto-approval. The evaluator sees a
            // per-claim danger summary (operation + in/out-of-project) so
            // it can judge bash compounds and redirect targets precisely.
            // dirge-a5ir: a Deny escalates to the human (see `enforce`).
            let resources = crate::permission::approval::summarize_claims(&req.claims);
            let verdict = try_auto_approve(perm, &req.tool, &req.display_input, resources).await;
            // No `details`: `display_input` is already the whole command.
            resolve_auto_verdict(verdict, ask_tx, perm, &req.tool, &req.display_input, None)
                .await?;
            // Approved → clear the loop-guard counter (see `enforce`).
            perm.lock_ignore_poison().note_allowed_request(&req);
            Ok(())
        }
    }
}

/// Back-compat wrapper for the legacy non-path check. Delegates to
/// [`enforce`] with [`Scope::Raw`]. New code should call `enforce`
/// directly.
pub async fn check_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Raw(input_key))
        .await
        .map(|_| ())
}

/// [`check_perm`] with display-only `details` for the prompt. See
/// [`enforce_with_details`].
pub async fn check_perm_with_details(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
    details: Option<&str>,
) -> Result<(), ToolError> {
    enforce_with_details(permission, ask_tx, tool, Scope::Raw(input_key), details)
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy path check. Delegates to
/// [`enforce`] with [`Scope::Path`]. New code should call `enforce`
/// directly.
pub async fn check_perm_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Path(path))
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy resolve-and-check entrypoint.
/// Delegates to [`enforce`] with [`Scope::PathResolve`] and returns
/// the canonical path. New code should call `enforce` directly.
///
/// Tools that perform a follow-up file operation (read/edit/write/
/// apply_patch) MUST pass this canonical path to the file API
/// instead of re-using the original `args.path`. Without this, the
/// OS dereferences the symlink a SECOND time at open, and a swap
/// between check-time and open-time lands the operation on a
/// different file than the one the user authorized (audit H12).
pub async fn check_perm_path_resolve(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<String, ToolError> {
    enforce(permission, ask_tx, tool, Scope::PathResolve(path)).await
}

/// Rooted path-tool preamble. Confinement always happens before permission
/// checks, so an allow rule cannot authorize an escape.
pub async fn require_and_resolve_rooted(
    root: Option<&ToolRoot>,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
    subject: &str,
) -> Result<String, ToolError> {
    let path = resolve_tool_path(root, path, subject)?;
    check_perm_path_resolve(permission, ask_tx, tool, &path).await
}

/// The path-tool call preamble, in one place: require `path` be absolute, then
/// run the permission check that resolves + pins it to its canonical form.
/// Centralizes the canonicalize → permission-check ordering (the Audit-H12
/// symlink-swap invariant) so a new tool physically can't get it wrong.
/// `subject` names the path in the absolute-path error (e.g. "the write path").
pub async fn require_and_resolve(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
    subject: &str,
) -> Result<String, ToolError> {
    require_absolute_path(path, subject).map_err(ToolError::Msg)?;
    check_perm_path_resolve(permission, ask_tx, tool, path).await
}

// `is_plan_file` and `canonicalize_or_parent` were removed when the
// prompt-level PLAN.md gate moved into the permission checker via
// `deny_tools` frontmatter. The few historical callers (WriteTool,
// EditTool, ApplyPatchTool) now drop the file-name comparison and
// rely on the prompt's deny-list to refuse the entire tool in plan
// mode.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{
        Action, OpSpec, PermissionConfig, RuleConfig, SecurityMode, checker::PermissionChecker,
    };
    use std::sync::{Arc, Mutex};

    /// dirge-hzd8: a bare "Permission denied" leaves the model guessing, so
    /// it retries a variant of the same call. The note the user types at the
    /// deny prompt is guidance about what to do INSTEAD, and it has to reach
    /// the model in the tool result.
    #[test]
    fn deny_note_reaches_the_model_behind_the_stable_prefix() {
        let msg = user_denial_message(Some("use `git clean -n` and show me the list first"));
        assert!(
            msg.contains("git clean -n"),
            "the user's redirection must survive: {msg}"
        );
        // The failure tracker / critic key off this prefix to tell a policy
        // refusal from a mechanical error — it must stay leading.
        assert!(msg.starts_with(DENIAL_PREFIX), "prefix moved: {msg}");
        assert!(is_permission_denial(&msg));
    }

    /// No note (plain `n`, Esc, Ctrl+C, cascade-deny, headless) keeps the
    /// original wording — nothing invented on the user's behalf.
    #[test]
    fn plain_deny_message_is_unchanged_and_blank_notes_do_not_count() {
        let plain = user_denial_message(None);
        assert_eq!(plain, format!("{DENIAL_PREFIX} by user"));
        for blank in ["", "   ", "\n\t "] {
            assert_eq!(
                user_denial_message(Some(blank)),
                plain,
                "whitespace-only note {blank:?} should not add a dangling clause",
            );
            assert!(is_permission_denial(&user_denial_message(Some(blank))));
        }
    }

    // ---- Pre-edit baseline for the syntax gate (dirge-ytu1) ----
    // `.janet` has no tree-sitter grammar, so these exercise the
    // comment/string-aware scanner and need no `semantic-<lang>` feature.

    #[cfg(feature = "semantic")]
    #[test]
    fn gate_blocks_an_edit_that_breaks_a_clean_file() {
        let path = std::path::Path::new("/tmp/gate.janet");
        let before = "(def a 1)\n(def b 2)\n";
        // Mid-file stray closer: unrepairable, and the edit introduced it.
        let candidate = "(def a 1))\n(def b 2)\n";
        assert!(
            syntax_gate(path, candidate, || Some(before.to_string())).is_err(),
            "breaking a previously-clean file must still be blocked"
        );
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn gate_steps_aside_when_the_file_was_already_broken() {
        let path = std::path::Path::new("/tmp/gate.janet");
        let before = "(def a 1))\n(def b 2)\n"; // already broken before the edit
        let candidate = "(def a 1))\n(def b 3)\n"; // unrelated change, still broken
        let (out, note) = syntax_gate(path, candidate, || Some(before.to_string()))
            .expect("a pre-existing error must not block an unrelated edit");
        assert_eq!(out, candidate, "the model's text is written verbatim");
        let note = note.expect("the model must be told why the gate stood down");
        assert!(!note.is_repair(), "this is a warning, not a repair");
        let mut msg = String::from("Applied edit");
        append_repair_note(&mut msg, Some(note));
        assert!(msg.contains("already"), "{msg}");
        assert!(!msg.contains("[auto-repair]"), "wrong label: {msg}");
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn gate_does_not_mechanically_repair_on_top_of_a_broken_baseline() {
        let path = std::path::Path::new("/tmp/gate.janet");
        let before = "(def a 1))\n"; // broken
        // Repairable on its own (trailing truncation), but the baseline is
        // broken, so the gate must not rewrite the model's bytes.
        let candidate = "(def a 1)\n(defn g []\n  (+ 1 2\n";
        let (out, note) =
            syntax_gate(path, candidate, || Some(before.to_string())).expect("must not block");
        assert_eq!(out, candidate, "no mechanical close on a broken baseline");
        assert!(!note.expect("note").is_repair());
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn gate_without_a_baseline_applies_in_full() {
        // `write`/`apply_patch` creating a NEW file has no baseline.
        let path = std::path::Path::new("/tmp/gate.janet");
        assert!(syntax_gate(path, "(def a 1))\n(def b 2)\n", || None).is_err());
        let (out, note) =
            syntax_gate(path, "(defn g []\n  (+ 1 2\n", || None).expect("truncation repairs");
        assert_eq!(out, "(defn g []\n  (+ 1 2\n))");
        assert!(note.expect("note").is_repair());
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn gate_does_not_consult_the_baseline_for_a_clean_edit() {
        let path = std::path::Path::new("/tmp/gate.janet");
        let mut consulted = false;
        let (out, note) = syntax_gate(path, "(def a 1)\n", || {
            consulted = true;
            None
        })
        .expect("clean");
        assert_eq!(out, "(def a 1)\n");
        assert!(note.is_none());
        assert!(
            !consulted,
            "the clean path must not pay for reading the file"
        );
    }

    #[test]
    fn is_permission_denial_recognizes_every_enforce_denial_form() {
        // Lock the contract: each message the enforce layer can emit on a
        // refusal must be recognized, and ordinary tool errors must not be.
        assert!(is_permission_denial(
            "Permission denied: writes outside project"
        ));
        assert!(is_permission_denial("Permission denied by user"));
        assert!(is_permission_denial(
            "Permission denied (non-interactive mode)"
        ));
        assert!(is_permission_denial(
            "Auto-approval denied by approval_provider: file is outside the project directory"
        ));
        // Leading whitespace (excerpt trimming) still matches.
        assert!(is_permission_denial("  Permission denied: x"));
        // Non-denials.
        assert!(!is_permission_denial("old_string not found in file"));
        assert!(!is_permission_denial("Command timed out after 120s"));
        assert!(!is_permission_denial(
            "error: the user lacks permission denied elsewhere in sentence"
        ));
    }

    // dirge-a5ir: an approval_provider Deny is advisory — it escalates to
    // the human prompt rather than hard-failing the call. These tests pin
    // that the human can override a Deny in interactive mode, and that it
    // stays terminal only when there's no human.

    /// Build a checker whose `Ask`-default applies (no rule matches) and
    /// install an approval_fn that always denies with `reason`.
    fn checker_with_denying_evaluator(reason: &'static str) -> PermCheck {
        use crate::permission::approval::ApprovalDecision;
        // Ask-everything: a single Ask rule over all edits so a write to an
        // out-of-cwd path routes through the auto-approval path.
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Ask)],
            ..Default::default()
        };
        let mut checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        checker.set_approval_fn(Arc::new(move |_req| {
            Box::pin(async move { Ok(ApprovalDecision::Deny(reason.to_string())) })
        }));
        Arc::new(Mutex::new(checker))
    }

    #[tokio::test]
    async fn approval_provider_deny_escalates_to_human_who_can_allow() {
        use crate::permission::ask::{AskRequest, UserDecision};
        let perm = checker_with_denying_evaluator("writes outside project");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AskRequest>(1);

        // Stand in for the UI: the human approves despite the evaluator's deny.
        // The prompt carries the evaluator's reason so the UI can show it
        // (dirge-r16x).
        let human = tokio::spawn(async move {
            let req = rx.recv().await.expect("a prompt must reach the human");
            assert_eq!(
                req.reason.as_deref(),
                Some("writes outside project"),
                "escalated deny prompt must carry the evaluator's reason"
            );
            let _ = req.reply.send(UserDecision::AllowOnce);
        });

        let result = enforce(
            &Some(perm),
            &Some(tx),
            "write",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            result.is_ok(),
            "human override of an evaluator deny should allow: {result:?}"
        );
        human.await.unwrap();
    }

    /// dirge-hzd8, end to end: the note the user types at the prompt comes
    /// back as the tool's error text, so the model reads the redirection
    /// instead of just "denied". Also pins that the display-only `details`
    /// reach the prompt without touching the match key.
    #[tokio::test]
    async fn deny_with_a_note_surfaces_as_the_tool_error() {
        use crate::permission::ask::{AskRequest, UserDecision};
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Ask)],
            ..Default::default()
        };
        let perm: PermCheck = Arc::new(Mutex::new(PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AskRequest>(1);

        let human = tokio::spawn(async move {
            let req = rx.recv().await.expect("a prompt must reach the human");
            assert_eq!(req.details.as_deref(), Some("{\"path\": \"/tmp/x.rs\"}"));
            // The match key is untouched by `details`.
            assert_eq!(req.input, "/tmp/x.rs");
            let _ = req.reply.send(UserDecision::Deny {
                note: Some("edit src/config.rs instead, that file is generated".into()),
            });
        });

        let err = enforce_with_details(
            &Some(perm),
            &Some(tx),
            "write",
            Scope::PathResolve("/tmp/x.rs"),
            Some("{\"path\": \"/tmp/x.rs\"}"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("edit src/config.rs instead"),
            "the user's redirection must reach the model: {err}"
        );
        assert!(is_permission_denial(&err), "still a denial: {err}");
        human.await.unwrap();
    }

    #[tokio::test]
    async fn approval_provider_deny_is_terminal_without_a_human() {
        let perm = checker_with_denying_evaluator("writes outside project");
        // No ask_tx → non-interactive → the deny stands, carrying the reason.
        let result = enforce(&Some(perm), &None, "write", Scope::PathResolve("/tmp/x.rs")).await;
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains(AUTO_DENIAL_PREFIX) && err.contains("writes outside project"),
            "non-interactive deny keeps the evaluator reason: {err}"
        );
        assert!(
            is_permission_denial(&err),
            "still a recognized denial: {err}"
        );
    }

    /// Test helper: build a single op-based rule (tool-agnostic).
    fn rule(op: OpSpec, pattern: &str, effect: Action) -> RuleConfig {
        RuleConfig {
            op,
            pattern: pattern.to_string(),
            effect,
            tool: None,
        }
    }

    // dirge-8k3k: required_nonblank extracts a present, non-blank value
    // or errors with a uniform "`field` is required for action 'x'".
    #[test]
    fn required_nonblank_extracts_or_errors() {
        assert_eq!(
            required_nonblank(Some("hello"), "content", "add").unwrap(),
            "hello"
        );
        for bad in [None, Some(""), Some("   \t")] {
            let msg = required_nonblank(bad, "content", "add")
                .unwrap_err()
                .to_string();
            assert!(msg.contains("content"), "names the field: {msg}");
            assert!(msg.contains("add"), "names the action: {msg}");
        }
    }

    // dirge-06cp: head_cap returns short input untouched and marks any
    // truncation (never silent), landing on a UTF-8 boundary.
    #[test]
    fn head_cap_passes_short_and_marks_truncation() {
        assert_eq!(head_cap("short".to_string(), 100, "x"), "short");

        let capped = head_cap("a".repeat(50), 10, "bash output");
        assert!(capped.starts_with(&"a".repeat(10)), "kept head: {capped}");
        assert!(capped.contains("truncated"), "marked: {capped}");
        assert!(
            capped.contains("dropped 40 of 50 bytes"),
            "counts: {capped}"
        );

        // Multibyte: 'é' is 2 bytes; a cap of 5 must land on a boundary
        // (4 bytes = 2 chars) without panicking or splitting a char.
        let capped = head_cap("é".repeat(10), 5, "x");
        assert!(capped.starts_with("éé"), "boundary-safe head: {capped}");
        assert!(capped.contains("truncated"));
    }

    // dirge-e1r9: the shared absolute-path guard accepts absolute paths
    // and rejects relative / bare ones with a single uniform message.
    #[test]
    fn require_absolute_path_accepts_absolute_rejects_relative() {
        assert!(require_absolute_path("/home/user/x.rs", "read path").is_ok());
        for bad in ["x.rs", "./x.rs", "../x.rs", "src/x.rs", "1"] {
            let err = require_absolute_path(bad, "read path")
                .expect_err("relative path must be rejected");
            assert!(err.contains("absolute path"), "message: {err}");
            assert!(err.contains(bad), "message names the offending path: {err}");
        }
    }

    /// F2 (dirge-jlj): `enforce(write, ...)` MUST also consult the
    /// `edit` rules. A user writing `edit: { "**": "deny" }`
    /// blocks `write` AND `apply_patch` too — matching opencode's
    /// `EDIT_TOOLS` aliasing.
    #[tokio::test]
    async fn enforce_write_aliases_to_edit_deny() {
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Deny)],
            ..Default::default()
        };
        let checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            result.is_err(),
            "edit deny should propagate to write; got {result:?}",
        );

        let result = enforce(
            &Some(perm),
            &None,
            "apply_patch",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            result.is_err(),
            "edit deny should propagate to apply_patch; got {result:?}",
        );
    }

    /// F2: most-restrictive-wins. If `write` is explicitly Allow
    /// but `edit` is Deny, the Deny wins.
    #[tokio::test]
    async fn enforce_write_alias_most_restrictive_wins() {
        // write/edit/apply_patch share Operation::Edit, so both rules
        // live in ONE ordered ruleset (last-match-wins): allow all,
        // then deny /etc/**.
        let config = PermissionConfig {
            rules: vec![
                rule(OpSpec::Edit, "**", Action::Allow),
                rule(OpSpec::Edit, "/etc/**", Action::Deny),
            ],
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // `/etc/passwd`: write allows (`**`), edit denies (`/etc/**`).
        // More restrictive (deny) wins.
        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/etc/passwd"),
        )
        .await;
        assert!(result.is_err());

        // `/tmp/x.rs`: write/edit/apply_patch now share Operation::Edit,
        // so both rules live in ONE ruleset, last-match-wins. The
        // `write: { "**": allow }` rule (added before the edit deny)
        // matches `/tmp/x.rs`; the `/etc/**` deny does not → Allow.
        // This is the F2 dissolution: "allow all writes except /etc".
        let result = enforce(&Some(perm), &None, "write", Scope::PathResolve("/tmp/x.rs")).await;
        assert!(
            result.is_ok(),
            "/tmp/x.rs: `write **: allow` governs (edit `/etc/**` deny doesn't match) → Allow; got {result:?}",
        );
    }

    /// F2 negative: tools NOT in EDIT_TOOLS aren't aliased.
    /// `read` shouldn't be affected by edit rules.
    #[tokio::test]
    async fn enforce_read_does_not_alias_to_edit() {
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Deny)],
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // read has builtin-allow `**: allow` → succeeds
        // regardless of edit's deny.
        let result = enforce(
            &Some(perm),
            &None,
            "read",
            Scope::PathResolve("anywhere.rs"),
        )
        .await;
        assert!(
            result.is_ok(),
            "read isn't aliased to edit; should pass via builtin-allow; got {result:?}",
        );
    }
}
