//! Clojure addon host (cargo feature `addons`).
//!
//! Loads addons written against an IAddon protocol in portable `.cljc` into
//! an embedded clojurust interpreter. An addon ships
//! `resources/META-INF/addons/<id>.edn`; its `tools` become loop tools, its
//! `:dirge/*` hooks run at dirge's hook points and its `:dirge/commands`
//! become slash commands. `/addons reload` swaps all of it in place. See
//! docs/addons.md.

pub mod cljrs;
pub mod command_hooks;
pub mod discovery;
pub mod domain;
pub mod events;
pub mod host;
pub mod layout;
pub mod lifecycle;
pub mod live;
pub mod loop_hooks;
pub mod manifest;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod policy;
pub mod port;
pub mod sink;
pub mod tool;
#[cfg(feature = "plugin")]
pub mod tool_calls;

#[cfg(test)]
mod acceptance_tests;
#[cfg(test)]
mod live_tests;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::json;

use cljrs::isolate::IsolateOptions;
use domain::{AddonPlan, LoadFailure, ReloadReport};
use host::{AddonHost, LoadSet};
use port::Harness;

/// Protocol namespace used when `addons.protocol_ns` is not set.
pub const DEFAULT_PROTOCOL_NS: &str = "hive-addon.protocol";

static HOST: OnceLock<Arc<AddonHost>> = OnceLock::new();

/// The process-wide host, once [`install_from_config`] or [`reload`]
/// started one.
pub fn global() -> Option<Arc<AddonHost>> {
    HOST.get().cloned()
}

/// Discover and load addons for this process. A no-op when disabled or when
/// no manifest is found, so a build with the feature costs nothing until an
/// addon is installed. Failures are logged, never fatal.
///
/// Called on the thread that runs dirge's single-threaded event loop, which
/// it marks: prompt hooks, loading and shutdown reach the isolate from that
/// thread, and addon code must not wait on the loop while it waits.
pub fn install_from_config(cfg: &crate::config::Config) {
    cljrs::isolate::mark_event_loop_thread();
    // `type: "addon"` command hooks reach whatever host runs at call time,
    // this one or the one a later `/addons reload` starts.
    crate::agent::command_hooks::boundary::install_addon_runner(Arc::new(
        command_hooks::LiveAddonHookRunner,
    ));
    let settings = cfg.addons.clone().unwrap_or_default();
    if settings.enabled == Some(false) {
        return;
    }
    let plan = discovery::plan(&search_dirs(&settings), &extra_roots(&settings));
    if plan.is_empty() {
        return;
    }
    match start_with(
        plan,
        harness(),
        protocol_ns(&settings),
        isolate_options(&settings),
    ) {
        Ok(host) => {
            for failure in host.failures() {
                tracing::warn!(
                    target: "dirge::addon",
                    manifest = %failure.manifest.display(),
                    error = %failure.error,
                    "addon failed to load"
                );
            }
            tracing::info!(
                target: "dirge::addon",
                addons = host.addons().len(),
                tools = host.tools().len(),
                "addon host started"
            );
            publish(Arc::new(host));
        }
        Err(error) => {
            tracing::warn!(target: "dirge::addon", %error, "addon host did not start");
        }
    }
}

/// Discover again and replace every addon in place: the host's reload when
/// one is running, a fresh start when dirge booted without addons. Blocks
/// on the isolate; call it off the async runtime (`spawn_blocking`).
pub fn reload(
    settings: &crate::config::AddonsConfig,
) -> Result<(Arc<AddonHost>, ReloadReport), String> {
    if settings.enabled == Some(false) {
        return Err("addons are disabled (addons.enabled is false)".to_string());
    }
    let plan = discovery::plan(&search_dirs(settings), &extra_roots(settings));
    if let Some(host) = global() {
        let report = host.reload(load_set(&plan, true));
        register_commands(&host);
        return Ok((host, report));
    }
    let host = Arc::new(start_with(
        plan,
        harness(),
        protocol_ns(settings),
        isolate_options(settings),
    )?);
    let report = ReloadReport {
        loaded: host.addons().into_iter().map(|a| a.id).collect(),
        failures: host.failures(),
        tools_added: host.tools().into_iter().map(|t| t.exposed_name).collect(),
        ..ReloadReport::default()
    };
    publish(host.clone());
    Ok((global().unwrap_or(host), report))
}

/// Shut every addon down. Call once on exit.
pub fn shutdown() {
    if let Some(host) = global() {
        host.shutdown();
    }
}

