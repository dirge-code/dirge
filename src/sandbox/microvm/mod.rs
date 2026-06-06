//! Hardware-isolated microVM sandbox using libkrun.
//!
//! Provides a long-running VM per session that boots once and stays alive
//! for all tool calls. The VM runs a minimal Linux guest with SSH for
//! command execution and virtio-fs for workspace file access.
//!
//! # Architecture
//!
//! - **runner** — child process that calls `krun_start_enter` (blocking).
//!   The parent communicates via SSH over localhost.
//! - **rootfs** — OCI image pulled via the [`oci`] module, cached, and
//!   cloned per VM session.
//! - **oci** — pure Rust OCI puller (no buildah/skopeo needed).
//! - **ssh** — ephemeral key generation and SSH command execution.
//!
//! # Requirements
//!
//! - `/dev/kvm` accessible (user in `kvm` group)
//! - `libkrun.so` + `libkrunfw.so` installed
//! - `gzip` and `tar` on PATH (for layer extraction)

pub mod oci;
#[cfg(test)]
#[cfg(feature = "sandbox-microvm")]
mod pty_harness;
pub mod rootfs;
pub mod runner;
pub mod ssh;
#[cfg(test)]
mod tests;

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use ssh::{EphemeralKeys, HostKeys, ssh_exec, wait_for_ssh};

/// Configuration for a microVM sandbox session.
#[derive(Debug, Clone)]
pub struct MicrovmConfig {
    /// OCI image reference (e.g. "docker.io/library/fedora:41").
    pub image: String,
    /// Host directory to mount as the VM's workspace (virtio-fs).
    pub workspace: PathBuf,
    /// Number of vCPUs.
    pub cpus: u8,
    /// RAM in MiB.
    pub memory_mib: u32,
    /// SSH port on localhost (0 = auto-pick ephemeral).
    pub ssh_port: u16,
    /// Directory to cache rootfs images.
    pub cache_dir: PathBuf,
}

impl Default for MicrovmConfig {
    fn default() -> Self {
        Self {
            image: "local://dirge-microvm:debian".to_string(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            cpus: 1,
            memory_mib: 512,
            ssh_port: 0,
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("dirge")
                .join("microvm"),
        }
    }
}

impl MicrovmConfig {
    /// Directory where snapshots are stored.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.cache_dir.join("snapshots")
    }

    /// Path to the cached base rootfs for the configured image.
    pub fn cached_base_path(&self) -> PathBuf {
        let image_safe = self.image.replace(['/', ':'], "_");
        self.cache_dir.join(&image_safe).join("base")
    }
}

/// A running microVM sandbox. Created by [`MicrovmSandbox::start`] and
/// cleaned up on drop.
pub struct MicrovmSandbox {
    pub(crate) config: MicrovmConfig,
    ssh_port: u16,
    child: Option<std::process::Child>,
    pub(crate) rootfs_path: Option<PathBuf>,
    pub(crate) keys: Option<EphemeralKeys>,
}

impl std::fmt::Debug for MicrovmSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrovmSandbox")
            .field("config", &self.config)
            .field("ssh_port", &self.ssh_port)
            .field("child_pid", &self.child.as_ref().map(|c| c.id()))
            .field("rootfs_path", &self.rootfs_path)
            .finish()
    }
}

impl MicrovmSandbox {
    /// Build a new sandbox (does NOT start the VM yet).
    pub fn new(config: MicrovmConfig) -> Self {
        Self {
            config,
            ssh_port: 0,
            child: None,
            rootfs_path: None,
            keys: None,
        }
    }

    /// The SSH port (0 if the VM has not been started).
    pub fn ssh_port(&self) -> u16 {
        self.ssh_port
    }

    /// Start the VM: prepare rootfs, spawn runner, wait for SSH.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let rootfs = rootfs::prepare(&self.config.image, &self.config.cache_dir).await?;
        self.rootfs_path = Some(rootfs.path().to_path_buf());
        std::mem::forget(rootfs);

        let keys = EphemeralKeys::generate()?;

        // Generate host keys on the host and inject them into the rootfs.
        // This follows brood-box's approach: host keys are written from the
        // host side so they appear as root-owned inside the VM, avoiding
        // the ownership corruption from non-root OCI layer extraction.
        let host_keys = HostKeys::generate()?;

