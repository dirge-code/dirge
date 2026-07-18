//! Root filesystem preparation for the microVM sandbox.
//!
//! Pulls OCI images via the [`oci`] module, caches base images by
//! image reference, and clones the cached rootfs for each VM session.

use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A prepared rootfs, cleaned up on drop.
#[derive(Debug)]
pub struct PreparedRootfs {
    path: PathBuf,
}

impl PreparedRootfs {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PreparedRootfs {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Prepare a rootfs from an OCI image or local image.
///
/// Two modes:
/// - `local://name:tag` — uses buildah to mount a locally-built image
///   and copy its rootfs. buildah must be on PATH.
/// - Any other ref — pulls via the [`oci`] module from Docker Hub.
///
/// On first use the rootfs is cached. On subsequent uses the cached
/// rootfs is cloned per session.
///
/// Cache directory structure:
/// ```text
/// <cache_dir>/<image_safe>/
///   .lock       — advisory lock file (blocks concurrent pulls)
///   .staging/   — temporary extraction target (cleaned up on success or failure)
///   base/       — cached rootfs (atomically renamed from .staging/)
///   session-<pid>/ — per-session clone of base/
/// ```
pub async fn prepare(image: &str, cache_dir: &Path) -> anyhow::Result<PreparedRootfs> {
    let image_safe = image.replace(['/', ':'], "_");
    let image_dir = cache_dir.join(&image_safe);
    let cached_base = image_dir.join("base");
    let lock_path = image_dir.join(".lock");
    let staging = image_dir.join(".staging");

    if !cached_base.exists() {
        std::fs::create_dir_all(&image_dir)?;

        // Acquire an advisory lock so only one session pulls at a time.
        // create_new fails if the lock file already exists — we retry
        // with a short sleep until the holder finishes or we time out.
        let lock_file = acquire_lock(&lock_path)?;

        // Double-check after acquiring the lock: another session may
        // have finished the pull while we were waiting.
        if !cached_base.exists() {
            // Remove any stale staging dir from a previous failed attempt.
            if staging.exists() {
                let _ = std::fs::remove_dir_all(&staging);
            }

            if let Some(local_ref) = image.strip_prefix("local://") {
                if cfg!(target_os = "macos") {
                    if let Some(variant) = local_ref.strip_prefix("dirge-microvm:") {
                        prepare_local_via_oci(variant, &staging, cache_dir).await?;
                    } else {
                        anyhow::bail!(
                            "local:// images other than dirge-microvm:* are not supported on macOS"
                        );
                    }
                } else {
                    prepare_local(local_ref, &staging)?;
                }
            } else {
                super::oci::pull(image, &staging, cache_dir).await?;
            }

            // Atomically promote the staging dir to the cached base.
            std::fs::rename(&staging, &cached_base)?;
        }

        drop(lock_file);
        let _ = std::fs::remove_file(&lock_path);
    }

    let session_dir = image_dir.join(format!("session-{}", std::process::id()));
    cp_r(&cached_base, &session_dir)?;

    Ok(PreparedRootfs { path: session_dir })
}

/// Acquire an advisory lock file, blocking (with polling) until the
/// lock is available or we time out.
fn acquire_lock(lock_path: &Path) -> anyhow::Result<std::fs::File> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(lock_path)
        {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if std::time::Instant::now() > deadline {
                    anyhow::bail!(
                        "timed out waiting for rootfs cache lock {}",
                        lock_path.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Extract a rootfs from a locally-built image via buildah.
///
/// Exports the image to an OCI archive, then extracts layer blobs
/// into the destination. All files are owned by the current user.
fn prepare_local(image_ref: &str, dest: &Path) -> anyhow::Result<()> {
    // Build the image if not already present in buildah's local storage.
    if let Some(variant) = image_ref.strip_prefix("dirge-microvm:") {
        build_guest_image(variant)?;
    }

    let tmp = std::env::temp_dir().join(format!("dirge-oci-export-{}", uuid::Uuid::new_v4()));
    let tarball = tmp.with_extension("tar");

    let push = std::process::Command::new("buildah")
        .args([
            "push",
            "--storage-driver",
            "vfs",
            image_ref,
            &format!("oci-archive:{}", tarball.display()),
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("buildah push failed — is buildah installed? ({e})"))?;

    if !push.status.success() {
        let _ = std::fs::remove_file(&tarball);
        anyhow::bail!(
            "buildah push {} failed: {}",
            image_ref,
            String::from_utf8_lossy(&push.stderr)
        );
    }

    std::fs::create_dir_all(&tmp)?;
    let untar = std::process::Command::new("tar")
        .args([
            "-xf",
            &tarball.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("tar extract OCI archive: {e}"))?;
    let _ = std::fs::remove_file(&tarball);

    if !untar.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        anyhow::bail!("failed to extract OCI archive");
    }

    // Parse OCI manifest to extract layers in correct order.
    // The manifest's `layers` array is ordered bottom-to-top.
    // read_dir order is arbitrary, and extracting base after RUN
    // would clobber files modified by the Dockerfile (e.g. /etc/passwd).
    let index_path = tmp.join("index.json");
    let index_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&index_path)
            .map_err(|e| anyhow::anyhow!("reading OCI index.json: {e}"))?,
    )
    .map_err(|e| anyhow::anyhow!("parsing OCI index.json: {e}"))?;

    let manifest_digest = index_json["manifests"][0]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no manifest digest in index.json"))?;

    let manifest_hash = manifest_digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("unexpected manifest digest format: {manifest_digest}"))?;

    let manifest_path = tmp.join("blobs").join("sha256").join(manifest_hash);
    let manifest_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .map_err(|e| anyhow::anyhow!("reading OCI manifest: {e}"))?,
    )
    .map_err(|e| anyhow::anyhow!("parsing OCI manifest: {e}"))?;

    let layers = manifest_json["layers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no layers array in OCI manifest"))?;

