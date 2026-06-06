//! Dependency checks for sandbox backends. Used by `dirge sandbox check`
//! and `dirge sandbox setup` subcommands.

#[cfg(feature = "sandbox-microvm")]
use std::path::Path;

/// Severity of a single dependency check.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Error,
}

/// One dependency check result.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: Status,
    pub message: String,
    /// Human-readable fix hint, one-liner.
    pub fix: Option<&'static str>,
}

/// Check all dependencies for the bwrap sandbox backend.
pub fn check_bwrap() -> Vec<CheckResult> {
    let mut results = Vec::new();

    let bwrap_ok = std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    results.push(CheckResult {
        name: "bwrap",
        status: if bwrap_ok { Status::Ok } else { Status::Error },
        message: if bwrap_ok {
            "bwrap found on PATH".into()
        } else {
            "bwrap not found on PATH".into()
        },
        fix: if bwrap_ok {
            None
        } else {
            Some("Install bubblewrap: apt install bubblewrap / dnf install bubblewrap / pacman -S bubblewrap")
        },
    });

    results
}

/// Check all dependencies for the microVM sandbox backend.
#[cfg(feature = "sandbox-microvm")]
pub fn check_microvm() -> Vec<CheckResult> {
    let mut results = Vec::new();

    // /dev/kvm
    let kvm_ok = Path::new("/dev/kvm").exists();
    results.push(CheckResult {
        name: "/dev/kvm",
        status: if kvm_ok { Status::Ok } else { Status::Error },
        message: if kvm_ok {
            "/dev/kvm is accessible".into()
        } else {
            "/dev/kvm not found".into()
        },
        fix: if kvm_ok {
            None
        } else {
            Some("Enable KVM in BIOS/firmware, or load the kvm kernel module: modprobe kvm")
        },
    });

    // libkrun.so
    let libkrun_ok = check_shared_library("libkrun.so");
    results.push(CheckResult {
        name: "libkrun.so",
        status: if libkrun_ok {
            Status::Ok
        } else {
            Status::Error
        },
        message: if libkrun_ok {
            "libkrun.so found".into()
        } else {
            "libkrun.so not found".into()
        },
        fix: if libkrun_ok {
            None
        } else {
            Some("Install libkrun: see https://github.com/containers/libkrun")
        },
    });

    // libkrunfw.so
    let libkrunfw_ok = check_shared_library("libkrunfw.so");
    results.push(CheckResult {
        name: "libkrunfw.so",
        status: if libkrunfw_ok {
            Status::Ok
        } else {
            Status::Error
        },
        message: if libkrunfw_ok {
            "libkrunfw.so found".into()
        } else {
            "libkrunfw.so not found".into()
        },
        fix: if libkrunfw_ok {
            None
        } else {
            Some("Install libkrunfw: comes with libkrun")
        },
    });

    // gzip
    let gzip_ok = which_in_path("gzip");
    results.push(CheckResult {
        name: "gzip",
        status: if gzip_ok { Status::Ok } else { Status::Error },
        message: if gzip_ok {
            "gzip found on PATH".into()
        } else {
            "gzip not found on PATH (needed for OCI layer extraction)".into()
        },
        fix: if gzip_ok {
            None
        } else {
            Some("Install gzip: apt install gzip / dnf install gzip")
        },
    });

    // tar
    let tar_ok = which_in_path("tar");
    results.push(CheckResult {
        name: "tar",
        status: if tar_ok { Status::Ok } else { Status::Error },
        message: if tar_ok {
            "tar found on PATH".into()
        } else {
            "tar not found on PATH (needed for OCI layer extraction)".into()
        },
        fix: if tar_ok {
            None
        } else {
            Some("Install tar: already present on most systems")
        },
    });

    // ssh-keygen
    let ssh_keygen_ok = which_in_path("ssh-keygen");
    results.push(CheckResult {
        name: "ssh-keygen",
        status: if ssh_keygen_ok {
            Status::Ok
        } else {
            Status::Error
        },
        message: if ssh_keygen_ok {
            "ssh-keygen found on PATH".into()
        } else {
            "ssh-keygen not found on PATH (needed for ephemeral SSH keys)".into()
        },
        fix: if ssh_keygen_ok {
            None
        } else {
            Some("Install openssh-client: apt install openssh-client")
        },
    });

    // runner binary
    let runner_ok = crate::sandbox::microvm::runner::find_runner_binary().is_ok();
    results.push(CheckResult {
        name: "dirge-microvm-runner",
        status: if runner_ok { Status::Ok } else { Status::Error },
        message: if runner_ok {
            "dirge-microvm-runner binary found".into()
        } else {
            "dirge-microvm-runner binary not found".into()
        },
        fix: if runner_ok {
            None
        } else {
            Some("Build with: cargo build --release --all-features")
        },
    });

    // buildah (only if using local:// images)
    let buildah_ok = which_in_path("buildah");
    results.push(CheckResult {
        name: "buildah (optional, for local:// images)",
        status: if buildah_ok { Status::Ok } else { Status::Warn },
        message: if buildah_ok {
            "buildah found on PATH".into()
        } else {
            "buildah not found on PATH (only needed for local:// OCI images)".into()
        },
        fix: if buildah_ok {
            None
        } else {
            Some("Install buildah: apt install buildah")
        },
    });

    // mold linker (nice-to-have)
    let mold_ok = which_in_path("mold");
    results.push(CheckResult {
        name: "mold linker (optional)",
        status: if mold_ok { Status::Ok } else { Status::Warn },
        message: if mold_ok {
            "mold found on PATH".into()
        } else {
            "mold not found on PATH (builds will be slower)".into()
        },
        fix: if mold_ok {
            None
        } else {
            Some("Install mold: apt install mold / dnf install mold, then add to ~/.cargo/config.toml")
        },
    });

    results
}

