use clap::{Parser, ValueEnum};
use compact_str::CompactString;

use crate::config;

/// dirge-rmk: output format selector for `--print` mode. Ported from
/// maki's `OutputFormat` enum (`maki/src/print.rs:44-49`) which itself
/// matches Claude Code's `--output-format` so tools/scripts written
/// against Claude Code work against dirge unchanged.
///
/// - `Text` (default): the raw assistant response only, no metadata.
/// - `Json`: a single Claude-Code-shaped `PrintResult` object on
///   stdout with `result`, `duration_ms`, `num_turns`, `usage`, etc.
/// - `StreamJson`: NDJSON — one JSON object per line. Emits
///   `system/init`, `assistant`, and a final `result` event so
///   downstream tools can stream-parse turn-by-turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Default)]
#[clap(rename_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

/// Auto-response policy for `harness/confirm` and `harness/select`
/// dialogs in headless modes (`--print`, `--loop`, ACP). Default is
/// `None` (preserves the old behavior: the dialog blocks waiting for
/// a UI that isn't there). When set, a background task drains the
/// plugin worker's dialog channel and replies synthetically so
/// plugin-driven prompts don't hang in CI.
///
/// - `Yes`: `confirm` returns `true`; `select` returns the FIRST option.
/// - `No`:  `confirm` returns `false`; `select` returns `nil`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum AutoConfirmMode {
    Yes,
    No,
}

#[derive(Parser)]
#[command(name = "dirge", version, about = "Minimal coding agent")]
pub struct Cli {
    #[arg(short = 'p', long = "print", help = "Print response and exit")]
    pub print: bool,

