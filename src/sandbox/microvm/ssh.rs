//! Ephemeral SSH key generation and command execution for the microVM sandbox.

use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;

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

    /// Return the raw 32-byte ed25519 public key for host-key verification.
    ///
    /// Decodes the OpenSSH-format public key (`ssh-ed25519 <base64>`) stored
    /// in [`Self::public_key`] into the bare ed25519 key bytes suitable for
    /// comparison against [`ssh2::Session::host_key`].
    pub fn public_key_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let encoded = self
            .public_key
            .strip_prefix("ssh-ed25519 ")
            .ok_or_else(|| anyhow::anyhow!("host public key has unexpected format"))?;
        // Strip optional trailing comment (some ssh-keygen versions append
        // " root@host" after the base64 data).
        let encoded = encoded.split_whitespace().next().unwrap_or(encoded);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| anyhow::anyhow!("failed to base64-decode host public key: {e}"))?;
        // SSH wire format: [u32 len][algo name]["ssh-ed25519"][u32 len][raw key]
        // For ed25519, the algo name is 11 bytes, so skip 4+11+4 = 19 bytes.
        if decoded.len() < 19 + 32 {
            anyhow::bail!("host public key too short for ed25519");
        }
        let algo_len =
            u32::from_be_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]) as usize;
        if algo_len + 4 > decoded.len() || &decoded[4..4 + algo_len] != b"ssh-ed25519" {
            anyhow::bail!("host public key algorithm is not ssh-ed25519");
        }
        let key_offset = 4 + algo_len;
        let key_len = u32::from_be_bytes([
            decoded[key_offset],
            decoded[key_offset + 1],
            decoded[key_offset + 2],
            decoded[key_offset + 3],
        ]) as usize;
        if key_offset + 4 + key_len > decoded.len() || key_len != 32 {
            anyhow::bail!("host public key has unexpected ed25519 key length");
        }
        Ok(decoded[key_offset + 4..key_offset + 4 + key_len].to_vec())
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
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).map_err(|e| anyhow::anyhow!("failed to create temp dir: {e}"))?;
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

/// Extract the raw 32-byte ed25519 key from the SSH wire-format blob
/// returned by [`ssh2::Session::host_key`].
///
/// Wire format is `[u32 algo_len][algo_name][u32 key_len][raw_key]`,
/// which for ed25519 is 4 + 11 + 4 + 32 = 51 bytes.
fn extract_ed25519_raw_key(key_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if key_data.len() < 19 {
        anyhow::bail!("host key data too short for ed25519 wire format");
    }
    let algo_len =
        u32::from_be_bytes([key_data[0], key_data[1], key_data[2], key_data[3]]) as usize;
    if 4 + algo_len > key_data.len() || &key_data[4..4 + algo_len] != b"ssh-ed25519" {
        anyhow::bail!("host key algorithm is not ssh-ed25519");
    }
    let key_offset = 4 + algo_len;
    if key_offset + 4 > key_data.len() {
        anyhow::bail!("host key data too short for key length field");
    }
    let key_len = u32::from_be_bytes([
        key_data[key_offset],
        key_data[key_offset + 1],
        key_data[key_offset + 2],
        key_data[key_offset + 3],
    ]) as usize;
    if key_offset + 4 + key_len > key_data.len() || key_len != 32 {
        anyhow::bail!("host key has unexpected ed25519 key length");
    }
    Ok(key_data[key_offset + 4..key_offset + 4 + key_len].to_vec())
}

