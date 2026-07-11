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

    #[arg(
        short = 'r',
        long = "resume",
        help = "Resume the most recent session, listing the last 10 for reference"
    )]
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
    /// Log in to OpenAI using browser OAuth (or device-code auth with --device-code)
    #[command(
        name = "openai",
        visible_alias = "chatgpt",
        long_about = "Log in to OpenAI using browser OAuth by default.\n\nUse --device-code for headless device-code auth; before using that mode, enable device-code auth in ChatGPT Codex security settings."
    )]
    Openai {
        /// Use the headless device-code login flow instead of browser OAuth
        #[arg(long = "device-code")]
        device_code: bool,
    },
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

/// Where the resolved provider name came from. Kept separate from the
/// resolution so the precedence is unit-testable without touching the
/// environment or credential stores, and so each source can log
/// differently (see [`Cli::resolve_provider`]).
#[derive(Debug, PartialEq)]
enum ProviderPick<'a> {
    /// `--provider` flag or `provider` in config.
    Explicit(&'a str),
    /// Autodetected from an API-key env var.
    Env(&'a str),
    /// Implied by a stored `dirge auth` OAuth login.
    Auth(&'a str),
    /// Nothing set — the hard `openrouter` default.
    Default,
}

/// Precedence for provider selection: explicit config wins, then an env
/// API key, then a stored `dirge auth` login, then the default. Pure so
/// the ordering is testable; the caller supplies the detection results.
fn pick_provider<'a>(
    explicit: Option<&'a str>,
    env_detected: Option<&'a str>,
    auth_detected: Option<&'a str>,
) -> ProviderPick<'a> {
    if let Some(p) = explicit {
        ProviderPick::Explicit(p)
    } else if let Some(e) = env_detected {
        ProviderPick::Env(e)
    } else if let Some(a) = auth_detected {
        ProviderPick::Auth(a)
    } else {
        ProviderPick::Default
    }
}

impl Cli {
    /// The provider entry that model / temperature / explicitness resolve
    /// from. When `--provider` / `DIRGE_PROVIDER` overrides the config
    /// default, that is the OVERRIDING provider's own entry (absent →
    /// `None`); the config `Default` role — which tracks `cfg.provider` —
    /// must NOT be consulted, or `--provider openai` would still load the
    /// config-default provider's model/temperature and send it to OpenAI
    /// (404). Without an override, the `Default` role's entry (dirge-314i).
    pub(crate) fn resolution_entry(&self, cfg: &config::Config) -> Option<config::ProviderEntry> {
        if let Some(provider) = self.provider.as_deref().filter(|p| !p.is_empty()) {
            return cfg.providers.as_ref().and_then(|providers| {
                providers
                    .get(provider)
                    .or_else(|| providers.get(&provider.to_ascii_lowercase()))
                    .cloned()
            });
        }
        cfg.resolve_role(config::ConfigRole::Default)
            .map(|(_, entry)| entry)
    }

    pub fn resolve_model(&self, cfg: &config::Config) -> CompactString {
        if let Some(m) = self.model.as_deref() {
            return CompactString::new(m);
        }
        // Model comes from the effective provider entry (the `--provider`
        // override's, or the `Default` role's) — never a cross-provider
        // fall-through to the config default (dirge-314i).
        if let Some(m) = self.resolution_entry(cfg).and_then(|e| e.model) {
            return CompactString::new(&m);
        }
        CompactString::new("deepseek/deepseek-v4-flash")
    }

