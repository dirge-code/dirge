//! Root filesystem preparation for the microVM sandbox.
//!
//! Pulls OCI images via the [`oci`] module, caches base images by
//! image reference, and clones the cached rootfs for each VM session.

use std::io;
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
                prepare_local(local_ref, &staging)?;
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
