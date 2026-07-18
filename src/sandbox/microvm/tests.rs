//! Integration tests for the microVM sandbox module.
//!
//! These tests require hardware virtualization support:
//! - Linux: `/dev/kvm` access and `libkrun.so`/`libkrunfw.so`
//! - macOS: Hypervisor.framework support and `libkrun.dylib`/`libkrunfw.dylib`
//! They are gated behind the `sandbox-microvm` feature and skip gracefully
//! when prerequisites are missing.

#[cfg(test)]
#[cfg(feature = "sandbox-microvm")]
mod tests {
    use super::super::*;
    use crate::sandbox::{Sandbox, SandboxMode};

    /// Serialize VM-booting tests — only one microVM can run at a time
    /// on the host. Parallel VM boots cause SSH handshake timeouts and
    /// spurious "Failed getting banner" failures in CI.
    static VM_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_vm_test() -> std::sync::MutexGuard<'static, ()> {
        VM_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Check whether we can actually boot a VM.
    fn vm_available() -> bool {
        let virtualization_ok = if cfg!(target_os = "macos") {
            // Check Hypervisor.framework availability via sysctl.
            std::process::Command::new("sysctl")
                .args(["-n", "kern.hv_support"])
                .output()
                .ok()
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    s.trim().parse::<u8>().ok()
                })
                == Some(1)
        } else {
            std::path::Path::new("/dev/kvm").exists()
        };
        virtualization_ok && crate::sandbox::microvm::runner::find_runner_binary().is_ok()
    }

    #[test]
    fn microvm_config_defaults() {
        let cfg = MicrovmConfig::default();
        assert_eq!(cfg.cpus, 1);
        assert_eq!(cfg.memory_mib, 512);
        if cfg!(target_os = "macos") {
            assert!(
                cfg.image.contains("alpine"),
                "expected Alpine on macOS, got: {}",
                cfg.image
            );
        } else {
            assert!(
                cfg.image.contains("debian"),
                "expected Debian on Linux, got: {}",
                cfg.image
            );
        }
    }

    #[test]
    fn microvm_config_paths() {
        let cfg = MicrovmConfig::default();
        let snap_dir = cfg.snapshots_dir();
        let base_path = cfg.cached_base_path();
        // snapshots_dir is under cache_dir/snapshots.
        assert!(snap_dir.ends_with("snapshots"));
        assert!(snap_dir.starts_with(&cfg.cache_dir));
        // cached_base_path is under cache_dir/<safe_image>/base.
        assert!(base_path.ends_with("base"));
        assert!(base_path.starts_with(&cfg.cache_dir));
        if cfg!(target_os = "macos") {
            assert!(
                base_path.to_string_lossy().contains("dirge-microvm_alpine"),
                "expected alpine path on macOS, got: {:?}",
                base_path
            );
        } else {
            assert!(
                base_path.to_string_lossy().contains("dirge-microvm_debian"),
                "expected debian path on Linux, got: {:?}",
                base_path
            );
        }
    }

    #[test]
    fn microvm_sandbox_new_does_not_start() {
        let cfg = MicrovmConfig::default();
        let sandbox = MicrovmSandbox::new(cfg);
        assert_eq!(sandbox.ssh_port(), 0);
    }

    #[test]
    fn exec_fails_if_not_started() {
        let cfg = MicrovmConfig::default();
        let sandbox = MicrovmSandbox::new(cfg);
        let result = sandbox.exec("echo hi", &[], ".");
        assert!(
            result.is_err(),
            "exec before start should fail, got: {result:?}"
        );
    }

    /// Passing environment variables is a no-op (ignored), but must not panic.
    #[test]
    fn exec_ignores_env_vars() {
        let cfg = MicrovmConfig::default();
        let sandbox = MicrovmSandbox::new(cfg);
        // Without env
        let err = sandbox.exec("echo hi", &[], ".").unwrap_err();
        let msg = err.to_string();
        // With env — same error, just verifying the _env param doesn't
        // cause a panic or change behavior.
        let err2 = sandbox
            .exec("echo hi", &[("FOO", "bar"), ("BAZ", "qux")], ".")
            .unwrap_err();
        assert_eq!(
            err2.to_string(),
            msg,
            "env vars should not affect exec error for unstarted VM"
        );
    }

    #[test]
    fn ssh_keys_generate_and_cleanup() {
        use crate::sandbox::microvm::ssh::EphemeralKeys;
        let keys = EphemeralKeys::generate().expect("ssh key generation failed");
        assert!(keys.public_key.starts_with("ssh-ed25519"));
        assert!(keys.private_key_path.exists());
        let key_dir = keys.private_key_path.parent().unwrap().to_path_buf();
        assert!(key_dir.exists());
        drop(keys);
        assert!(!key_dir.exists(), "temp dir should be cleaned up on drop");
    }

    #[test]
    fn ssh_wait_for_timeout() {
        use crate::sandbox::microvm::ssh::wait_for_ssh;
        use std::time::Duration;
        let result = wait_for_ssh("127.0.0.1", 19999, Duration::from_millis(200));
        assert!(result.is_err());
    }

    #[test]
    fn krun_config_has_required_mounts() {
        // Mirrors the krun_config built in MicrovmSandbox::start().
        // If you change the production config, update this test too.
        let config = serde_json::json!({
            "Cmd": [
                "/bin/sh", "-c",
                "mount -t tmpfs tmpfs /run \
                 && mkdir -p /run/sshd \
                 && mkdir -p /workspace \
                 && mount -t virtiofs workspace /workspace \
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

        let cmd = config["Cmd"][2].as_str().unwrap();

        // Host keys are injected from the host (ed25519 only).
        // Only the injected keys are present — no ssh-keygen -A.
        assert!(
            cmd.contains("mount -t tmpfs tmpfs /run"),
            "init command must mount tmpfs on /run"
        );
        assert!(
            cmd.contains("mkdir -p /run/sshd"),
            "init command must create /run/sshd"
        );
        assert!(
            cmd.contains("mount -t virtiofs workspace /workspace"),
            "init command must mount workspace virtiofs"
        );
        assert!(
            !cmd.contains("ssh-keygen"),
            "init command must NOT run ssh-keygen; host keys are injected from host"
        );
        assert!(cmd.contains("sshd -D -e"), "init command must start sshd");

        let mounts = config["mounts"].as_array().unwrap();
        let destinations: Vec<&str> = mounts
            .iter()
            .map(|m| m["destination"].as_str().unwrap())
            .collect();
        assert!(
            destinations.contains(&"/var/empty"),
            "missing /var/empty tmpfs mount"
        );
        assert!(
            destinations.contains(&"/workspace"),
            "missing /workspace virtiofs mount"
        );
    }

    #[test]
    fn host_keys_generate_and_inject() {
        use crate::sandbox::microvm::ssh::HostKeys;
        let tmp = std::env::temp_dir().join(format!(
            "dirge-test-host-keys-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let host_keys = HostKeys::generate().expect("host key generation failed");
        host_keys.inject(&tmp).expect("host key injection failed");

        let key_path = tmp.join("etc").join("ssh").join("ssh_host_ed25519_key");
        assert!(key_path.exists(), "host key not written to rootfs");
        assert!(
            key_path.metadata().unwrap().len() > 0,
            "host key file is empty"
        );

        // HostKeys::drop cleans up the temp dir.
        drop(host_keys);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn oci_pull_nonexistent_image_is_error() {
        // Pulling an image that doesn't exist should fail.
        let cache = std::env::temp_dir().join("dirge-test-oci-nonexistent");
        let dest = std::env::temp_dir().join("dirge-test-oci-nonexistent-dest");
        let _ = std::fs::remove_dir_all(&cache);
        let _ = std::fs::remove_dir_all(&dest);
        let result = crate::sandbox::microvm::oci::pull(
            "docker.io/library/this-image-should-not-exist-xyz:999",
            &dest,
            &cache,
        )
        .await;
        let _ = std::fs::remove_dir_all(&cache);
        let _ = std::fs::remove_dir_all(&dest);
        assert!(
            result.is_err(),
            "pulling nonexistent image should fail, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn full_microvm_lifecycle() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        // This test boots a real microVM. Requires:
        // 1. /dev/kvm accessible
        // 2. libkrun.so + libkrunfw.so installed
        // 3. Network access to pull the OCI image

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-microvm-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        eprintln!(
            "[full_microvm_lifecycle] image={} cache={:?}",
            cfg.image, cfg.cache_dir
        );

        let mut sandbox = MicrovmSandbox::new(cfg);

        eprintln!("[full_microvm_lifecycle] starting VM ...");
        match tokio::time::timeout(std::time::Duration::from_secs(120), sandbox.start()).await {
            Ok(Ok(())) => eprintln!("[full_microvm_lifecycle] VM started OK"),
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e:#}");
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start timed out after 120s — see [alpine]/[oci] trace above");
            }
        }

        eprintln!("[full_microvm_lifecycle] exec 'echo hello' ...");
        let result = sandbox.exec("echo hello", &[], "/");
        match result {
            Ok((stdout, stderr, code)) => {
                eprintln!(
                    "[full_microvm_lifecycle] exec OK: code={code} stdout={stdout:?} stderr={stderr:?}"
                );
                assert_eq!(code, 0, "expected exit 0, got {code} — stderr: {stderr}");
                assert!(
                    stdout.contains("hello"),
                    "expected 'hello' in stdout, got: {stdout}"
                );
            }
            Err(e) => {
                panic!("exec failed: {e:#}");
            }
        }

        // Verify fd limit is raised by krun_set_rlimits in the runner.
        let (ulimit_out, ulimit_err, ulimit_code) = sandbox
            .exec("ulimit -n", &[], "/")
            .expect("ulimit exec failed");
        assert_eq!(
            ulimit_code, 0,
            "ulimit -n should succeed — stderr: {ulimit_err}"
        );
        let nofile: u32 = ulimit_out
            .trim()
            .parse()
            .expect("ulimit output should be a number");
        // On Linux, krun_set_rlimits raises the guest fd limit above 1024.
        // On macOS, libkrun doesn't support krun_set_rlimits (RLIM_INFINITY
        // overflows the kernel cmdline), so the guest stays at the kernel default.
        if cfg!(target_os = "linux") {
            assert!(
                nofile > 1024,
                "fd limit should be raised above 1024 by krun_set_rlimits, got {nofile}"
            );
        } else {
            eprintln!("[ulimit] macOS: guest nofile={nofile} (krun_set_rlimits not supported)");
        }

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[tokio::test]
    async fn full_microvm_lifecycle_alpine() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-microvm-alpine-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            image: "local://dirge-microvm:alpine".to_string(),
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        eprintln!("[alpine] image={} cache={:?}", cfg.image, cfg.cache_dir);

        let mut sandbox = MicrovmSandbox::new(cfg);

        eprintln!("[alpine] starting VM ...");
        match tokio::time::timeout(std::time::Duration::from_secs(120), sandbox.start()).await {
            Ok(Ok(())) => eprintln!("[alpine] VM started OK"),
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e:#}");
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start timed out after 120s — see [alpine]/[oci] trace above");
            }
        }

        eprintln!("[alpine] exec 'uname -a && id' ...");
        let result = sandbox.exec("uname -a && id", &[], "/");
        match result {
            Ok((stdout, stderr, code)) => {
                eprintln!("[alpine] exec OK: code={code} stdout={stdout:?} stderr={stderr:?}");
                assert_eq!(code, 0, "expected exit 0, got {code} — stderr: {stderr}");
                assert!(
                    stdout.contains("Linux"),
                    "expected 'Linux' in uname output, got: {stdout}"
                );
                assert!(
                    stdout.contains("sandbox"),
                    "expected 'sandbox' user, got: {stdout}"
                );
            }
            Err(e) => {
                panic!("exec failed: {e:#}");
            }
        }

        // Verify fd limit is raised by krun_set_rlimits in the runner.
        let (ulimit_out, ulimit_err, ulimit_code) = sandbox
            .exec("ulimit -n", &[], "/")
            .expect("ulimit exec failed");
        assert_eq!(
            ulimit_code, 0,
            "ulimit -n should succeed — stderr: {ulimit_err}"
        );
        let nofile: u32 = ulimit_out
            .trim()
            .parse()
            .expect("ulimit output should be a number");
        // On Linux, krun_set_rlimits raises the guest fd limit above 1024.
        // On macOS, libkrun doesn't support krun_set_rlimits (RLIM_INFINITY
        // overflows the kernel cmdline), so the guest stays at the kernel default.
        if cfg!(target_os = "linux") {
            assert!(
                nofile > 1024,
                "fd limit should be raised above 1024 by krun_set_rlimits, got {nofile}"
            );
        } else {
            eprintln!("[ulimit] macOS: guest nofile={nofile} (krun_set_rlimits not supported)");
        }

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Exercise exec edge cases in a single VM boot: non-zero exit codes,
    /// stderr capture, special characters, and cwd parameter.
    #[tokio::test]
    async fn exec_edge_cases() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-edge-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // ── non-zero exit code ─────────────────────────────────
        let (stdout, stderr, code) = sandbox.exec("exit 42", &[], "/").expect("exec exit 42");
        assert_eq!(
            code, 42,
            "exit code should be 42 — stdout: {stdout} stderr: {stderr}"
        );

        // ── stderr capture ─────────────────────────────────────
        let (stdout, stderr, code) = sandbox
            .exec("echo to-stdout; echo to-stderr >&2", &[], "/")
            .expect("exec stderr test");
        assert_eq!(code, 0, "exit should be 0 — stderr: {stderr}");
        assert!(
            stdout.contains("to-stdout"),
            "stdout should contain to-stdout: {stdout}"
        );
        assert!(
            !stdout.contains("to-stderr"),
            "stdout should NOT contain to-stderr: {stdout}"
        );
        assert!(
            stderr.contains("to-stderr"),
            "stderr should contain to-stderr: {stderr}"
        );

        // ── special characters ─────────────────────────────────
        let (stdout, _stderr, code) = sandbox
            .exec(r#"echo 'quotes " double' "'single" '$dollar'"#, &[], "/")
            .expect("exec special chars");
        assert_eq!(code, 0);
        assert!(
            stdout.contains(r#"quotes " double"#),
            "double quotes should pass through: {stdout}"
        );
        assert!(
            stdout.contains("'single"),
            "single quotes should pass through: {stdout}"
        );
        assert!(
            stdout.contains("$dollar"),
            "literal dollar should pass through: {stdout}"
        );

        // ── unicode ────────────────────────────────────────────
        let (stdout, _stderr, code) = sandbox
            .exec("echo 'héllo wörld 世界'", &[], "/")
            .expect("exec unicode");
        assert_eq!(code, 0);
        assert!(
            stdout.contains("héllo wörld 世界"),
            "unicode should round-trip: {stdout}"
        );

        // ── cwd parameter ──────────────────────────────────────
        let (stdout, _stderr, code) = sandbox.exec("pwd", &[], "/tmp").expect("exec pwd in /tmp");
        assert_eq!(code, 0);
        assert!(
            stdout.trim() == "/tmp" || stdout.trim().ends_with("/tmp"),
            "pwd in /tmp should output /tmp, got: {stdout}"
        );

        // ── cwd to a non-existent dir returns non-zero exit ──────
        let (stdout, _stderr, code) = sandbox
            .exec("pwd", &[], "/nonexistent_dir_xyz")
            .expect("exec should not fail at SSH level");
        assert_ne!(
            code, 0,
            "cd to nonexistent dir should fail, got code 0 stdout={stdout}"
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Verify large output (> 64KB) is captured correctly via SSH channels.
    #[tokio::test]
    async fn exec_large_output() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-large-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Generate ~128KB of output using dd + base64.
        let (stdout, _stderr, code) = sandbox
            .exec(
                "dd if=/dev/urandom bs=1024 count=128 2>/dev/null | base64 -w0",
                &[],
                "/",
            )
            .expect("exec large output");
        assert_eq!(code, 0, "large output command should succeed");

        // 128KB random → base64 ~= 170KB.
        assert!(
            stdout.len() > 100_000,
            "large output should be >100KB, got {} bytes",
            stdout.len()
        );

        // Verify output is valid base64 (no truncation artifacts).
        let trimmed = stdout.trim();
        assert!(
            trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "output should be valid base64, got unexpected chars"
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Rapid sequential exec calls to verify SSH session reuse reliability.
    #[tokio::test]
    async fn many_sequential_execs() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-sequential-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // 50 rapid exec calls — each opens a new SSH channel. Verify
        // all succeed and return the expected output.
        for i in 0..50 {
            let (stdout, _stderr, code) = sandbox
                .exec(&format!("echo iter{}", i), &[], "/")
                .expect("sequential exec should not fail");
            assert_eq!(code, 0, "iter {i} should exit 0");
            assert!(
                stdout.contains(&format!("iter{i}")),
                "iter {i} output mismatch: {stdout}"
            );
        }

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Verify that files written inside the VM at /workspace/ appear on the
    /// host, and files written on the host appear inside the VM. This is the
    /// core virtio-fs path — every tool call depends on it.
    #[tokio::test]
    async fn workspace_file_round_trip() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        if cfg!(target_os = "macos") {
            eprintln!("skipping: virtio-fs workspace sharing unavailable on macOS libkrun 1.19.4");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-workspace-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let workspace = std::env::temp_dir().join(format!(
            "dirge-test-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            workspace: workspace.clone(),
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                let _ = std::fs::remove_dir_all(&workspace);
                panic!("VM start failed: {e}");
            }
        }

        // ── VM → Host: write file inside VM ──────────────────────
        let (stdout, stderr, code) = sandbox
            .exec(
                "echo 'hello-from-vm' > /workspace/vm-to-host.txt && cat /workspace/vm-to-host.txt",
                &[],
                "/",
            )
            .expect("write file in VM");
        assert_eq!(code, 0, "write vm-to-host failed — stderr: {stderr}");
        assert!(
            stdout.contains("hello-from-vm"),
            "VM should see its own file: {stdout}"
        );

        // Verify host can see it.
        let host_file = workspace.join("vm-to-host.txt");
        assert!(
            host_file.exists(),
            "host should see file written by VM at {}",
            host_file.display()
        );
        let content = std::fs::read_to_string(&host_file).expect("read host-side file");
        assert!(
            content.contains("hello-from-vm"),
            "host content mismatch: {content}"
        );

        // ── Host → VM: write file on host, read inside VM ────────
        std::fs::write(workspace.join("host-to-vm.txt"), "hello-from-host\n").unwrap();
        let (stdout, stderr, code) = sandbox
            .exec("cat /workspace/host-to-vm.txt", &[], "/")
            .expect("read host file in VM");
        assert_eq!(code, 0, "read host-to-vm failed — stderr: {stderr}");
        assert!(
            stdout.contains("hello-from-host"),
            "VM should see host-written file: {stdout}"
        );

        // ── binary data round-trip ───────────────────────────────
        // Write 4KB of binary data to catch any newline/encoding issues.
        let binary_data: Vec<u8> = (0..255u8).cycle().take(4096).collect();
        std::fs::write(workspace.join("binary.bin"), &binary_data).unwrap();

        let (stdout, stderr, code) = sandbox
            .exec("wc -c < /workspace/binary.bin", &[], "/")
            .expect("count binary file");
        assert_eq!(code, 0, "binary file wc failed — stderr: {stderr}");
        let size: usize = stdout.trim().parse().expect("wc output should be a number");
        assert_eq!(size, 4096, "binary file size should be 4096, got {size}");

        // Verify the binary content matches via sha256sum.
        let (host_hash, _, _) = {
            use std::process::Command;
            let output = Command::new("sha256sum")
                .arg(workspace.join("binary.bin"))
                .output()
                .expect("sha256sum host");
            (
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                String::new(),
                0i32,
            )
        };
        let (vm_hash, stderr, code) = sandbox
            .exec("sha256sum /workspace/binary.bin", &[], "/")
            .expect("sha256sum VM");
        assert_eq!(code, 0, "sha256sum in VM failed — stderr: {stderr}");
        let vm_hash = vm_hash.split_whitespace().next().unwrap_or("");
        assert_eq!(
            host_hash, vm_hash,
            "binary hash mismatch: host={host_hash} vm={vm_hash}"
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// Full snapshot lifecycle: save, list, restore, delete.
    #[tokio::test]
    async fn snapshot_save_list_restore_delete() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-snapshot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Create a marker file inside the VM.
        let (stdout, _stderr, code) = sandbox
            .exec(
                "echo 'snapshot-marker-content' > /tmp/marker && cat /tmp/marker",
                &[],
                "/",
            )
            .expect("create marker");
        assert_eq!(code, 0);
        assert!(stdout.contains("snapshot-marker-content"));

        // Save snapshot.
        sandbox.save_snapshot("test-snap").expect("save snapshot");

        // List snapshots — should include "test-snap".
        let snaps = sandbox.list_snapshots().expect("list snapshots");
        assert!(
            snaps.contains(&"test-snap".to_string()),
            "snapshots: {snaps:?}"
        );

        // Stop the VM.
        sandbox.stop().expect("stop VM");

        // Delete the marker in cached base (simulates VM changes being lost).
        // Restore snapshot to bring the marker back.
        sandbox
            .restore_snapshot("test-snap")
            .expect("restore snapshot");

        // Restart the VM.
        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM restart after restore failed: {e}");
            }
        }

        // Verify marker file exists after restore.
        let (stdout, _stderr, code) = sandbox
            .exec("cat /tmp/marker", &[], "/")
            .expect("check marker after restore");
        assert_eq!(code, 0);
        assert!(
            stdout.contains("snapshot-marker-content"),
            "marker should be restored, got: {stdout}"
        );

        // Delete snapshot.
        sandbox.stop().expect("stop VM before delete");
        sandbox
            .delete_snapshot("test-snap")
            .expect("delete snapshot");

        // List should be empty.
        let snaps = sandbox.list_snapshots().expect("list after delete");
        assert!(
            !snaps.contains(&"test-snap".to_string()),
            "snapshot not deleted"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Verify reboot stops and starts the VM, and in-VM changes are lost.
    #[tokio::test]
    async fn reboot_discards_state() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-reboot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Sanity-check: VM is alive.
        let (stdout, _stderr, code) = sandbox
            .exec("echo alive", &[], "/")
            .expect("pre-reboot exec");
        assert_eq!(code, 0);
        assert!(stdout.contains("alive"));

        // Create state that should be lost after reboot.
        sandbox
            .exec("echo 'before-reboot' > /tmp/state-file", &[], "/")
            .expect("create state file");

        // Reboot.
        sandbox.reboot().await.expect("reboot");

        // VM should be reachable after reboot.
        let (stdout, _stderr, code) = sandbox
            .exec("echo after-reboot", &[], "/")
            .expect("post-reboot exec");
        assert_eq!(code, 0);
        assert!(stdout.contains("after-reboot"));

        // State file should be gone (reboot re-clones from cached base).
        let (stdout, _stderr, _code) = sandbox
            .exec(
                "test -f /tmp/state-file && echo EXISTS || echo GONE",
                &[],
                "/",
            )
            .expect("check state file");
        assert!(
            stdout.contains("GONE"),
            "state file should be gone after reboot, got: {stdout}"
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Snapshot save fails if VM not started.
    #[test]
    fn snapshot_save_requires_started_vm() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        let result = sandbox.save_snapshot("test");
        assert!(result.is_err(), "save_snapshot before start should fail");
    }

    /// Empty snapshot name is rejected.
    #[test]
    fn save_snapshot_rejects_empty_name() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        let result = sandbox.save_snapshot("");
        assert!(result.is_err(), "save_snapshot with empty name should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid snapshot name"),
            "expected 'invalid snapshot name', got: {err}"
        );
    }

    /// Save snapshot fails when a snapshot with the same name already exists.
    #[test]
    fn save_snapshot_name_already_exists_is_error() {
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-snapshot-exists-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let rootfs = cache.join("fake-rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join("some-file"), b"hello").unwrap();

        let cfg = MicrovmConfig {
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };
        let snap_dir = cfg.snapshots_dir().join("my-snap");
        std::fs::create_dir_all(&snap_dir).unwrap();

        let mut sandbox = MicrovmSandbox::new(cfg);
        sandbox.rootfs_path = Some(rootfs);

        let result = sandbox.save_snapshot("my-snap");
        assert!(
            result.is_err(),
            "save_snapshot with existing name should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("already exists"),
            "expected 'already exists' in error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Path traversal in snapshot name is rejected.
    #[test]
    fn save_snapshot_rejects_path_traversal() {
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-snap-traversal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let rootfs = cache.join("fake-rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let cfg = MicrovmConfig {
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };
        let mut sandbox = MicrovmSandbox::new(cfg);
        sandbox.rootfs_path = Some(rootfs);

        for bad_name in &["../evil", "a/b", "..", "foo/../bar"] {
            let result = sandbox.save_snapshot(bad_name);
            assert!(
                result.is_err(),
                "save_snapshot with '{bad_name}' should be rejected"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected 'invalid snapshot name' for '{bad_name}', got: {err}"
            );
        }

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Snapshot delete on nonexistent name returns error.
    #[test]
    fn snapshot_delete_nonexistent_is_error() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        let result = sandbox.delete_snapshot("nonexistent-snap");
        assert!(result.is_err(), "delete nonexistent snapshot should fail");
    }

    /// Path traversal in snapshot name is rejected for delete.
    #[test]
    fn delete_snapshot_rejects_path_traversal() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        for bad_name in &["../evil", "a/b", "..", "foo/../bar"] {
            let result = sandbox.delete_snapshot(bad_name);
            assert!(
                result.is_err(),
                "delete_snapshot with '{bad_name}' should be rejected"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected 'invalid snapshot name' for '{bad_name}', got: {err}"
            );
        }
    }

    // ── validate_snapshot_name allowlist ─────────────────────────

    #[test]
    fn snapshot_name_allowlist_accepts_valid() {
        for name in &["snap", "my-snap", "snap_1", "v1.0", "a.b-c_d", "foo"] {
            MicrovmSandbox::validate_snapshot_name(name)
                .unwrap_or_else(|e| panic!("'{name}' should be valid: {e}"));
        }
    }

    #[test]
    fn snapshot_name_allowlist_rejects_control_chars() {
        for name in &["a\nb", "tab\tx", "\x00", "\x1b"] {
            let err = MicrovmSandbox::validate_snapshot_name(name)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected rejection for control chars in '{name}': {err}"
            );
        }
    }

    #[test]
    fn snapshot_name_allowlist_rejects_spaces() {
        for name in &["a b", " leading", "trailing ", "mid dle"] {
            let err = MicrovmSandbox::validate_snapshot_name(name)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected rejection for spaces in '{name}': {err}"
            );
        }
    }

    #[test]
    fn snapshot_name_allowlist_rejects_special_chars() {
        for name in &["a@b", "x!y", "p#q", "a$b", "%x", "a^b", "&x", "a*b", "x(y)"] {
            let err = MicrovmSandbox::validate_snapshot_name(name)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected rejection for special chars in '{name}': {err}"
            );
        }
    }

    #[test]
    fn snapshot_name_allowlist_rejects_empty() {
        let err = MicrovmSandbox::validate_snapshot_name("")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid snapshot name"));
    }

    #[test]
    fn snapshot_name_allowlist_rejects_dot_and_dotdot() {
        for name in &[".", ".."] {
            let err = MicrovmSandbox::validate_snapshot_name(name)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected rejection for '{name}': {err}"
            );
        }
    }

    /// delete_snapshot fails when the named entry is a file, not a directory.
    #[test]
    fn delete_snapshot_file_not_dir_is_error() {
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-delete-file-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };
        let snap_dir = cfg.snapshots_dir().join("not-a-dir");
        std::fs::create_dir_all(snap_dir.parent().unwrap()).unwrap();
        std::fs::write(&snap_dir, b"i am a file").unwrap();

        let sandbox = MicrovmSandbox::new(cfg);
        let result = sandbox.delete_snapshot("not-a-dir");
        assert!(
            result.is_err(),
            "delete_snapshot on a file (not dir) should fail"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// list_snapshots returns an empty vec when the snapshots directory
    /// does not exist yet.
    #[test]
    fn list_snapshots_empty_when_no_dir() {
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-list-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };
        let sandbox = MicrovmSandbox::new(cfg);

        let snaps = sandbox.list_snapshots().expect("list_snapshots");
        assert!(
            snaps.is_empty(),
            "expected empty list when snapshots dir doesn't exist, got: {snaps:?}"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// list_snapshots returns entries sorted alphabetically.
    #[test]
    fn list_snapshots_returns_sorted_entries() {
        let cache = std::env::temp_dir().join(format!(
            "dirge-test-list-sorted-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };
        let snap_dir = cfg.snapshots_dir();
        std::fs::create_dir_all(&snap_dir).unwrap();

        // Create directories in non-alphabetical order.
        for name in &["z-snap", "a-snap", "m-snap"] {
            std::fs::create_dir(snap_dir.join(name)).unwrap();
        }

        let sandbox = MicrovmSandbox::new(cfg);
        let snaps = sandbox.list_snapshots().expect("list_snapshots");
        assert_eq!(
            snaps,
            vec![
                "a-snap".to_string(),
                "m-snap".to_string(),
                "z-snap".to_string()
            ],
            "snapshots should be sorted alphabetically"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Restore snapshot fails if VM is still running.
    #[tokio::test]
    async fn snapshot_restore_requires_stopped_vm() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-restore-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Save a snapshot so it exists.
        sandbox
            .save_snapshot("restore-test")
            .expect("save for restore test");

        // Attempt restore while running — should fail.
        let result = sandbox.restore_snapshot("restore-test");
        assert!(
            result.is_err(),
            "restore while VM running should fail, got: {result:?}"
        );

        sandbox.stop().ok();
        let _ = sandbox.delete_snapshot("restore-test");
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Restore snapshot with a name that doesn't exist returns error.
    /// Does not need a VM — the restore_snapshot code checks ssh_port
    /// first, then verifies the snapshot exists.
    #[test]
    fn restore_snapshot_nonexistent_is_error() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        let result = sandbox.restore_snapshot("nonexistent-snap-name");
        assert!(
            result.is_err(),
            "restore nonexistent snapshot should fail, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("does not exist"),
            "error should mention snapshot doesn't exist, got: {msg}"
        );
    }

    /// Path traversal in snapshot name is rejected for restore.
    #[test]
    fn restore_snapshot_rejects_path_traversal() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        for bad_name in &["../evil", "a/b", "..", "foo/../bar"] {
            let result = sandbox.restore_snapshot(bad_name);
            assert!(
                result.is_err(),
                "restore_snapshot with '{bad_name}' should be rejected"
            );
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("invalid snapshot name"),
                "expected 'invalid snapshot name' for '{bad_name}', got: {err}"
            );
        }
    }

    /// SSH connect info returns None before VM start.
    #[test]
    fn ssh_connect_info_none_before_start() {
        let sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        // ssh_connect_info is on Sandbox (the wrapper), not MicrovmSandbox.
        // MicrovmSandbox::ssh_port returns 0 before start.
        assert_eq!(sandbox.ssh_port(), 0);
        assert!(sandbox.keys.is_none());
    }

    /// stop() is idempotent — calling it twice doesn't panic.
    #[test]
    fn stop_is_idempotent() {
        let mut sandbox = MicrovmSandbox::new(MicrovmConfig::default());
        sandbox.stop().ok();
        sandbox.stop().ok(); // second stop must not panic
    }

    /// stop() works even when there's no child process (never started).
    #[test]
    fn stop_handles_missing_child() {
        let mut sandbox = MicrovmSandbox::new(MicrovmConfig {
            ssh_port: 22, // fake port, no child
            ..MicrovmConfig::default()
        });
        sandbox.keys = None;
        sandbox.child = None;
        sandbox.rootfs_path = None;
        // Should not panic or hang.
        sandbox.stop().ok();
    }

    /// Load-test: boots the microVM then pumps synthetic keystrokes
    /// through the editor+renderer hot path, measuring wall-clock
    /// latency per iteration. Catches app-layer regressions (the
    /// CRS-GAP stutter is OS-level and not exercisable here, but the
    /// diagnostic-log probes in input_reader.rs catch that at
    /// runtime). Assert p99 < 20ms, max < 50ms.
    #[tokio::test]
    async fn keyboard_load_test() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-keyload-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Sanity-check: the VM is alive and reachable.
        match sandbox.exec("echo ok", &[], "/") {
            Ok((stdout, _, code)) => {
                assert_eq!(code, 0);
                assert!(stdout.contains("ok"));
            }
            Err(e) => {
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                panic!("pre-flight exec failed: {e}");
            }
        }

        use crate::ui::input::InputEditor;
        use crate::ui::renderer::Renderer;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::time::{Duration, Instant};

        let mut editor = InputEditor::new();
        let mut renderer = Renderer::new().expect("Renderer::new in test");

        const ITERATIONS: usize = 100;
        let mut latencies: Vec<Duration> = Vec::with_capacity(ITERATIONS);

        let chars: Vec<char> = "the quick brown fox jumps over the lazy dog. "
            .chars()
            .cycle()
            .take(ITERATIONS)
            .collect();

        for &ch in &chars {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            let t0 = Instant::now();

            editor.handle_key(key);
            // Build a minimal status line matching the real path's shape.
            let status = format!("{} | {} | ready", ch, "load-test");
            renderer
                .draw_bottom(&editor, &status, false)
                .expect("draw_bottom in test");

            latencies.push(t0.elapsed());
        }

        latencies.sort();
        let p50 = latencies[ITERATIONS / 2];
        let p99 = latencies[(ITERATIONS * 99) / 100];
        let max = latencies[ITERATIONS - 1];

        eprintln!(
            "keyboard_load_test: p50={:?} p99={:?} max={:?}",
            p50, p99, max
        );

        assert!(
            p99 < Duration::from_millis(20),
            "p99 latency {:?} exceeds 20ms threshold",
            p99
        );
        assert!(
            max < Duration::from_millis(50),
            "max latency {:?} exceeds 50ms threshold",
            max
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// OS-level load test: boots the microVM with a guest CPU burner,
    /// then drives synthetic keystrokes at 1000 bytes/sec through a real
    /// PTY + the production crossterm input reader, measuring wall-clock
    /// gaps between consecutive keystrokes.
    ///
    /// At 1000bps the injector writes one byte every 1ms. With taskset
    /// CPU isolation + renice -n 19, the KVM vCPU thread is pinned away
    /// from dirge's threads. The crossterm input reader should see ~1ms
    /// gaps without scheduling starvation even under guest CPU load.
    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "expensive: boots a real VM and pumps synthetic keystrokes"]
    async fn keyboard_input_reader_load_test() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-keyreader-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let cfg = MicrovmConfig {
            cpus: 1,
            memory_mib: 256,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Sanity-check: VM is alive.
        match sandbox.exec("echo ok", &[], "/") {
            Ok((stdout, _, code)) => {
                assert_eq!(code, 0);
                assert!(stdout.contains("ok"));
            }
            Err(e) => {
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                panic!("pre-flight exec failed: {e}");
            }
        }

        // Run a CPU burner inside the guest to stress the KVM vCPU thread.
        let _ = sandbox.exec(
            "nohup dd if=/dev/zero of=/dev/null bs=1M >/dev/null 2>&1 &",
            &[],
            "/",
        );

        // 1000 bytes/sec — the injector writes one byte every 1ms.
        // This exposes even brief scheduling gaps.
        let driver = match crate::sandbox::microvm::pty_harness::KeystrokeDriver::new(1000) {
            Some(d) => d,
            None => {
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                eprintln!("skipping: PTY allocation failed");
                return;
            }
        };

        const SAMPLES: usize = 500;
        let mut gaps: Vec<std::time::Duration> = Vec::with_capacity(SAMPLES - 1);
        let mut prev: Option<std::time::Instant> = None;

        for tick in driver.receiver().iter().take(SAMPLES) {
            if let Some(p) = prev {
                let gap = tick.timestamp.duration_since(p);
                if gap > std::time::Duration::from_millis(5) {
                    eprintln!("CRS-GAP (test): {:?} between keystrokes", gap);
                }
                gaps.push(gap);
            }
            prev = Some(tick.timestamp);
        }

        // Drop the driver to restore stdin + stop the reader.
        drop(driver);

        // Kill CPU burner.
        let _ = sandbox.exec(
            "killall dd 2>/dev/null; wait 2>/dev/null; echo done",
            &[],
            "/",
        );

        if gaps.is_empty() {
            sandbox.stop().ok();
            let _ = std::fs::remove_dir_all(&cache);
            eprintln!("skipping: no keystrokes collected");
            return;
        }

        gaps.sort();
        let p50 = gaps[gaps.len() / 2];
        let p99 = gaps[(gaps.len() * 99) / 100];
        let max = gaps[gaps.len() - 1];

        eprintln!(
            "keyboard_input_reader_load_test: p50={:?} p99={:?} max={:?}",
            p50, p99, max
        );

        // With taskset CPU isolation + renice -n 19, KVM vCPU threads
        // are pinned to a dedicated core and deprioritized. Even with
        // a guest CPU burner and 1000bps injection, the crossterm
        // input reader should not see starvation gaps.
        assert!(
            p99 < std::time::Duration::from_millis(120),
            "p99 crossterm gap {:?} exceeds 120ms — KVM vCPU starvation",
            p99
        );
        assert!(
            max < std::time::Duration::from_millis(300),
            "max crossterm gap {:?} exceeds 300ms — severe scheduling starvation",
            max
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Stress test: boots the microVM with 2 vCPUs, runs a CPU burner
    /// inside the guest, and injects keystrokes at 100 bytes/sec through
    /// a PTY + crossterm input reader. Measures CRS-GAP under maximum
    /// CPU contention. p99 must stay under 100ms; max under 500ms.
    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "expensive: boots a real VM with 2 vCPUs, runs CPU burners, pumps keystrokes"]
    async fn keyboard_stress_test() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-keyreader-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        // 2 vCPUs = maximum scheduling pressure on the host.
        let cfg = MicrovmConfig {
            cpus: 2,
            memory_mib: 512,
            cache_dir: cache.clone(),
            ..MicrovmConfig::default()
        };

        let mut sandbox = MicrovmSandbox::new(cfg);

        match sandbox.start().await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cache);
                panic!("VM start failed: {e}");
            }
        }

        // Sanity-check: VM is alive.
        match sandbox.exec("echo ok", &[], "/") {
            Ok((stdout, _, code)) => {
                assert_eq!(code, 0);
                assert!(stdout.contains("ok"));
            }
            Err(e) => {
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                panic!("pre-flight exec failed: {e}");
            }
        }

        // Run CPU burners on both vCPUs inside the guest — this causes
        // the KVM vCPU threads to compete with dirge's input reader.
        let _ = sandbox.exec(
            "nohup dd if=/dev/zero of=/dev/null bs=1M >/dev/null 2>&1 & \
             nohup dd if=/dev/zero of=/dev/null bs=1M >/dev/null 2>&1 &",
            &[],
            "/",
        );

        // 100 bytes/sec = one byte every 10ms — fast enough to expose
        // scheduling gaps without saturating the PTY.
        let driver = match crate::sandbox::microvm::pty_harness::KeystrokeDriver::new(100) {
            Some(d) => d,
            None => {
                sandbox.stop().ok();
                let _ = std::fs::remove_dir_all(&cache);
                eprintln!("skipping: PTY allocation failed");
                return;
            }
        };

        const SAMPLES: usize = 500;
        let mut gaps: Vec<std::time::Duration> = Vec::with_capacity(SAMPLES - 1);
        let mut prev: Option<std::time::Instant> = None;
        let mut worst_gap = std::time::Duration::ZERO;
        let mut gap_count_below_50ms = 0usize;

        for tick in driver.receiver().iter().take(SAMPLES) {
            if let Some(p) = prev {
                let gap = tick.timestamp.duration_since(p);
                if gap > worst_gap {
                    worst_gap = gap;
                }
                if gap < std::time::Duration::from_millis(50) {
                    gap_count_below_50ms += 1;
                }
                gaps.push(gap);
            }
            prev = Some(tick.timestamp);
        }

        // Drop the driver to restore stdin + stop the reader.
        drop(driver);

        // Kill CPU burners.
        let _ = sandbox.exec(
            "killall dd 2>/dev/null; wait 2>/dev/null; echo done",
            &[],
            "/",
        );

        if gaps.is_empty() {
            sandbox.stop().ok();
            let _ = std::fs::remove_dir_all(&cache);
            eprintln!("skipping: no keystrokes collected");
            return;
        }

        gaps.sort();
        let p50 = gaps[gaps.len() / 2];
        let p99 = gaps[(gaps.len() * 99) / 100];
        let p999 = gaps[((gaps.len() as f64) * 0.999) as usize];
        let max = gaps[gaps.len() - 1];

        let pct_below_50 = (gap_count_below_50ms * 100) / gaps.len();

        eprintln!(
            "keyboard_stress_test: {} samples, p50={:?} p99={:?} p99.9={:?} max={:?} worst={:?} below_50ms={}%",
            gaps.len(),
            p50,
            p99,
            p999,
            max,
            worst_gap,
            pct_below_50
        );

        // With taskset CPU isolation + renice -n 19, even under guest
        // CPU burners the KVM vCPU threads are pinned away from dirge's
        // threads. Assert that CRS-GAP stays within injector bounds.
        assert!(
            p99 < std::time::Duration::from_millis(200),
            "p99 crossterm gap {:?} exceeds 200ms under CPU stress — KVM starvation",
            p99
        );
        assert!(
            max < std::time::Duration::from_millis(500),
            "max crossterm gap {:?} exceeds 500ms under CPU stress — severe starvation",
            max
        );

        sandbox.stop().ok();
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Verify that a long-running command (`sleep 300`) is killed by
    /// the dual-layer timeout (guest-side `timeout N` prefix +
    /// host-side `tokio::time::timeout` around `spawn_blocking`).
    #[tokio::test]
    async fn timeout_kills_long_running_command() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-timeout-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);

        let sb = Sandbox::new(SandboxMode::Microvm);
        // Override the default image to use the local test image.
        sb.set_microvm_image("local://dirge-microvm:alpine".to_string())
            .ok();
        // Set minimal resources for fast boot.
        sb.set_microvm_resources(1, 256).ok();

        let start = std::time::Instant::now();
        let result = sb.exec("sleep 300", 2).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "sleep 300 with 2s timeout should fail, got: {result:?}"
        );
        let msg = format!("{:?}", result);
        assert!(
            msg.contains("timed out after 2s"),
            "expected 'timed out after 2s' in error: {msg}"
        );
        // Must return within 10s — if we waited the full 300s this
        // test would hang the suite.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "timeout took too long: {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    /// Boot a minimal Alpine microVM using the OCI puller (no buildah needed).
    ///
    /// This proves the VM actually boots on the current platform without
    /// requiring a locally-built `dirge-microvm:*` image. It uses the pure
    /// Rust OCI puller to fetch Alpine from Docker Hub, writes a simple
    /// `.krun_config.json` (no SSH), spawns the runner, and verifies the
    /// init command ran inside the guest.
    #[tokio::test]
    async fn microvm_boots_alpine_via_oci() {
        if !vm_available() {
            eprintln!("skipping: hardware virtualization not available");
            return;
        }
        let _guard = serial_vm_test();

        let cache = std::env::temp_dir().join(format!(
            "dirge-test-alpine-boot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).expect("create cache dir");

        // Pull alpine via OCI puller (pure Rust, no buildah/Docker needed).
        let rootfs = cache.join("rootfs");
        crate::sandbox::microvm::oci::pull("docker.io/library/alpine:latest", &rootfs, &cache)
            .await
            .expect("OCI pull of alpine:latest failed");

        // Verify the rootfs has /bin/sh
        assert!(
            rootfs.join("bin").join("sh").exists() || rootfs.join("bin").join("busybox").exists(),
            "alpine rootfs missing /bin/sh or /bin/busybox"
        );

        // Find the runner binary and create workspace dir.
        let binary = crate::sandbox::microvm::runner::find_runner_binary()
            .expect("dirge-microvm-runner binary not found");
        let workspace = cache.join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace dir");

        // Write runner config with exec_cmd so it works on both Linux
        // (where libkrun's init reads .krun_config.json) and macOS
        // (where we must pass exec_cmd via krun_set_exec).
        let config = serde_json::json!({
            "rootfs_path": rootfs,
            "workspace_path": workspace,
            "ssh_port": 0,
            "cpus": 1,
            "memory_mib": 256,
            "exec_cmd": ["/bin/sh", "-c", "echo 'VM booted successfully' > /workspace/booted"],
        });
        // Spawn the runner process using tokio::process::Command so the
        // async timeout below can actually fire (std::process::wait() is
        // blocking and would stall the entire tokio runtime).
        let mut child = tokio::process::Command::new(&binary)
            .arg(serde_json::to_string(&config).expect("serialize runner config"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn dirge-microvm-runner");

        // Wait for the VM to boot, run the command, and exit.
        // Use a 120s timeout in case something goes wrong so the test
        // doesn't hang the suite indefinitely.
        let output = tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await;

        let output = match output {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                let _ = std::fs::remove_dir_all(&cache);
                panic!("runner process error: {e}");
            }
            Err(_) => {
                // Timeout — kill the runner.
                let _ = child.kill().await;
                let _ = std::fs::remove_dir_all(&cache);
                panic!("runner timed out after 120s — VM may not have booted");
            }
        };

        // Read stderr after process exits.
        // NOTE: `child.stderr` is a field, not a method — `tokio::process::Child`
        // exposes it as `Option<ChildStderr>`, unlike the std version.
        // Also note: `tokio::process::ChildStderr` implements `tokio::io::AsyncRead`,
        // not `std::io::Read`, so we must use the async trait to read it.
        let stderr = if let Some(mut s) = child.stderr.take() {
            let mut buf = String::new();
            use tokio::io::AsyncReadExt;
            let _ = s.read_to_string(&mut buf).await;
            buf
        } else {
            String::new()
        };

        // Check booted marker file.
        // On macOS the workspace is inside the rootfs (the runner creates
        // {rootfs_path}/workspace for the guest). On Linux the workspace is
        // a separate virtio-fs mount at the configured workspace_path.
        let booted = if cfg!(target_os = "macos") {
            rootfs.join("workspace").join("booted")
        } else {
            workspace.join("booted")
        };
        let booted_content = if booted.exists() {
            std::fs::read_to_string(&booted).unwrap_or_default()
        } else {
            String::new()
        };

        let _ = std::fs::remove_dir_all(&cache);

        assert!(
            output.success(),
            "runner failed (exit={:?}): {}",
            output.code(),
            stderr,
        );
        assert!(
            booted.exists(),
            "VM did not boot — /workspace/booted not found.\n\
             Runner stderr: {stderr}\n\
             Rootfs /bin/sh exists: {}",
            rootfs.join("bin").join("sh").exists() || rootfs.join("bin").join("busybox").exists(),
        );
        assert!(
            booted_content.contains("VM booted successfully"),
            "unexpected boot marker content: {booted_content:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dyld_fallback_library_path_valid_on_macos() {
        // Regression guard: the DYLD_FALLBACK_LIBRARY_PATH the spawn code
        // builds must contain a directory where libkrunfw.5.dylib lives.
        // Uses the same logic as mod.rs: MicrovmSandbox::start().
        use std::path::Path;

        // Resolve brew prefix (same as the spawn code).
        let brew_prefixes: Vec<String> = [
            std::process::Command::new("brew")
                .args(["--prefix", "libkrunfw"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() || !o.stdout.is_empty() {
                        Some(
                            std::str::from_utf8(&o.stdout)
                                .unwrap_or("")
                                .trim()
                                .to_string(),
                        )
                    } else {
                        None
                    }
                }),
            std::process::Command::new("brew")
                .args(["--prefix"])
                .output()
                .ok()
                .and_then(|o| {
                    let s = std::str::from_utf8(&o.stdout)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if s.is_empty() { None } else { Some(s) }
                }),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();

        eprintln!("brew prefixes resolved: {:?}", brew_prefixes);

        // At least one brew prefix should be /opt/homebrew or /usr/local.
        assert!(
            !brew_prefixes.is_empty(),
            "brew --prefix returned no prefixes — is Homebrew installed?"
        );

        // Each prefix should have a /lib subdirectory with libkrunfw.5.dylib.
        let krunfw_name = "libkrunfw.5.dylib";
        let found = brew_prefixes.iter().any(|p| {
            let lib_path = Path::new(p).join("lib").join(krunfw_name);
            let exists = lib_path.exists();
            eprintln!(
                "  {} -> {}",
                lib_path.display(),
                if exists { "OK" } else { "MISSING" }
            );
            exists
        });

        assert!(
            found,
            "libkrunfw.5.dylib not found under any brew prefix -- {} --          install it: brew tap libkrun/krun && brew trust libkrun/krun &&          brew install libkrun libkrunfw",
            brew_prefixes
                .iter()
                .map(|p| format!("{p}/lib/{krunfw_name}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
}
