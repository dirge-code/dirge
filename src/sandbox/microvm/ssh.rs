//! Ephemeral SSH key generation and command execution for the microVM sandbox.

use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An ephemeral SSH key pair for authenticating with the guest VM.
pub struct EphemeralKeys {
    /// Path to the temporary private key file.
    pub private_key_path: PathBuf,
    /// The public key content (for authorized_keys injection in rootfs hooks).
    pub public_key: String,
    /// The temp directory holding the keys (cleaned on drop).
    _temp_dir: PathBuf,
}

impl EphemeralKeys {
    /// Generate a new ed25519 key pair using the `ssh-keygen` CLI.
    pub fn generate() -> anyhow::Result<Self> {
        let dir = temp_dir("dirge-ssh")?;
        let key_path = dir.join("id_ed25519");
        run_ssh_keygen(&key_path)?;
        let pubkey_path = key_path.with_extension("pub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let public_key = std::fs::read_to_string(&pubkey_path)
            .map_err(|e| anyhow::anyhow!("failed to read public key: {e}"))?
            .trim()
            .to_string();

        Ok(Self {
            public_key,
            private_key_path: key_path,
            _temp_dir: dir,
        })
    }
}

/// Pre-generated SSH host key pair, injectable into the rootfs before boot.
///
/// This follows brood-box's approach: host keys are generated on the
/// host and written into the rootfs as files. Inside the VM they appear
/// as root-owned files because libkrun's init runs as root. This avoids
/// the ownership corruption that occurs when OCI layer tarballs are
/// extracted as a non-root user.
pub struct HostKeys {
    /// The private key content (PEM).
    pub private_key_pem: Vec<u8>,
    /// The public key content (for ssh_host_ed25519_key.pub).
    pub public_key: String,
    /// The temporary directory, cleaned up on drop.
    _temp_dir: PathBuf,
}

impl HostKeys {
    /// Generate an ed25519 host key pair.
    pub fn generate() -> anyhow::Result<Self> {
        let dir = temp_dir("dirge-host-key")?;
        let key_path = dir.join("ssh_host_ed25519_key");
        run_ssh_keygen(&key_path)?;
        let private_key_pem = std::fs::read(&key_path)
            .map_err(|e| anyhow::anyhow!("failed to read host key: {e}"))?;
        let pubkey_path = key_path.with_extension("pub");
        let public_key = std::fs::read_to_string(&pubkey_path)
            .map_err(|e| anyhow::anyhow!("failed to read host public key: {e}"))?
            .trim()
            .to_string();
        Ok(Self {
            private_key_pem,
            public_key,
            _temp_dir: dir,
        })
    }

    /// Write the host key into a rootfs so sshd can find it at boot.
    /// Writes both the private key and the public key, and removes any
    /// stale host keys left over from the image build to prevent
    /// mismatches.
    pub fn inject(&self, rootfs: &Path) -> anyhow::Result<()> {
        let ssh_dir = rootfs.join("etc").join("ssh");
        std::fs::create_dir_all(&ssh_dir)?;

        // Remove stale host keys generated at image build time.
        // Only our injected ed25519 keys should be present at boot.
        if let Ok(entries) = std::fs::read_dir(&ssh_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("ssh_host_") && name_str.contains("_key") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        let host_key_path = ssh_dir.join("ssh_host_ed25519_key");
        std::fs::write(&host_key_path, &self.private_key_pem)?;
        let pubkey_path = ssh_dir.join("ssh_host_ed25519_key.pub");
        std::fs::write(&pubkey_path, format!("{}\n", self.public_key))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&host_key_path, std::fs::Permissions::from_mode(0o600))?;
            std::fs::set_permissions(&pubkey_path, std::fs::Permissions::from_mode(0o644))?;
        }
        Ok(())
    }
}

impl Drop for HostKeys {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._temp_dir);
    }
}

fn temp_dir(prefix: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("failed to create temp dir: {e}"))?;
    Ok(dir)
}

fn run_ssh_keygen(key_path: &Path) -> anyhow::Result<()> {
    let output = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &key_path.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run ssh-keygen: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

impl Drop for EphemeralKeys {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._temp_dir);
    }
}