    /// dirge-rmk: output format for `--print` mode (text | json |
    /// stream-json). Mirrors Claude Code's flag exactly. Ignored
    /// outside `--print`.
    #[arg(
        long = "output-format",
        value_enum,
        default_value_t = OutputFormat::Text,
        requires = "print",
        help = "Output format for --print mode (text | json | stream-json)"
    )]
    pub output_format: OutputFormat,

    #[arg(short = 'c', long = "continue", help = "Continue most recent session")]
    pub continue_session: bool,

    #[arg(short = 'r', long = "resume", help = "Browse and select a session")]
    pub resume: bool,

    #[arg(
        long = "session",
        help = "Resume a session by id/prefix, or create one with this exact id if none exists (stable id for scripts / the shell plugin)"
    )]
    pub session: Option<String>,

    #[arg(
        long = "goal",
        help = "Natural-language stop condition for autonomous runs (e.g. 'all tests pass and changes committed'). At each finalization an independent judge decides whether it's met; if not, the run continues (bounded). Requires a configured critic_provider as the judge."
    )]
    pub goal: Option<String>,

    #[arg(long = "no-session", help = "Ephemeral mode, do not save")]
    pub no_session: bool,

    #[arg(long = "provider", env = "DIRGE_PROVIDER", help = "API provider")]
    pub provider: Option<String>,

    #[arg(long = "model", env = "DIRGE_MODEL", help = "Model name")]
    pub model: Option<String>,

    #[arg(
        long = "api-key",
        help = "API key for the provider (WARNING: visible to other users via ps/htop; prefer env vars or --api-key-file)"
    )]
    pub api_key: Option<String>,

    /// Read the API key from a file at startup. Preferred over
    /// `--api-key` because the value never reaches argv / proc
    /// listings. Audit C2.
    #[arg(
        long = "api-key-file",
        value_name = "PATH",
        help = "Read API key from a file (preferred over --api-key; file contents must be the raw key, with trailing whitespace stripped)"
    )]
    pub api_key_file: Option<std::path::PathBuf>,

    /// Read the API key from stdin at startup. Useful for piping
    /// from a secrets manager (`pass | dirge --api-key-stdin …`).
    /// Mutually exclusive with `--api-key-file`.
    #[arg(
        long = "api-key-stdin",
        help = "Read API key from stdin at startup (single line; mutually exclusive with --api-key-file)"
    )]
    pub api_key_stdin: bool,

    /// Populated after startup resolves `--api-key-file` / `--api-key-stdin`.
    /// Skipped by Clap so rebuild paths can reuse the secret without exposing a
    /// second CLI option.
    #[arg(skip)]
    pub resolved_api_key: Option<String>,

    #[arg(long = "max-tokens", help = "Maximum tokens in response")]
    pub max_tokens: Option<u64>,

    #[arg(long = "max-agent-turns", help = "Maximum agent turns")]
    pub max_agent_turns: Option<usize>,

    #[arg(long = "temperature", help = "Model temperature (0.0 to 2.0)")]
    pub temperature: Option<f64>,

    #[arg(long = "no-tools", help = "Disable all tools")]
    pub no_tools: bool,

    #[cfg(feature = "lsp")]
    #[arg(
        long = "no-lsp",
        help = "Disable LSP integration (no diagnostics on edit/write, no `lsp` agent tool)"
    )]
    pub no_lsp: bool,

    #[arg(long = "no-color", help = "Disable colored TUI output")]
    pub no_color: bool,

    #[arg(
        short = 'v',
        long = "verbose",
        help = "Enable verbose logging (debug for dirge, warn for plugin hooks; equivalent to RUST_LOG=dirge=debug,dirge::plugin=warn). RUST_LOG env takes precedence if set."
    )]
    pub verbose: bool,

    #[arg(
        long = "restrictive",
        short = 'R',
        help = "Default all tools to ask for approval"
    )]
    pub restrictive: bool,

    #[arg(
        long = "accept-all",
        help = "Auto-accept all operations within the working directory"
    )]
    pub accept_all: bool,

    #[arg(
        long = "yolo",
        help = "Auto-accept ALL operations without any restriction"
    )]
    pub yolo: bool,

    #[arg(
        long = "sandbox",
        num_args = 0..=1,
        default_missing_value = "none",
        require_equals = false,
        help = "Run bash in an isolated sandbox: 'bwrap' (bubblewrap), 'microvm' (hardware VM via libkrun), or 'none' (default, no sandbox)"
    )]
    pub sandbox: Option<String>,

    #[arg(
        long = "microvm-image",
        value_name = "IMAGE",
        help = "OCI image or local reference for the microVM sandbox (e.g. 'docker.io/library/alpine:3.21', 'local://my-image:tag')"
    )]
    pub microvm_image: Option<String>,

    #[arg(
        long = "no-context-files",
        short = 'n',
        help = "Disable AGENTS.md loading"
    )]
    pub no_context_files: bool,

    #[cfg(feature = "loop")]
    #[arg(
        long = "loop",
        help = "Run in headless loop mode (requires --loop-prompt or message)"
    )]
    pub loop_mode: bool,

    #[cfg(feature = "acp")]
    #[arg(
        long = "acp",
        help = "Enable ACP (Agent Communication Protocol) support"
    )]
    pub acp_enabled: bool,

    // Note: --acp-host / --acp-port are intentionally NOT exposed.
    // The current ACP implementation only supports stdio transport
    // (see `src/extras/acp/mod.rs`). The historical config keys still
    // deserialize for backward compatibility but are ignored. If TCP
    // ACP support is added in the future, restore these flags then.
    #[cfg(feature = "loop")]
    #[arg(long = "loop-prompt", help = "Prompt for each loop iteration")]
    pub loop_prompt: Option<String>,

    #[cfg(feature = "loop")]
    #[arg(long = "loop-plan", help = "Plan file path [default: LOOP_PLAN.md]")]
    pub loop_plan: Option<std::path::PathBuf>,

    #[cfg(feature = "loop")]
    #[arg(long = "loop-max", help = "Maximum number of iterations")]
    pub loop_max: Option<u32>,

    #[cfg(feature = "loop")]
    #[arg(
        long = "loop-run",
        help = "Validation command to run after each iteration"
    )]
    pub loop_run: Option<String>,

    #[arg(
        long = "auto-confirm",
        value_enum,
        help = "Auto-respond to plugin harness/confirm and harness/select dialogs in headless modes. Without this flag, dialogs hang waiting for an interactive UI."
    )]
    pub auto_confirm: Option<AutoConfirmMode>,

    /// EXT-6: lock the session to a specific prompt at launch.
    /// Equivalent to `/prompt <name>` but applied before the first
    /// turn. Takes precedence over the config `default_prompt`.
    /// Primarily useful in ACP mode (`--server`) where no
    /// interactive `/prompt` slash command is available.
    #[arg(
        long = "prompt",
        value_name = "NAME",
        help = "Lock the session to a specific prompt at launch (e.g. --prompt plan)"
    )]
    pub prompt: Option<String>,

    #[arg(help = "Prompt message(s)")]
    pub message: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Manage provider authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Check and set up sandbox dependencies
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    /// Run dirge as an MCP server so another agent can delegate
    /// implementation tasks to it (and review them). Speaks MCP over
    /// stdio; keeps a persistent per-project session. Requires the
    /// `mcp-server` build feature.
    #[cfg(feature = "mcp-server")]
    Mcp {
        /// Model dirge uses for delegated work (overrides config for this
        /// server). Defaults to the configured/default model.
        #[arg(long = "model")]
        model: Option<String>,
        /// Sandbox bash during delegations: 'bwrap', 'microvm', or 'none'.
        /// Defaults to no sandbox (tools are still cwd-scoped accept-all).
        #[arg(long = "sandbox")]
        sandbox: Option<String>,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum AuthAction {
    /// Log in to OpenAI using device-code auth
    #[command(
        name = "openai",
        visible_alias = "chatgpt",
        long_about = "Log in to OpenAI using device-code auth.\n\nBefore running this command, enable device-code auth in ChatGPT Codex security settings."
    )]
    Openai,
    /// Start Anthropic Claude Code OAuth login and persist credentials
    Anthropic,
}