/// Execute a command via SSH and return (stdout, stderr, exit_code).
///
/// `host_key_bytes` is the raw 32-byte ed25519 public key expected from
/// the server. If provided, the server's host key is verified against it
/// immediately after the handshake.
pub fn ssh_exec(
    host: &str,
    port: u16,
    private_key_path: &Path,
    command: &str,
    host_key_bytes: Option<&[u8]>,
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

    // Verify the server's host key against the expected ed25519 key.
    if let Some(expected_raw) = host_key_bytes {
        let (key_data, key_type) = session
            .host_key()
            .ok_or_else(|| anyhow::anyhow!("SSH server did not present a host key"))?;
        if !matches!(key_type, ssh2::HostKeyType::Ed25519) {
            anyhow::bail!("host key type mismatch: expected ed25519, got {key_type:?}");
        }
        // session.host_key() returns the SSH wire-format blob:
        //   [u32 algo_len][algo_name][u32 key_len][raw_key]
        // For ed25519: 4 + 11 + 4 + 32 = 51 bytes.
        // Extract just the raw key bytes for comparison.
        let key_data_raw = extract_ed25519_raw_key(key_data)?;
        if key_data_raw != expected_raw {
            anyhow::bail!(
                "host key mismatch: expected ed25519 key from our injected host keys, \
                 got different key data ({} bytes)",
                key_data.len()
            );
        }
    }

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
        let result = ssh_exec("127.0.0.1", free_port, &tmp_key, "echo hi", None);
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
        let result = ssh_exec("127.0.0.1", port, &tmp_key, "echo hi", None);
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
        let result = ssh_exec("nonexistent.invalid", 22, &tmp_key, "echo hi", None);
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

    #[test]
    fn host_keys_public_key_bytes_roundtrip() {
        let hk = HostKeys::generate().expect("generate host keys");
        let raw = hk.public_key_bytes().expect("decode public key");
        assert_eq!(raw.len(), 32, "ed25519 raw key must be 32 bytes");
        let raw2 = hk.public_key_bytes().expect("decode again");
        assert_eq!(raw, raw2);
    }

    #[test]
    fn host_keys_generated_key_is_ed25519() {
        let hk = HostKeys::generate().unwrap();
        assert!(
            hk.public_key.starts_with("ssh-ed25519 "),
            "generated host key should be ed25519"
        );
    }

    #[test]
    fn extract_ed25519_raw_key_from_wire_format() {
        let hk = HostKeys::generate().unwrap();
        let raw = hk.public_key_bytes().unwrap();
        assert_eq!(raw.len(), 32);

        // Construct the SSH wire-format blob as returned by session.host_key():
        // [u32 algo_len][algo_name][u32 key_len][raw_key]
        let algo = b"ssh-ed25519";
        let algo_len = algo.len() as u32;
        let key_len = raw.len() as u32;
        let mut wire = Vec::new();
        wire.extend_from_slice(&algo_len.to_be_bytes());
        wire.extend_from_slice(algo);
        wire.extend_from_slice(&key_len.to_be_bytes());
        wire.extend_from_slice(&raw);

        assert_eq!(wire.len(), 4 + 11 + 4 + 32);

        let extracted = extract_ed25519_raw_key(&wire).unwrap();
        assert_eq!(extracted, raw);
    }

    #[test]
    fn extract_ed25519_raw_key_rejects_wrong_algo() {
        // Wire format with "ssh-rsa" algo
        let algo = b"ssh-rsa";
        let algo_len = algo.len() as u32;
        let mut wire = Vec::new();
        wire.extend_from_slice(&algo_len.to_be_bytes());
        wire.extend_from_slice(algo);
        wire.extend_from_slice(&32u32.to_be_bytes());
        wire.extend_from_slice(&[0u8; 32]);
        let err = extract_ed25519_raw_key(&wire).unwrap_err().to_string();
        assert!(err.contains("not ssh-ed25519"));
    }

    #[test]
    fn extract_ed25519_raw_key_rejects_wrong_key_len() {
        // Wire format with key_len=64 for ed25519
        let algo = b"ssh-ed25519";
        let algo_len = algo.len() as u32;
        let mut wire = Vec::new();
        wire.extend_from_slice(&algo_len.to_be_bytes());
        wire.extend_from_slice(algo);
        wire.extend_from_slice(&64u32.to_be_bytes());
        wire.extend_from_slice(&[0u8; 64]);
        let err = extract_ed25519_raw_key(&wire).unwrap_err().to_string();
        assert!(err.contains("unexpected ed25519 key length"));
    }

    #[test]
    fn extract_ed25519_raw_key_rejects_too_short() {
        let err = extract_ed25519_raw_key(&[0u8; 5]).unwrap_err().to_string();
        assert!(err.contains("too short"));
    }
}
