//! Sandbox backend abstraction.
//!
//! The [`SandboxBackend`] trait decouples the sandbox execution model from
//! the outer [`Sandbox`](super::Sandbox) wrapper. Two backends exist:
//!
//! - **NoopBackend** — local execution (Off / Bwrap modes). All microVM
//!   methods are no-ops or return "not available" errors.
//! - **MicrovmBackend** — hardware-isolated microVM via libkrun (only
//!   available with the `sandbox-microvm` feature).

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::process::Command;

use super::SandboxMode;
use crate::agent::tools::bash::exec::InterleavedOutput;
use crate::agent::tools::ToolError;

/// Abstraction over sandbox execution backends.
///
/// Default implementations on microVM-specific methods return "not available"
/// errors, so only the `exec` + `wrap_command` path is mandatory.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Whether this backend provides hardware isolation.
    fn is_microvm(&self) -> bool {
        false
    }

    /// Execute a command through the sandbox.
    async fn exec(
        &self,
        command: &str,
        timeout_secs: u64,
    ) -> Result<InterleavedOutput, ToolError>;

    /// Wrap a command for local process execution (bwrap or bare bash).
    fn wrap_command(&self, command: &str) -> Command;

    /// Override the microVM image. No-op for non-microvm backends.
    fn set_microvm_image(&self, _image: String) {}

    /// Override microVM vCPUs and RAM. No-op for non-microvm backends.
    fn set_microvm_resources(&self, _cpus: u8, _memory_mib: u32) {}

    /// Return SSH connection info when the microVM is running.
    fn ssh_connect_info(&self) -> Option<(u16, PathBuf)> {
        None
    }

    /// Save a named snapshot of the VM's rootfs.
    fn save_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// List saved snapshots.
    fn list_snapshots(&self) -> Result<Vec<String>, anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Restore a snapshot (replaces cached base rootfs). VM must be stopped.
    fn restore_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Delete a saved snapshot.
    fn delete_snapshot(&self, _name: &str) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }

    /// Reboot the microVM: stop, re-clone rootfs from cache, start.
    async fn reboot_microvm(&self) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("microVM sandbox not available"))
    }
}

// ── NoopBackend ───────────────────────────────────────────────────

/// Local-execution backend for Off and Bwrap sandbox modes.
pub struct NoopBackend {
    mode: SandboxMode,
}

impl NoopBackend {
    pub fn new(mode: SandboxMode) -> Self {
        Self { mode }
    }
}

#[async_trait]
impl SandboxBackend for NoopBackend {
    fn is_microvm(&self) -> bool {
        false
    }

    async fn exec(
        &self,
        command: &str,
        timeout_secs: u64,
    ) -> Result<InterleavedOutput, ToolError> {
        crate::agent::tools::bash::exec::run_with_timeout(
            self.wrap_command(command),
            timeout_secs,
        )
        .await
    }

    fn wrap_command(&self, command: &str) -> Command {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let mut cmd = if self.mode == SandboxMode::Off {
            let mut c = Command::new("bash");
            c.arg("-c").arg(command);
            c
        } else {
            let mut c = Command::new("bwrap");
            c.args(["--ro-bind", "/", "/", "--bind"]);
            c.arg(cwd.as_os_str());
            c.arg(cwd.as_os_str());
            c.args([
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--unshare-all",
                "--new-session",
                "--unshare-user-try",
                "--die-with-parent",
                "bash",
                "-c",
                command,
            ]);
            c
        };
        super::scrub_env(&mut cmd);
        cmd
    }
}

// ── MicrovmBackend ────────────────────────────────────────────────

/// Hardware-isolated microVM backend (requires `sandbox-microvm` feature).
#[cfg(feature = "sandbox-microvm")]
pub struct MicrovmBackend {
    inner: std::sync::Arc<tokio::sync::Mutex<super::microvm::MicrovmSandbox>>,
}

#[cfg(feature = "sandbox-microvm")]
impl MicrovmBackend {
    pub fn new(mv: super::microvm::MicrovmSandbox) -> Self {
        Self {
            inner: std::sync::Arc::new(tokio::sync::Mutex::new(mv)),
        }
    }
}

#[cfg(feature = "sandbox-microvm")]
#[async_trait]
impl SandboxBackend for MicrovmBackend {
    fn is_microvm(&self) -> bool {
        true
    }

    async fn exec(
        &self,
        command: &str,
        _timeout_secs: u64,
    ) -> Result<InterleavedOutput, ToolError> {
        let mut guard = self.inner.lock().await;
        if guard.ssh_port() == 0 {
            guard
                .start()
                .await
                .map_err(|e| ToolError::Msg(e.to_string()))?;
        }
        let ssh_port = guard.ssh_port();
        let private_key_path = guard
            .keys
            .as_ref()
            .map(|k| k.private_key_path.clone())
            .ok_or_else(|| ToolError::Msg("VM keys missing".to_string()))?;
        drop(guard);

        let command = format!("cd /workspace && {}", command);
        let (stdout, stderr, exit_code) = tokio::task::spawn_blocking(move || {
            super::microvm::ssh::ssh_exec(
                "127.0.0.1",
                ssh_port,
                &private_key_path,
                &command,
            )
        })
        .await
        .map_err(|e| ToolError::Msg(format!("microvm exec join error: {e}")))?
        .map_err(|e| ToolError::Msg(e.to_string()))?;

        Ok(InterleavedOutput {
            merged: if stderr.is_empty() {
                stdout
            } else {
                format!("{stdout}\n{stderr}")
            },
            exit_code,
        })
    }

    fn wrap_command(&self, command: &str) -> Command {
        // Microvm never uses wrap_command; exec goes through SSH.
        // Return a bare bash command as a fallback for any edge case.
        let mut c = Command::new("bash");
        c.arg("-c").arg(command);
        c
    }

    fn set_microvm_image(&self, image: String) {
        use super::microvm::rootfs;
        let canonical = rootfs::canonicalize_image_ref(&image);
        if let Ok(mut guard) = self.inner.try_lock() {
            guard.config.image = canonical;
        }
    }

    fn set_microvm_resources(&self, cpus: u8, memory_mib: u32) {
        if let Ok(mut guard) = self.inner.try_lock() {
            guard.config.cpus = cpus;
            guard.config.memory_mib = memory_mib;
        }
    }

    fn ssh_connect_info(&self) -> Option<(u16, PathBuf)> {
        let guard = self.inner.try_lock().ok()?;
        if guard.ssh_port() == 0 {
            return None;
        }
        let key_path = guard.keys.as_ref()?.private_key_path.clone();
        Some((guard.ssh_port(), key_path))
    }

    fn save_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .inner
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        guard.save_snapshot(name)
    }

    fn list_snapshots(&self) -> Result<Vec<String>, anyhow::Error> {
        let guard = self
            .inner
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        guard.list_snapshots()
    }

    fn restore_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .inner
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        guard.restore_snapshot(name)
    }

    fn delete_snapshot(&self, name: &str) -> Result<(), anyhow::Error> {
        let guard = self
            .inner
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cannot acquire microvm lock — try again"))?;
        guard.delete_snapshot(name)
    }

    async fn reboot_microvm(&self) -> Result<(), anyhow::Error> {
        let mut guard = self.inner.lock().await;
        guard.reboot().await
    }
}