    std::fs::create_dir_all(dest)?;

    for layer in layers {
        let digest = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("layer missing digest"))?;
        let layer_hash = digest
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("unexpected layer digest: {digest}"))?;
        let path = tmp.join("blobs").join("sha256").join(layer_hash);

        let mut child = std::process::Command::new("gzip")
            .arg("-dc")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning gzip for layer extraction: {e}"))?;

        let gzip_stdout = child.stdout.take().unwrap();

        let tar_status = std::process::Command::new("tar")
            .args(["-x", "--no-same-owner", "--no-same-permissions", "-C"])
            .arg(dest)
            .stdin(gzip_stdout)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .map_err(|e| anyhow::anyhow!("extracting layer: {e}"))?;

        let gzip_result = child
            .wait()
            .map_err(|e| anyhow::anyhow!("waiting for gzip: {e}"))?;

        if !tar_status.success() || !gzip_result.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            anyhow::bail!("layer extraction failed for {}", path.display());
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Install Alpine packages into a rootfs by downloading and extracting .apk files.
///
/// This implements a minimal subset of `apk add --root <dest> <packages>` by:
/// 1. Fetching the Alpine APKINDEX from the appropriate mirror
/// 2. Resolving package dependencies (recursively, skipping `so:` and `cmd:` deps
///    which are assumed to be already present in the base Alpine image)
/// 3. Downloading each `.apk` file
/// 4. Extracting the payload (second gzip member) into the rootfs
async fn install_alpine_packages(dest: &Path, packages: &[&str]) -> anyhow::Result<()> {
    let alpine_version = "v3.21";
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        anyhow::bail!("unsupported architecture for Alpine package installation");
    };
    let repo_url = format!("https://dl-cdn.alpinelinux.org/alpine/{alpine_version}/main/{arch}");

    // Step 1: Download and parse APKINDEX.
    let index_url = format!("{repo_url}/APKINDEX.tar.gz");
    eprintln!("[alpine] downloading APKINDEX from {index_url} ...");
    let index_bytes = reqwest::get(&index_url).await?.bytes().await?.to_vec();
    eprintln!(
        "[alpine] downloaded {} bytes from APKINDEX",
        index_bytes.len()
    );