    pub fn resolve_provider(&self, cfg: &config::Config) -> CompactString {
        let explicit = self.provider.as_deref().or(cfg.provider.as_deref());
        match pick_provider(
            explicit,
            crate::provider::auto_detect_provider(),
            crate::provider::auth_detect_provider(),
        ) {
            ProviderPick::Explicit(p) => CompactString::new(p),
            // PROV-4: log when autodetect picks a provider from env vars
            // so users with multiple API keys set understand which one
            // is being used. Resolution order is fixed and deepseek wins
            // over openrouter if both are present — surprising silent
            // behavior previously.
            ProviderPick::Env(detected) => {
                eprintln!(
                    "info: provider auto-detected from environment: {detected} (set `--provider` or `provider` in config.json to override)",
                );
                CompactString::new(detected)
            }
            // GH #617: a stored `dirge auth` login is enough to launch,
            // even with no API-key env var or `provider` in config.
            ProviderPick::Auth(authed) => {
                eprintln!(
                    "info: provider selected from stored `dirge auth` login: {authed} (set `--provider` or `provider` in config.json to override)",
                );
                CompactString::new(authed)
            }
            ProviderPick::Default => CompactString::new("openrouter"),
        }
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
        // Same effective-entry rule as the model: a `--provider` override
        // reads temperature from ITS entry, not the config-default provider's
        // (dirge-314i).
        if let Some(t) = self
            .resolution_entry(cfg)
            .and_then(|e| e.options_temperature())
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
                action: AuthAction::Openai { device_code: false },
            }) => {}
            other => panic!("expected auth openai command, got {other:?}"),
        }
    }

    #[test]
    fn pick_provider_prefers_explicit_over_everything() {
        let pick = pick_provider(Some("glm"), Some("openai"), Some("anthropic"));
        assert!(matches!(pick, ProviderPick::Explicit("glm")));
    }

    #[test]
    fn pick_provider_uses_env_when_no_explicit() {
        let pick = pick_provider(None, Some("openai"), Some("anthropic"));
        assert!(matches!(pick, ProviderPick::Env("openai")));
    }

    /// GH #617: with no explicit config and no env key, a stored
    /// `dirge auth` login is used before the openrouter default.
    #[test]
    fn pick_provider_uses_auth_login_when_no_explicit_or_env() {
        let pick = pick_provider(None, None, Some("openai"));
        assert!(matches!(pick, ProviderPick::Auth("openai")));
    }

    #[test]
    fn pick_provider_falls_back_to_default_when_nothing_set() {
        let pick = pick_provider(None, None, None);
        assert!(matches!(pick, ProviderPick::Default));
    }

    #[test]
    fn parses_auth_chatgpt_alias_as_openai() {
        let cli = Cli::try_parse_from(["dirge", "auth", "chatgpt"]).unwrap();

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Openai { device_code: false },
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

        assert!(openai_help.contains("browser OAuth"));
        assert!(openai_help.contains("device-code auth"));
        assert!(openai_help.contains("ChatGPT Codex security settings"));
    }

    fn cfg_with_glm_default_and_ollama() -> config::Config {
        use std::collections::HashMap;
        let providers = HashMap::from([
            (
                "glm".to_string(),
                config::ProviderEntry {
                    provider_type: Some("glm".to_string()),
                    model: Some("glm-5.2".to_string()),
                    ..Default::default()
                },
            ),
            (
                "ollama".to_string(),
                config::ProviderEntry {
                    provider_type: Some("openai".to_string()),
                    base_url: Some("http://127.0.0.1:11434/v1".to_string()),
                    model: Some("vibe-thinker:latest".to_string()),
                    ..Default::default()
                },
            ),
        ]);
        config::Config {
            provider: Some("glm".to_string()),
            providers: Some(providers),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_model_honors_provider_override() {
        // `--provider ollama` (no --model) must take ollama's pinned model, not
        // the config default provider's (glm) — otherwise the endpoint switches
        // but the model stays glm-5.2.
        let cli = Cli::parse_from(["dirge", "--provider", "ollama"]);
        assert_eq!(
            cli.resolve_model(&cfg_with_glm_default_and_ollama()),
            "vibe-thinker:latest"
        );
    }

    #[test]
    fn resolve_model_without_override_uses_config_default() {
        let cli = Cli::parse_from(["dirge"]);
        assert_eq!(
            cli.resolve_model(&cfg_with_glm_default_and_ollama()),
            "glm-5.2"
        );
    }

    #[test]
    fn resolve_model_explicit_model_flag_wins_over_provider() {
        let cli = Cli::parse_from(["dirge", "--provider", "ollama", "--model", "llama3.1"]);
        assert_eq!(
            cli.resolve_model(&cfg_with_glm_default_and_ollama()),
            "llama3.1"
        );
    }

    #[test]
    fn resolve_model_override_to_provider_without_entry_does_not_leak_config_default() {
        // dirge-314i: `--provider openai` with NO openai entry must NOT fall
        // through to the config-default (glm) provider's model — sending
        // glm-5.2 to an OpenAI client 404s. resolution_entry returns None, so
        // the model is NOT the config default's; main.rs then sees
        // model_explicit=false and picks OpenAI's own default via
        // default_model_for_alias.
        let cli = Cli::parse_from(["dirge", "--provider", "openai"]);
        let got = cli.resolve_model(&cfg_with_glm_default_and_ollama());
        assert_ne!(
            got, "glm-5.2",
            "override to a provider without an entry must not load the config-default model"
        );
        // And with no explicit model, the override provider yields no
        // config_model, so it is treated as NOT explicit.
        assert!(
            cli.resolution_entry(&cfg_with_glm_default_and_ollama())
                .is_none()
        );
    }

    #[test]
    fn resolve_temperature_override_ignores_config_default_providers_temp() {
        // dirge-314i: give the config-default (glm) a temperature via options;
        // override to `openai` (no entry). resolve_temperature must NOT return
        // glm's — it falls through to config.temperature / unset instead.
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "glm".to_string(),
            config::ProviderEntry {
                model: Some("glm-5.2".to_string()),
                options: Some(
                    serde_json::json!({ "temperature": 0.9 })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..Default::default()
            },
        );
        let cfg = config::Config {
            provider: Some("glm".to_string()),
            providers: Some(providers),
            ..Default::default()
        };
        let cli = Cli::parse_from(["dirge", "--provider", "openai"]);
        assert_eq!(
            cli.resolve_temperature(&cfg),
            None,
            "override to a provider without an entry must not inherit glm's temperature"
        );
        // Sanity: without the override, glm's temperature IS used.
        let cli_no_override = Cli::parse_from(["dirge"]);
        assert_eq!(cli_no_override.resolve_temperature(&cfg), Some(0.9));
    }

    #[test]
    fn parses_auth_openai_device_code_option() {
        let cli = Cli::try_parse_from(["dirge", "auth", "openai", "--device-code"]).unwrap();

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Openai { device_code: true },
            }) => {}
            other => panic!("expected auth openai --device-code command, got {other:?}"),
        }
    }
}