        // Inject the public key, ensure sandbox user + group exist,
        // and write .krun_config.json so libkrun's built-in init knows
        // what to execute.
        if let Some(ref rootfs_path) = self.rootfs_path {
            // Ensure sandbox user exists (OCI layer extraction order
            // can drop it if the base layer overwrites our overlay).
            let passwd_path = rootfs_path.join("etc").join("passwd");
            let passwd = std::fs::read_to_string(&passwd_path).unwrap_or_default();
            if !passwd.contains("sandbox:") {
                std::fs::write(
                    &passwd_path,
                    format!("{passwd}sandbox:x:1000:1000::/home/sandbox:/bin/sh\n"),
                )?;
            }
            let group_path = rootfs_path.join("etc").join("group");
            let group = std::fs::read_to_string(&group_path).unwrap_or_default();
            if !group.contains("sandbox:") {
                std::fs::write(&group_path, format!("{group}sandbox:x:1000:\n"))?;
            }

            let ssh_dir = rootfs_path.join("home").join("sandbox").join(".ssh");
            std::fs::create_dir_all(&ssh_dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700))?;
                // sshd requires the home directory to NOT be group-writable.
                std::fs::set_permissions(
                    rootfs_path.join("home").join("sandbox"),
                    std::fs::Permissions::from_mode(0o700),
                )?;
            }
            let auth_keys_path = ssh_dir.join("authorized_keys");
            std::fs::write(&auth_keys_path, format!("{}\n", keys.public_key))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&auth_keys_path, std::fs::Permissions::from_mode(0o600))?;
            }

            // Inject host keys into the rootfs. Removes stale keys from
            // the image build and writes fresh ed25519 keys generated on
            // the host. ssh-keygen -A inside the VM fills in RSA/ECDSA.
            host_keys.inject(rootfs_path)?;

            // libkrun reads this JSON to determine the init command.
            // The `mounts` field tells the init to mount tmpfs over
            // /var/empty before launching sshd — the rootfs is
            // host-user-owned and sshd requires root:root /var/empty.
            // Format matches libkrun init's config_parse_mounts.
            let krun_config = serde_json::json!({
                "Cmd": [
                    "/bin/sh", "-c",
                    "mount -t tmpfs tmpfs /run \
                     && mkdir -p /run/sshd \
                     && mkdir -p /workspace \
                     && mount -t virtiofs workspace /workspace \
                     && ssh-keygen -A \
                     && chmod 755 /var/empty \
                     && exec /usr/sbin/sshd -D -e -o StrictModes=no"
                ],
                "mounts": [
                    {"destination": "/var/empty", "type": "tmpfs", "source": "tmpfs"},
                    {"destination": "/workspace", "type": "virtiofs", "source": "workspace"}
                ],
                "Env": [],
                "WorkingDir": "/"
            });
            std::fs::write(
                rootfs_path.join(".krun_config.json"),
                serde_json::to_string_pretty(&krun_config)?,
            )?;
        }

        let port = if self.config.ssh_port == 0 {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            drop(listener);
            port
        } else {
            self.config.ssh_port
        };

        let binary = runner::find_runner_binary()?;
        let config = serde_json::json!({
            "rootfs_path": self.rootfs_path.as_ref().unwrap(),
            "workspace_path": self.config.workspace,
            "ssh_port": port,
            "cpus": self.config.cpus,
            "memory_mib": self.config.memory_mib,
        });
        let child = std::process::Command::new(&binary)
            .arg(serde_json::to_string(&config)?)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn dirge-microvm-runner: {e}"))?;

        // ── scheduler isolation: prevent KVM vCPU threads from ──────
        // starving dirge's input-reader thread.
        //
        // renice -n 19 — lowest CFS priority for the runner and all
        // its child threads (including KVM vCPUs). Combined with
        // setpriority(-10) in the input reader thread, this gives the
        // input reader ~50x scheduling weight over KVM threads,
        // eliminating typing stutter without root.
        // ──────────────────────────────────────────────────────────

        let _renice_status = std::process::Command::new("renice")
            .arg("-n")
            .arg("19")
            .arg("-p")
            .arg(child.id().to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Pin the runner to the last CPU so KVM vCPU threads
        // don't compete with dirge threads on other cores.
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let runner_cpu = cpu_count.saturating_sub(1);
        let _taskset_status = std::process::Command::new("taskset")
            .arg("-cp")
            .arg(runner_cpu.to_string())
            .arg(child.id().to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        self.child = Some(child);
        self.ssh_port = port;
        self.keys = Some(keys);

        // spawn_blocking: wait_for_ssh is a sync loop with thread::sleep.
        // Running it directly would block the async reactor for up to 30s,
        // freezing the TUI during VM boot (first bash call).
        let wait_port = port;
        match tokio::task::spawn_blocking(move || {
            wait_for_ssh("127.0.0.1", wait_port, Duration::from_secs(30))
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // SSH never came up — check if the runner crashed.
                if let Some(mut child) = self.child.take() {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let mut stderr = String::new();
                            if let Some(ref mut pipe) = child.stderr {
                                use std::io::Read;
                                let _ = pipe.read_to_string(&mut stderr);
                            }
                            let status_str = if let Some(code) = status.code() {
                                if code == 127 {
                                    format!(
                                        "{status} — the VM may be missing sshd. \
                                         Try re-running `dirge sandbox setup`"
                                    )
                                } else {
                                    status.to_string()
                                }
                            } else {
                                status.to_string()
                            };
                            anyhow::bail!(
                                "runner exited {status_str} — stderr: {}",
                                if stderr.is_empty() {
                                    "(empty)"
                                } else {
                                    &stderr
                                }
                            );
                        }
                        Ok(None) => {
                            // Still running but no SSH — kill it.
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                        Err(_) => {}
                    }
                }
                return Err(e);
            }
            Err(join_err) => {
                anyhow::bail!("spawn_blocking panicked or failed: {join_err}");
            }
        }

        Ok(())
    }

    /// Execute a command inside the VM via SSH.
    #[allow(dead_code)] // used by tests; async path uses ssh_exec directly via spawn_blocking
    pub fn exec(
        &self,
        command: &str,
        _env: &[(&str, &str)],
        cwd: &str,
    ) -> anyhow::Result<(String, String, i32)> {
        let keys = self
            .keys
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("VM not started"))?;
        let command = format!("cd {} && {}", cwd, command);
        ssh_exec("127.0.0.1", self.ssh_port, &keys.private_key_path, &command)
    }

    /// Shut down the VM gracefully.
    ///
    /// Tries graceful shutdown via SSH first ("poweroff"), then escalates
    /// through SIGTERM → SIGKILL. Rootfs cleanup runs regardless.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            // 1. Fire-and-forget graceful SSH shutdown.
            if let (Some(keys), ssh_port) = (self.keys.as_ref(), self.ssh_port) {
                if ssh_port != 0 {
                    let private_key = keys.private_key_path.clone();
                    std::thread::spawn(move || {
                        let _ = ssh_exec("127.0.0.1", ssh_port, &private_key, "poweroff");
                    });
                    // Give the VM a moment to process the poweroff.
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }

            // 2. If still running, try SIGTERM via kill(1).
            if child.try_wait().ok().flatten().is_none() {
                let pid = child.id();
                let _ = std::process::Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                // Wait up to 5 seconds.
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(5) {
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // 3. SIGKILL as last resort.
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(ref path) = self.rootfs_path {
            rootfs::cleanup(path)?;
        }
        self.ssh_port = 0;
        self.keys = None;
        Ok(())
    }

    // ── snapshot management ──────────────────────────────────────

    /// Reject snapshot names that contain path separators or traversal.
    fn validate_snapshot_name(name: &str) -> anyhow::Result<()> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            anyhow::bail!("invalid snapshot name '{name}'");
        }
        Ok(())
    }

    /// Save a copy of the VM's rootfs as a named snapshot.
    /// VM must have been started (rootfs must exist).
    pub fn save_snapshot(&self, name: &str) -> anyhow::Result<()> {
        Self::validate_snapshot_name(name)?;
        let rootfs = self
            .rootfs_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("VM not started — no rootfs to snapshot"))?;
        let snap_dir = self.config.snapshots_dir().join(name);
        if snap_dir.exists() {
            anyhow::bail!(
                "snapshot '{name}' already exists — delete it first or choose a different name"
            );
        }
        std::fs::create_dir_all(self.config.snapshots_dir())?;
        rootfs::cp_r(rootfs, &snap_dir)?;
        Ok(())
    }

    /// List all saved snapshot names (sorted).
    pub fn list_snapshots(&self) -> anyhow::Result<Vec<String>> {
        let snap_dir = self.config.snapshots_dir();
        if !snap_dir.exists() {
            return Ok(vec![]);
        }
        let mut names: Vec<String> = std::fs::read_dir(&snap_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Restore a snapshot to replace the cached base rootfs.
    /// The VM must be stopped; the restored rootfs takes effect on
    /// the next call to [`start`].
    pub fn restore_snapshot(&self, name: &str) -> anyhow::Result<()> {
        Self::validate_snapshot_name(name)?;
        if self.ssh_port != 0 {
            anyhow::bail!("VM is running — stop it before restoring a snapshot");
        }
        let snap_path = self.config.snapshots_dir().join(name);
        if !snap_path.exists() {
            anyhow::bail!("snapshot '{name}' does not exist");
        }
        let base = self.config.cached_base_path();
        // Remove stale cached base, then clone snapshot back.
        if base.exists() {
            std::fs::remove_dir_all(&base)?;
        }
        std::fs::create_dir_all(base.parent().unwrap())?;
        rootfs::cp_r(&snap_path, &base)?;
        Ok(())
    }

    /// Delete a saved snapshot.
    pub fn delete_snapshot(&self, name: &str) -> anyhow::Result<()> {
        Self::validate_snapshot_name(name)?;
        let snap_path = self.config.snapshots_dir().join(name);
        if !snap_path.exists() {
            anyhow::bail!("snapshot '{name}' does not exist");
        }
        std::fs::remove_dir_all(&snap_path)?;
        Ok(())
    }

    /// Stop the VM and start a fresh one.
    /// This re-clones the rootfs from the cached base so any
    /// in-VM changes are discarded.
    pub async fn reboot(&mut self) -> anyhow::Result<()> {
        self.stop()?;
        self.start().await
    }
}

impl Drop for MicrovmSandbox {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