/// Validate, boot, load: the plan becomes a running host. Manifests that
/// fail validation or loading are kept as [`LoadFailure`]s beside the
/// addons that loaded.
#[cfg_attr(not(test), allow(dead_code))]
pub fn start(plan: AddonPlan, harness: Harness, protocol_ns: &str) -> Result<AddonHost, String> {
    start_with(plan, harness, protocol_ns, IsolateOptions::default())
}

/// [`start`], the runtime set up by `options`.
pub fn start_with(
    plan: AddonPlan,
    harness: Harness,
    protocol_ns: &str,
    options: IsolateOptions,
) -> Result<AddonHost, String> {
    let set = load_set(&plan, false);
    if set.manifests.is_empty() {
        return Err(describe_failures(&set.failures));
    }
    let isolate = Arc::new(cljrs::Isolate::spawn(
        set.source_roots.clone(),
        harness,
        protocol_ns,
        options,
    )?);
    let host_config = json!({ "harness": "dirge", "version": env!("CARGO_PKG_VERSION") });
    Ok(AddonHost::load(isolate, set, host_config))
}

/// What loading `plan` works from. `with_sources` lists the addons' own
/// source files, which only a reload evaluates.
fn load_set(plan: &AddonPlan, with_sources: bool) -> LoadSet {
    let (manifests, failures) = validate(&plan.manifests, &plan.source_roots);
    let sources = if with_sources {
        discovery::own_sources(&manifests)
    } else {
        Vec::new()
    };
    LoadSet {
        manifests,
        failures,
        source_roots: plan.source_roots.clone(),
        sources,
    }
}

fn publish(host: Arc<AddonHost>) {
    register_commands(&host);
    let _ = HOST.set(host);
}

/// Hand `host`'s addons the command names left to them: names a built-in
/// or plugin command takes are withheld, the rest complete on Tab.
fn register_commands(host: &AddonHost) {
    host.reserve_commands(Arc::new(taken_by_dirge));
    #[cfg(feature = "slash-completion")]
    crate::ui::slash::register_addon_commands(
        host.commands().into_iter().map(|c| c.name).collect(),
    );
}

/// True when `name` (without the `/`) is a built-in or plugin slash
/// command, which dirge dispatches before any addon command.
fn taken_by_dirge(name: &str) -> bool {
    if crate::ui::slash::is_known_slash_command(&format!("/{name}")) {
        return true;
    }
    #[cfg(feature = "plugin")]
    if let Some(plugins) = crate::plugin::hook::global() {
        use crate::sync_util::LockExt;
        return plugins
            .lock_ignore_poison()
            .list_commands()
            .iter()
            .any(|(taken, _)| taken == name);
    }
    false
}

/// What `dirge.harness` reaches in this process: the TUI for notifications
/// and panels, dirge's loop tools, and the MCP servers dirge connects to.
fn harness() -> Harness {
    let tui = Arc::new(sink::TuiSink);
    let mut harness = Harness::with_sink(tui.clone());
    harness.panels = tui;
    #[cfg(feature = "plugin")]
    if let Some(live) = tool_calls::LoopTools::live() {
        harness.tools = Arc::new(live);
    }
    #[cfg(not(feature = "plugin"))]
    {
        harness.tools = Arc::new(port::ToolsUnavailable(
            "call-tool is unavailable in this build: dirge was built without the `plugin` feature",
        ));
    }
    #[cfg(feature = "mcp")]
    if let Some(live) = mcp::LiveMcp::current() {
        harness.mcp = Arc::new(live);
    }
    harness
}

/// How the runtime is set up: whether it serves an nREPL, and whether a
/// REPL evaluation re-reads the addons.
fn isolate_options(settings: &crate::config::AddonsConfig) -> IsolateOptions {
    IsolateOptions {
        #[cfg(feature = "addons-nrepl")]
        repl: repl_options(
            settings.nrepl.as_ref(),
            std::env::var("DIRGE_ADDON_NREPL").ok().as_deref(),
        ),
        refresh_after_eval: settings.live_refresh != Some(false),
    }
}

/// Default file the addon nREPL's port is written to, under the working
/// directory.
#[cfg(feature = "addons-nrepl")]
pub const DEFAULT_REPL_PORT_FILE: &str = ".dirge/addons/.nrepl-port";