#[cfg(not(feature = "sandbox-microvm"))]
pub fn check_microvm() -> Vec<CheckResult> {
    vec![CheckResult {
        name: "sandbox-microvm feature",
        status: Status::Error,
        message: "dirge was built without the sandbox-microvm feature".into(),
        fix: Some("Rebuild with: cargo build --release --features sandbox-microvm"),
    }]
}

#[cfg(feature = "sandbox-microvm")]
fn which_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(name).exists()))
        .unwrap_or(false)
}

#[cfg(feature = "sandbox-microvm")]
fn check_shared_library(name: &str) -> bool {
    let output = std::process::Command::new("ldconfig").arg("-p").output();
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains(name) {
            return true;
        }
    }
    for dir in &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/local/lib64",
    ] {
        if std::path::Path::new(dir).join(name).exists() {
            return true;
        }
    }
    false
}

/// Check whether a cached rootfs for `image_ref` is valid (contains sshd).
#[cfg(feature = "sandbox-microvm")]
pub fn check_cached_rootfs(image_ref: &str, cache_dir: &Path) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let image_safe = image_ref.replace(['/', ':'], "_");
    let base_dir = cache_dir.join(&image_safe).join("base");

    if !base_dir.exists() {
        results.push(CheckResult {
            name: "cached rootfs",
            status: Status::Warn,
            message: format!("no cached rootfs for {image_ref} — run `dirge sandbox setup`"),
            fix: Some("Run: dirge sandbox setup"),
        });
        return results;
    }

    let sshd_path = base_dir.join("usr/sbin/sshd");
    if sshd_path.exists() {
        results.push(CheckResult {
            name: "cached rootfs",
            status: Status::Ok,
            message: format!("cached rootfs for {image_ref} is valid"),
            fix: None,
        });
    } else {
        results.push(CheckResult {
            name: "cached rootfs",
            status: Status::Error,
            message: format!("cached rootfs for {image_ref} is missing sshd — cache is stale"),
            fix: Some("Re-run: dirge sandbox setup"),
        });
    }

    results
}

#[cfg(test)]
#[cfg(feature = "sandbox-microvm")]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn check_cached_rootfs_missing() {
        let tmp = std::env::temp_dir().join("dirge-check-test-missing");
        let _ = fs::remove_dir_all(&tmp);
        let results = check_cached_rootfs("local://dirge-microvm:debian", &tmp);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Warn);
        assert!(results[0].message.contains("no cached rootfs"));
    }

    #[test]
    fn check_cached_rootfs_valid() {
        let tmp = std::env::temp_dir().join("dirge-check-test-valid");
        let _ = fs::remove_dir_all(&tmp);
        let base = tmp
            .join("local___dirge-microvm_debian")
            .join("base")
            .join("usr")
            .join("sbin");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("sshd"), b"fake sshd").unwrap();

        let results = check_cached_rootfs("local://dirge-microvm:debian", &tmp);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Ok);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn check_cached_rootfs_stale() {
        let tmp = std::env::temp_dir().join("dirge-check-test-stale");
        let _ = fs::remove_dir_all(&tmp);
        let base = tmp.join("local___dirge-microvm_debian").join("base");
        fs::create_dir_all(&base).unwrap();
        // No usr/sbin/sshd — simulates a stale cache

        let results = check_cached_rootfs("local://dirge-microvm:debian", &tmp);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Error);
        assert!(results[0].message.contains("missing sshd"));
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── check_bwrap ─────────────────────────────────────────────

    #[test]
    fn check_bwrap_has_name_and_status() {
        let results = check_bwrap();
        assert!(
            !results.is_empty(),
            "check_bwrap should return at least one result"
        );
        let bwrap = &results[0];
        assert_eq!(bwrap.name, "bwrap");
        // Status can be Ok or Error depending on whether bwrap is in PATH;
        // either is valid — we just verify the structure.
        assert!(
            bwrap.message.contains("bwrap"),
            "message should mention bwrap"
        );
    }

    // ── check_microvm ───────────────────────────────────────────

    #[test]
    fn check_microvm_includes_kvm_check() {
        let results = check_microvm();
        assert!(
            results.len() >= 6,
            "check_microvm should return at least 6 results, got {}",
            results.len()
        );
        // First entry should be /dev/kvm check
        assert_eq!(results[0].name, "/dev/kvm");
        // One of the last entries should be in the runner binary check
        let names: Vec<_> = results.iter().map(|r| r.name).collect();
        assert!(
            names.contains(&"dirge-microvm-runner"),
            "should include runner check, got: {names:?}"
        );
        assert!(
            names.contains(&"libkrun.so"),
            "should include libkrun.so check, got: {names:?}"
        );
        assert!(
            names.contains(&"libkrunfw.so"),
            "should include libkrunfw.so check, got: {names:?}"
        );
    }
}