    // Write APKINDEX to a temp file for tar extraction to avoid a
    // bidirectional pipe deadlock (tar blocks on stdout pipe while
    // parent blocks on stdin write). Mirrors the oci.rs blob fix.
    let tmp = std::env::temp_dir().join(format!("dirge-apkindex-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &index_bytes)
        .map_err(|e| anyhow::anyhow!("writing APKINDEX temp file: {e}"))?;
    let index_output = std::process::Command::new("tar")
        .args(["-xzf", &tmp.to_string_lossy(), "-O"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("extracting APKINDEX: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if !index_output.status.success() {
        let stderr = String::from_utf8_lossy(&index_output.stderr);
        anyhow::bail!("failed to extract Alpine APKINDEX: {stderr}");
    }
    let index_text = String::from_utf8_lossy(&index_output.stdout).to_string();
    eprintln!(
        "[alpine] APKINDEX extracted ({} bytes uncompressed)",
        index_text.len()
    );
    let entries = parse_apkindex(&index_text);

    // Step 2: BFS dependency resolution (collect package names + versions).
    let mut needed: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: Vec<String> = packages.iter().map(|s| s.to_string()).collect();

    while let Some(pkg) = queue.pop() {
        if !seen.insert(pkg.clone()) {
            continue;
        }
        if let Some(entry) = entries.iter().find(|e| e.name == pkg) {
            needed.push(format!("{}-{}", entry.name, entry.version));
            for dep_name in &entry.dep_names {
                if !seen.contains(dep_name) {
                    queue.push(dep_name.clone());
                }
            }
        } else {
            anyhow::bail!("package not found in Alpine APKINDEX: {pkg}");
        }
    }

    // Step 3: Download and extract each package.
    eprintln!(
        "[alpine] resolved {} dependencies: {:?}",
        needed.len(),
        packages,
    );
    for pkg_file in &needed {
        let url = format!("{repo_url}/{pkg_file}.apk");
        eprintln!("[alpine] downloading {url} ...");
        let bytes = reqwest::get(&url).await?.bytes().await?.to_vec();
        eprintln!("[alpine] downloaded {} bytes for {pkg_file}", bytes.len());
        extract_apk_payload(&bytes, dest)?;
        eprintln!("[alpine] extracted {pkg_file}");
    }

    eprintln!("[alpine] all {n} packages installed", n = needed.len());

    Ok(())
}

/// A single entry parsed from the Alpine APKINDEX.
struct ApkEntry {
    name: String,
    version: String,
    /// Package-name dependencies (so:, cmd:, and version constraints excluded).
    dep_names: Vec<String>,
}

/// Parse the Alpine APKINDEX text format.
///
/// Each entry is separated by a blank line. Fields use the format `KEY:VALUE`.
///
/// ```text
/// P:openssh-server
/// V:9.9_p1-r2
/// D:openssh-server-common (= 9.9_p1-r2) so:libcrypto.so.3
/// ```
fn parse_apkindex(data: &str) -> Vec<ApkEntry> {
    let mut entries = Vec::new();
    for block in data.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut entry = ApkEntry {
            name: String::new(),
            version: String::new(),
            dep_names: Vec::new(),
        };
        for line in block.lines() {
            if let Some((key, rest)) = line.split_once(':') {
                let value = rest.trim_start();
                match key {
                    "P" => entry.name = value.to_string(),
                    "V" => entry.version = value.to_string(),
                    "D" => {
                        for token in value.split_whitespace() {
                            let token = token.trim();
                            if token.is_empty()
                                || token == "("
                                || token == ")"
                                || token.starts_with("so:")
                                || token.starts_with("cmd:")
                                || token.starts_with('/')
                                || token.starts_with('=')
                                || token.starts_with('<')
                                || token.starts_with('>')
                            {
                                continue;
                            }
                            // Strip ! prefix (negative/conflicts dep).
                            let token = if token.starts_with('!') {
                                &token[1..]
                            } else {
                                token
                            };
                            // Strip version operators (pkgname=version, pkgname>=version, etc.)
                            let clean = if let Some(pos) =
                                token.find(|c: char| c == '=' || c == '>' || c == '<')
                            {
                                token[..pos].trim().to_string()
                            } else {
                                token.to_string()
                            };
                            // Also handle parenthesized "(= version)" format.
                            let clean = if let Some(stripped) = clean.strip_suffix(')') {
                                if let Some(paren) = stripped.rfind('(') {
                                    let c = stripped[..paren].trim();
                                    if c.is_empty() {
                                        continue;
                                    }
                                    c.to_string()
                                } else {
                                    clean
                                }
                            } else {
                                clean
                            };
                            if !clean.is_empty() {
                                entry.dep_names.push(clean);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !entry.name.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

/// Extract the payload (last gzip member) from a `.apk` file into `dest`.
///
/// Alpine `.apk` files contain gzip members concatenated:
/// - Optional: `.SIGN.*` (signature)
/// - `control.tar.gz` — package metadata (.PKGINFO, install scripts)
/// - `payload.tar.gz` — the actual files
///
/// We extract only the last gzip member (the payload).
fn extract_apk_payload(data: &[u8], dest: &Path) -> anyhow::Result<()> {
    let magic = [0x1f, 0x8b, 0x08];
    // Find the LAST gzip member — the payload is always the last one in .apk files.
    let payload_offset = data
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == magic)
        .last()
        .map(|(i, _)| i)
        .ok_or_else(|| anyhow::anyhow!("no gzip members found in .apk file"))?;

    // Write the payload to a temp file and use `tar -xzf` on it.
    // We avoid stdin piping because tar pre-checks the file before extracting
    // and may close the pipe early if it detects an issue.
    let tmp = std::env::temp_dir().join(format!("dirge-apk-payload-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &data[payload_offset..])?;

    let output = std::process::Command::new("tar")
        .args([
            "-xzf",
            &tmp.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
        ])
        .output()?;

    let _ = std::fs::remove_file(&tmp);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Show a sample of the data for debugging.
        let sample_hex: String = data[payload_offset..payload_offset + 32]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        anyhow::bail!(
            "tar extraction of .apk payload failed: {stderr}\n\
             payload_offset={payload_offset}, data_len={}, first_bytes={sample_hex}",
            data.len()
        );
    }

    Ok(())
}

/// Build an Alpine-based microVM image using OCI pull and package installation.
///
/// On platforms where buildah is unavailable (macOS), this function
/// provides an alternative path for preparing `dirge-microvm:*` images:
///
/// 1. Pulls the base image via the pure-Rust OCI puller.
/// 2. Installs `openssh-server` and dependencies by downloading `.apk`
///    files directly from the Alpine mirror and extracting their payload
///    into the rootfs.
/// 3. Creates `/var/empty` (required by sshd privilege separation).
async fn prepare_local_via_oci(variant: &str, dest: &Path, cache_dir: &Path) -> anyhow::Result<()> {
    let base_image = match variant {
        "alpine" => "docker.io/library/alpine:3.21.3",
        "debian" => {
            anyhow::bail!(
                "Debian microVM images are not yet supported on this platform via OCI pull. \
                 Use Alpine instead."
            );
        }
        other => anyhow::bail!("unsupported dirge-microvm variant: {other}"),
    };

    // Pull the base image via pure Rust OCI puller.
    eprintln!("[alpine] OCI pulling base image {base_image} ...");
    super::oci::pull(base_image, dest, cache_dir).await?;
    eprintln!("[alpine] OCI pull complete");

    // Install openssh-server by downloading and extracting Alpine packages.
    // `apk` (Alpine Package Keeper) is not available on macOS via Homebrew,
    // so we download the .apk files directly and extract the payload tarballs
    // into the rootfs. This avoids needing apk on the host system.
    eprintln!("[alpine] installing packages via install_alpine_packages ...");
    install_alpine_packages(dest, &["openssh-server"]).await?;
    eprintln!("[alpine] package installation complete");

    // Create /var/empty (required by sshd privilege separation).
    eprintln!("[alpine] creating /var/empty ...");
    let var_empty = dest.join("var").join("empty");
    std::fs::create_dir_all(&var_empty)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&var_empty, std::fs::Permissions::from_mode(0o755))?;
    }
    // chown to root so the guest kernel sees root-owned /var/empty
    // (virtio-fs presents host file uids/gids as-is in the guest).
    let _ = std::process::Command::new("sudo")
        .args(["chown", "0:0"])
        .arg(&var_empty)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // Also hardlink libcrypto to where musl-prngs expects it
    eprintln!("[alpine] /var/empty ready");

    Ok(())
}

/// Clean up an ephemeral rootfs clone.
pub fn cleanup(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)
            .map_err(|e| anyhow::anyhow!("failed to remove rootfs clone: {e}"))?;
    }
    Ok(())
}

/// Build a dirge guest image from images/<name>/Dockerfile.
///
/// Uses `buildah bud` to produce a local image tagged `dirge-microvm:<name>`.
/// Supported names: "debian", "alpine" (any directory under `images/` with a Dockerfile).
#[cfg(target_os = "linux")]
pub fn build_guest_image(name: &str) -> anyhow::Result<()> {
    let tag = format!("dirge-microvm:{name}");

    // Resolve images/<name>/Dockerfile relative to the project root.
    let dockerfile_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("images")
        .join(name)
        .join("Dockerfile");
    if !dockerfile_path.exists() {
        anyhow::bail!(
            "no Dockerfile found for variant '{name}' — expected {}",
            dockerfile_path.display()
        );
    }

    // Check if the image already exists.
    let inspect = std::process::Command::new("buildah")
        .args(["inspect", "--type", "image", &tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if inspect.map(|s| s.success()).unwrap_or(false) {
        return Ok(());
    }

    let status = std::process::Command::new("buildah")
        .args([
            "bud",
            "--storage-driver",
            "vfs",
            "--tag",
            &tag,
            "-f",
            &dockerfile_path.to_string_lossy(),
            ".",
        ])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run buildah bud: {e}"))?;

    if !status.success() {
        anyhow::bail!("buildah bud failed");
    }

    Ok(())
}
#[cfg(target_os = "macos")]
pub fn build_guest_image(_name: &str) -> anyhow::Result<()> {
    eprintln!("  macOS: skipping buildah build (OCI pull will be used at runtime)");
    Ok(())
}

/// Canonicalize a user-supplied image reference for the microVM sandbox.
///
/// - `alpine` or `debian` (bare name, no `/` or `://`) → `local://dirge-microvm:<name>`
/// - Anything with `/` or `://` → passed through as-is (Docker Hub, local://, etc.)
pub fn canonicalize_image_ref(raw: &str) -> String {
    if raw.contains('/') || raw.contains("://") {
        raw.to_string()
    } else {
        format!("local://dirge-microvm:{raw}")
    }
}

/// If `image_ref` is a `local://dirge-microvm:<name>` reference, return the
/// variant name (`"debian"`, `"alpine"`, etc.). Otherwise `None`.
pub fn local_variant_name(image_ref: &str) -> Option<&str> {
    image_ref.strip_prefix("local://dirge-microvm:")
}

/// Copy a regular file using copy_file_range, falling back to std::fs::copy.
///
/// On filesystems that support reflinks (btrfs, xfs), copy_file_range
/// creates a CoW clone — instant and space-efficient. Falls back to a
/// full data copy when reflinks aren't available (different filesystem,
/// unsupported by the OS, etc.).
///
/// `copy_file_range` is a Linux syscall (absent from `libc` on macOS/BSD),
/// so this fast path is Linux-only; see the non-Linux fallback below.
#[cfg(target_os = "linux")]
fn copy_file_reflink(src: &Path, dst: &Path) -> io::Result<u64> {
    use std::fs::OpenOptions;

    let file_in = OpenOptions::new().read(true).open(src)?;
    let file_out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;

    let fd_in = file_in.as_raw_fd();
    let fd_out = file_out.as_raw_fd();
    let len = file_in.metadata()?.len() as i64;

    let mut offset_in: i64 = 0;
    let mut offset_out: i64 = 0;
    let mut remaining = len;
    let mut total_written: u64 = 0;

    while remaining > 0 {
        let count = remaining;
        let written = unsafe {
            libc::copy_file_range(
                fd_in,
                &mut offset_in,
                fd_out,
                &mut offset_out,
                count as usize,
                0,
            )
        };
        if written <= 0 {
            let err = io::Error::last_os_error();
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "copy_file_range returned 0",
                ));
            }
            // EXDEV (cross-device), ENOSYS (not supported), EINVAL →
            // fall back to std::fs::copy.
            let raw = err.raw_os_error().unwrap_or(0);
            if raw == libc::EXDEV || raw == libc::ENOSYS {
                return Err(err);
            }
            // EINVAL can mean the kernel doesn't support the flags,
            // or that the filesystem doesn't support reflinks.
            if raw == libc::EINVAL {
                return Err(err);
            }
            return Err(err);
        }
        remaining -= written as i64;
        total_written += written as u64;
    }

    Ok(total_written)
}

/// Non-Linux fallback: `copy_file_range` doesn't exist here. Report
/// "unsupported" (`ENOSYS`) so [`cp_r`] takes its `std::fs::copy` path.
#[cfg(not(target_os = "linux"))]
fn copy_file_reflink(_src: &Path, _dst: &Path) -> io::Result<u64> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

/// Copy a directory tree (like `cp -a`).
///
/// Tries copy_file_range for regular files (reflinks on btrfs/xfs),
/// falling back to std::fs::copy.
pub(crate) fn cp_r(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            cp_r(&src_path, &dst_path)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else {
            let metadata = entry.metadata()?;
            match copy_file_reflink(&src_path, &dst_path) {
                Ok(_) => {
                    // copy_file_range preserves data but NOT permissions.
                    // On filesystems where it succeeds (ext4, NFS, CIFS),
                    // the destination gets default permissions, breaking
                    // executables like /bin/sh inside the VM rootfs.
                    std::fs::set_permissions(&dst_path, metadata.permissions())?;
                    eprintln!("[alpine] /var/empty ready");
                }
                Err(e) => {
                    let raw = e.raw_os_error().unwrap_or(0);
                    if raw == libc::EXDEV || raw == libc::ENOSYS || raw == libc::EINVAL {
                        // std::fs::copy preserves permissions automatically.
                        std::fs::copy(&src_path, &dst_path)?;
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_bare_name() {
        assert_eq!(
            canonicalize_image_ref("alpine"),
            "local://dirge-microvm:alpine"
        );
        assert_eq!(
            canonicalize_image_ref("debian"),
            "local://dirge-microvm:debian"
        );
    }

    #[test]
    fn canonicalize_passthrough() {
        assert_eq!(
            canonicalize_image_ref("local://dirge-microvm:debian"),
            "local://dirge-microvm:debian"
        );
        assert_eq!(
            canonicalize_image_ref("docker.io/library/alpine:latest"),
            "docker.io/library/alpine:latest"
        );
    }

    #[test]
    fn canonicalize_bare_name_with_tag() {
        // Bare name with a tag produces double-colon format.
        assert_eq!(
            canonicalize_image_ref("alpine:3.21"),
            "local://dirge-microvm:alpine:3.21"
        );
        assert_eq!(
            canonicalize_image_ref("debian:bookworm"),
            "local://dirge-microvm:debian:bookworm"
        );
    }

    #[test]
    fn local_variant_extraction() {
        assert_eq!(
            local_variant_name("local://dirge-microvm:debian"),
            Some("debian")
        );
        assert_eq!(
            local_variant_name("local://dirge-microvm:alpine"),
            Some("alpine")
        );
        assert_eq!(local_variant_name("docker.io/library/alpine:latest"), None);
        assert_eq!(local_variant_name("alpine"), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn build_guest_image_invalid_name() {
        let result = build_guest_image("nonexistent-variant-xyz");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no Dockerfile found"),
            "expected 'no Dockerfile found' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn prepare_local_nonexistent_image_is_error() {
        let cache = std::env::temp_dir().join("dirge-test-prepare-local-nonexistent");
        let _ = std::fs::remove_dir_all(&cache);
        let result = prepare("local://dirge-microvm:nonexistent-image-xyz-123", &cache).await;
        let _ = std::fs::remove_dir_all(&cache);
        assert!(
            result.is_err(),
            "prepare with nonexistent local image should fail, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn prepare_docker_nonexistent_image_is_error() {
        let cache = std::env::temp_dir().join("dirge-test-prepare-docker-nonexistent");
        let _ = std::fs::remove_dir_all(&cache);
        let result = prepare(
            "docker.io/library/this-image-should-not-exist-xyz:999",
            &cache,
        )
        .await;
        let _ = std::fs::remove_dir_all(&cache);
        assert!(
            result.is_err(),
            "prepare with nonexistent docker image should fail, got: {result:?}"
        );
    }

    // The reflink fast path is Linux-only (copy_file_range); off-Linux
    // copy_file_reflink is a stub that always reports ENOSYS so cp_r falls
    // back to std::fs::copy. These two exercise the real syscall.
    #[cfg(target_os = "linux")]
    #[test]
    fn copy_file_reflink_copies_content() {
        let dir = std::env::temp_dir().join("dirge-test-reflink-content");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::write(&src, b"hello reflink world").unwrap();

        let written = copy_file_reflink(&src, &dst).unwrap();
        assert_eq!(written, 19);
        assert_eq!(
            std::fs::read_to_string(&dst).unwrap(),
            "hello reflink world"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copy_file_reflink_empty_file() {
        let dir = std::env::temp_dir().join("dirge-test-reflink-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("empty");
        let dst = dir.join("empty-dst");
        std::fs::write(&src, b"").unwrap();

        let written = copy_file_reflink(&src, &dst).unwrap();
        assert_eq!(written, 0);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_file_reflink_nonexistent_src_is_error() {
        let dir = std::env::temp_dir().join("dirge-test-reflink-no-src");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result = copy_file_reflink(&dir.join("nope"), &dir.join("dst"));
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp_r_copies_dir_tree() {
        let dir = std::env::temp_dir().join("dirge-test-cpr-tree");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(src.join("subdir")).unwrap();
        std::fs::write(src.join("a.txt"), b"file a").unwrap();
        std::fs::write(src.join("subdir").join("b.txt"), b"file b").unwrap();

        let dst = dir.join("dst");
        cp_r(&src, &dst).unwrap();

        assert!(dst.exists());
        assert!(dst.join("subdir").exists());
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).unwrap(),
            "file a"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("subdir").join("b.txt")).unwrap(),
            "file b"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp_r_copies_symlinks() {
        let dir = std::env::temp_dir().join("dirge-test-cpr-symlink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("target"), b"link target").unwrap();
        std::os::unix::fs::symlink("target", src.join("link")).unwrap();

        let dst = dir.join("dst");
        cp_r(&src, &dst).unwrap();

        let link_target = std::fs::read_link(dst.join("link")).unwrap();
        assert_eq!(link_target, Path::new("target"));
        assert_eq!(
            std::fs::read_to_string(dst.join("link")).unwrap(),
            "link target"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp_r_file_src_is_error() {
        let dir = std::env::temp_dir().join("dirge-test-cpr-file-src");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let file = dir.join("a-file");
        std::fs::write(&file, b"hello").unwrap();

        let dst = dir.join("dst");
        let result = cp_r(&file, &dst);
        assert!(
            result.is_err(),
            "cp_r with a file (not dir) as source should fail, got: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_nonexistent_is_ok() {
        let path = std::env::temp_dir().join("dirge-test-cleanup-nonexistent");
        let _ = std::fs::remove_dir_all(&path);
        // cleanup on a path that doesn't exist should be a no-op.
        assert!(
            cleanup(&path).is_ok(),
            "cleanup on nonexistent path should succeed"
        );
    }

    #[test]
    fn cleanup_removes_existing_dir() {
        let dir = std::env::temp_dir().join("dirge-test-cleanup-existing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a-file"), b"data").unwrap();
        assert!(dir.exists(), "test dir should exist before cleanup");
        cleanup(&dir).expect("cleanup should succeed on existing dir");
        assert!(!dir.exists(), "dir should be removed after cleanup");
    }

    #[test]
    fn acquire_lock_exclusive() {
        let dir = std::env::temp_dir().join(format!(
            "dirge-test-lock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join(".lock");

        let _lock1 = acquire_lock(&lock_path).expect("first lock");
        // Second lock on the same path should fail (the lock file exists).
        let result = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path);
        assert!(
            result.is_err(),
            "second lock should fail while first is held"
        );

        drop(_lock1);
        let _ = std::fs::remove_file(&lock_path);
        // Now a new lock should succeed.
        let _lock2 = acquire_lock(&lock_path).expect("lock after release");
        drop(_lock2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