#[derive(clap::Subcommand, Debug)]
pub enum SandboxAction {
    /// Print a report of sandbox dependencies
    Check,
    /// Set up microVM sandbox: check deps, update config.json, pre-pull OCI image
    Setup {
        /// OCI image to use (default: docker.io/library/debian:bookworm-slim)
        #[arg(long = "image")]
        image: Option<String>,
    },
}

impl Cli {
    pub fn resolve_model(&self, cfg: &config::Config) -> CompactString {
        if let Some(m) = self.model.as_deref() {
            return CompactString::new(m);
        }
        if let Some((_, entry)) = cfg.resolve_role(config::ConfigRole::Default)
            && let Some(m) = entry.model
        {
            return CompactString::new(m);
        }
        CompactString::new("deepseek/deepseek-v4-flash")
    }

    pub fn resolve_provider(&self, cfg: &config::Config) -> CompactString {
        if let Some(p) = self.provider.as_deref().or(cfg.provider.as_deref()) {
            return CompactString::new(p);
        }
        // PROV-4: log when autodetect picks a provider from env vars
        // so users with multiple API keys set understand which one
        // is being used. Resolution order is fixed and deepseek wins
        // over openrouter if both are present — surprising silent
        // behavior previously.
        if let Some(detected) = crate::provider::auto_detect_provider() {
            eprintln!(
                "info: provider auto-detected from environment: {} (set `--provider` or `provider` in config.json to override)",
                detected,
            );
            return CompactString::new(detected);
        }
        CompactString::new("openrouter")
    }

    pub fn resolve_max_tokens(&self, cfg: &config::Config) -> u64 {
        self.max_tokens.or(cfg.max_tokens).unwrap_or(8192)
    }

    /// Model temperature with `CLI > providers.<default>.options.temperature >
    /// config.temperature > unset` precedence. Clamped to `[0.0, 2.0]` by
    /// the caller (builder).
    pub fn resolve_temperature(&self, cfg: &config::Config) -> Option<f64> {
        if let Some(t) = self.temperature {
            return Some(t);
        }
        if let Some((_, entry)) = cfg.resolve_role(config::ConfigRole::Default)
            && let Some(t) = entry.options_temperature()
        {
            return Some(t);
        }
        cfg.temperature
    }

    pub fn resolve_max_agent_turns(&self, cfg: &config::Config) -> usize {
        self.max_agent_turns.or(cfg.max_agent_turns).unwrap_or(100)
    }

    pub fn resolve_no_context_files(&self, cfg: &config::Config) -> bool {
        self.no_context_files || cfg.no_context_files.unwrap_or(false)
    }

    pub fn resolve_no_tools(&self, cfg: &config::Config) -> bool {
        self.no_tools || cfg.no_tools.unwrap_or(false)
    }

    #[cfg(feature = "lsp")]
    pub fn resolve_lsp_enabled(&self, cfg: &config::Config) -> bool {
        if self.no_lsp || self.no_tools {
            return false;
        }
        match &cfg.lsp {
            Some(c) => c.is_enabled(),
            None => true, // default-on
        }
    }

    pub fn resolve_sandbox(&self, cfg: &config::Config) -> crate::sandbox::SandboxMode {
        if let Some(val) = self.sandbox.as_deref() {
            return crate::sandbox::SandboxMode::parse(Some(val));
        }
        cfg.resolve_sandbox_mode()
    }

    /// Override image for microVM sandbox. None = use default.
    pub fn resolve_microvm_image(&self, cfg: &config::Config) -> Option<String> {
        self.microvm_image
            .clone()
            .or_else(|| cfg.resolve_microvm_image())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn parses_auth_openai_subcommand() {
        let cli = Cli::try_parse_from(["dirge", "auth", "openai"]).unwrap();

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Openai,
            }) => {}
            other => panic!("expected auth openai command, got {other:?}"),
        }
    }

    #[test]
    fn parses_auth_chatgpt_alias_as_openai() {
        let cli = Cli::try_parse_from(["dirge", "auth", "chatgpt"]).unwrap();

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Openai,
            }) => {}
            other => panic!("expected auth openai command from chatgpt alias, got {other:?}"),
        }
    }

    #[test]
    fn help_mentions_auth_and_openai_device_code_prerequisite() {
        let top_level_help = Cli::command().render_help().to_string();
        assert!(top_level_help.contains("auth"));

        let err = match Cli::try_parse_from(["dirge", "auth", "openai", "--help"]) {
            Ok(_) => panic!("--help must return a display-help error"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let openai_help = err.to_string();

        assert!(openai_help.contains("device-code auth"));
        assert!(openai_help.contains("ChatGPT Codex security settings"));
    }
}