/// Wait for the SSH server to become reachable on the given port.
pub fn wait_for_ssh(host: &str, port: u16, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        match TcpStream::connect_timeout(
            &format!("{host}:{port}")
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid address: {e}"))?,
            Duration::from_millis(500),
        ) {
            Ok(_) => return Ok(()),
            Err(_) => {
                if start.elapsed() > timeout {
                    anyhow::bail!("timed out waiting for SSH on {host}:{port}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Execute a command via SSH and return (stdout, stderr, exit_code).
pub fn ssh_exec(
    host: &str,
    port: u16,
    private_key_path: &Path,
    command: &str,
) -> anyhow::Result<(String, String, i32)> {
    let tcp = TcpStream::connect(format!("{host}:{port}"))
        .map_err(|e| anyhow::anyhow!("failed to connect to SSH: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(60)))?;

    let mut session =
        ssh2::Session::new().map_err(|e| anyhow::anyhow!("failed to create SSH session: {e}"))?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| anyhow::anyhow!("SSH handshake failed: {e}"))?;

    session
        .userauth_pubkey_file("sandbox", None, private_key_path, None)
        .map_err(|e| {
            anyhow::anyhow!(
                "SSH authentication failed: {e}\n\
             If using a microVM, virtio-fs maps host files as root-owned inside the guest, \
             which causes sshd's StrictModes check to reject the authorized_keys file. \
             Ensure the VM image has sshd configured with `-o StrictModes=no`."
            )
        })?;

    let mut channel = session
        .channel_session()
        .map_err(|e| anyhow::anyhow!("failed to open SSH channel: {e}"))?;
    channel
        .exec(command)
        .map_err(|e| anyhow::anyhow!("failed to exec command: {e}"))?;

    let mut stdout = String::new();
    channel.read_to_string(&mut stdout)?;

    let mut stderr = String::new();
    let mut stderr_stream = channel.stderr();
    stderr_stream.read_to_string(&mut stderr)?;

    channel
        .wait_close()
        .map_err(|e| anyhow::anyhow!("failed to wait for channel close: {e}"))?;
    let exit_code = channel.exit_status().unwrap_or(-1);

    Ok((stdout, stderr, exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_exec_connection_refused() {
        // Pick a port where nothing is listening.
        // Binding to port 0 and then closing gives us a guaranteed-free port.
        let free_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            port
        };

        let tmp_key = std::env::temp_dir().join(format!(
            "dirge-test-key-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let result = ssh_exec("127.0.0.1", free_port, &tmp_key, "echo hi");
        assert!(
            result.is_err(),
            "ssh_exec to free port should fail, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("failed to connect to SSH") || msg.contains("SSH handshake failed"),
            "error should mention connection failure, got: {msg}"
        );
    }

    #[test]
    fn ssh_exec_handshake_timeout_not_hang() {
        // Connect to a port that accepts TCP but doesn't speak SSH.
        // Use a short-lived listener that accepts then immediately drops.
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn a thread that accepts one connection and immediately closes it.
        // This simulates a non-SSH server — TCP connect succeeds but SSH
        // handshake will fail because the server sends nothing.
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });

        let tmp_key = std::env::temp_dir().join(format!(
            "dirge-test-key2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let result = ssh_exec("127.0.0.1", port, &tmp_key, "echo hi");
        assert!(
            result.is_err(),
            "ssh_exec to non-SSH port should fail, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("SSH handshake failed") || msg.contains("failed to connect"),
            "error should mention handshake failure, got: {msg}"
        );
    }

    #[test]
    fn ssh_exec_invalid_hostname_fails_fast() {
        // Use a hostname in the reserved .invalid TLD (RFC 6761) that
        // will never resolve. Ensures DNS failure doesn't hang.
        let tmp_key = std::env::temp_dir().join(format!(
            "dirge-test-key3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let start = std::time::Instant::now();
        let result = ssh_exec("nonexistent.invalid", 22, &tmp_key, "echo hi");
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "ssh_exec to unresolvable hostname should fail, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("failed to connect to SSH"),
            "error should mention connection failure, got: {msg}"
        );
        // Must fail fast — DNS resolution shouldn't take more than 10s.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "DNS resolution took {:?}, expected <10s",
            elapsed
        );
    }

    #[test]
    fn wait_for_ssh_invalid_address() {
        // A address string that cannot be parsed as a socket address.
        let result = wait_for_ssh("not a valid host", 22, Duration::from_millis(500));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid address"),
            "expected 'invalid address', got: {msg}"
        );
    }
}