/// The nREPL `config` and the `DIRGE_ADDON_NREPL` value `env` ask for.
/// `env` wins: `0`, `false`, `off` or `no` start none; `1`, `true`, `on` or
/// `yes` start one on the configured (or an OS-picked) port; any other
/// number listens on that port.
#[cfg(feature = "addons-nrepl")]
fn repl_options(
    config: Option<&crate::config::AddonsNreplConfig>,
    env: Option<&str>,
) -> Option<cljrs::isolate::ReplOptions> {
    let env = env.map(str::trim).filter(|v| !v.is_empty());
    let env_port = match env {
        Some("0" | "false" | "off" | "no") => return None,
        Some("1" | "true" | "on" | "yes") => None,
        Some(v) => v.parse::<u16>().ok(),
        None => None,
    };
    if env.is_none() && config.is_none_or(|c| c.enabled == Some(false)) {
        return None;
    }
    let default = crate::config::AddonsNreplConfig::default();
    let config = config.unwrap_or(&default);
    let ip: std::net::IpAddr = config
        .bind
        .as_deref()
        .and_then(|b| b.parse().ok())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let port = env_port.or(config.port).unwrap_or(0);
    let port_file = match config.port_file.as_deref() {
        Some("") => None,
        Some(file) => Some(expand_home(file)),
        None => Some(PathBuf::from(DEFAULT_REPL_PORT_FILE)),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Some(cljrs::isolate::ReplOptions {
        addr: std::net::SocketAddr::new(ip, port),
        port_file: port_file.map(|f| if f.is_absolute() { f } else { cwd.join(f) }),
    })
}

fn protocol_ns(settings: &crate::config::AddonsConfig) -> &str {
    settings
        .protocol_ns
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL_NS)
}

/// Manifests that parse and whose init namespace has portable source.
/// JVM-only addons sharing a repo with portable ones are skipped quietly.
fn validate(manifests: &[PathBuf], roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<LoadFailure>) {
    let mut valid = Vec::new();
    let mut failures = Vec::new();
    for path in manifests {
        match manifest::AddonManifest::from_path(path) {
            Ok(m) if discovery::has_portable_source(roots, &m.init_ns) => valid.push(path.clone()),
            Ok(m) => tracing::debug!(
                target: "dirge::addon",
                manifest = %path.display(),
                init_ns = %m.init_ns,
                "skipped: no .cljc/.cljrs source for the init namespace"
            ),
            Err(e) => failures.push(LoadFailure {
                manifest: path.clone(),
                error: e.to_string(),
            }),
        }
    }
    (valid, failures)
}

fn describe_failures(failures: &[LoadFailure]) -> String {
    let lines: Vec<String> = failures
        .iter()
        .map(|f| format!("{}: {}", f.manifest.display(), f.error))
        .collect();
    format!("no loadable addon manifest ({})", lines.join("; "))
}

/// Where manifests are searched: the project's `.dirge/addons/`, the user's
/// `~/.config/dirge/addons/`, then configured `addons.paths`.
fn search_dirs(settings: &crate::config::AddonsConfig) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dirs = vec![
        crate::extras::dirge_paths::ProjectPaths::new(&cwd)
            .dirge_dir()
            .join("addons"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".config").join("dirge").join("addons"));
    }
    dirs.extend(settings.paths.iter().map(|p| expand_home(p)));
    dirs
}

/// Extra source roots: configured `addons.source_paths`, then
/// `DIRGE_ADDON_PATH` (a PATH-style list).
fn extra_roots(settings: &crate::config::AddonsConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = settings
        .source_paths
        .iter()
        .map(|p| expand_home(p))
        .collect();
    if let Some(list) = std::env::var_os("DIRGE_ADDON_PATH") {
        roots.extend(std::env::split_paths(&list));
    }
    roots
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(all(test, feature = "addons-nrepl"))]
mod repl_option_tests {
    use super::*;
    use crate::config::AddonsNreplConfig;

    #[test]
    fn no_key_and_no_env_starts_no_repl() {
        assert!(repl_options(None, None).is_none());
    }

    #[test]
    fn a_present_key_starts_one_on_loopback_with_the_default_port_file() {
        let repl = repl_options(Some(&AddonsNreplConfig::default()), None).expect("a repl");
        assert_eq!(repl.addr, std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
        assert!(repl.port_file.unwrap().ends_with(DEFAULT_REPL_PORT_FILE));
    }

    #[test]
    fn the_env_overrides_the_key_both_ways() {
        let off = AddonsNreplConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let repl = repl_options(Some(&off), Some("7888")).expect("env turns it on");
        assert_eq!(repl.addr.port(), 7888);
        assert!(repl_options(Some(&AddonsNreplConfig::default()), Some("off")).is_none());
        let on = repl_options(None, Some("1")).expect("env alone turns it on");
        assert_eq!(on.addr.port(), 0, "1 means on, not port 1");
    }

    #[test]
    fn an_empty_port_file_writes_none() {
        let config = AddonsNreplConfig {
            port_file: Some(String::new()),
            port: Some(7000),
            ..Default::default()
        };
        let repl = repl_options(Some(&config), None).expect("a repl");
        assert!(repl.port_file.is_none());
        assert_eq!(repl.addr.port(), 7000);
    }
}
