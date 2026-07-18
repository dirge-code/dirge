//! dirge-microvm-runner — boots a microVM using libkrun and blocks until exit.
//!
//! This binary is spawned as a child process by dirge. It calls
//! `krun_start_enter()` which blocks until the guest VM shuts down.
//!
//! Platform-specific setup:
//! - On macOS, binary must be codesigned with `com.apple.security.hypervisor`
//!   entitlement. The build script handles this automatically.
//! - On Linux, no codesigning is needed.
//!
//! Two execution modes:
//! 1. If config provides `exec_cmd` (array of strings), calls krun_set_exec()
//!    to set the guest command directly. Used on macOS where
//!    libkrun does not support /.krun_config.json init parsing;
//!    on Linux this is optional.
//! 2. Otherwise, relies on libkrun's built-in init reading
//!    /.krun_config.json from the rootfs (Linux-only).
//!
//! Usage: dirge-microvm-runner '<json-config>'
//!
//! JSON config fields:
//!   rootfs_path:     path to the guest root filesystem (virtio-fs)
//!   workspace_path:  path to mount as /workspace inside the VM
//!   ssh_port:        host port for SSH forwarding (guest:22)
//!   cpus:            number of vCPUs
//!   memory_mib:      RAM in MiB
//!   exec_cmd:        optional array of strings [cmd, arg1, ...] (sets krun_set_exec)

#![allow(unused_imports)]

use std::ffi::CString;

// Re-export libkrun_sys's krun_set_log_level, krun_create_ctx, etc. are
// linked dynamically via the `libkrun-sys` crate.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!(
        "dirge-microvm-runner: the microVM sandbox is only supported on Linux and macOS          (requires KVM or Hypervisor.framework)."
    );
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    let config_json = std::env::args()
        .nth(1)
        .expect("usage: dirge-microvm-runner '<json-config>'");

    let config: serde_json::Value =
        serde_json::from_str(&config_json).expect("invalid JSON config");

    let rootfs_path = config["rootfs_path"].as_str().expect("missing rootfs_path");
    let workspace_path = config["workspace_path"]
        .as_str()
        .expect("missing workspace_path");
    let ssh_port: u16 = config["ssh_port"].as_u64().unwrap_or(0) as u16;
    let cpus: u8 = config["cpus"].as_u64().unwrap_or(2) as u8;
    let memory_mib: u32 = config["memory_mib"].as_u64().unwrap_or(512) as u32;
    let exec_cmd: Option<Vec<String>> = config["exec_cmd"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    unsafe {
        #[cfg(debug_assertions)]
        libkrun_sys::krun_set_log_level(0);

        let ctx = libkrun_sys::krun_create_ctx();
        assert!(ctx >= 0, "krun_create_ctx failed: {ctx}");

        let rc = libkrun_sys::krun_set_vm_config(ctx as u32, cpus, memory_mib);
        assert!(rc == 0, "krun_set_vm_config failed: {rc}");

        // ── fd limit ──────────────────────────────────────────
        #[cfg(target_os = "linux")]
        {
            let mut rlim: libc::rlimit = std::mem::zeroed();
            assert!(
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) == 0,
                "getrlimit(RLIMIT_NOFILE) failed"
            );
            // Cap at 1_048_576 — macOS returns RLIM_INFINITY which causes the
            // guest kernel to reject the cmdline ("TooLarge"). The krun_set_rlimits
            // call below serialises this value into the kernel cmdline.
            let max_nofile: libc::rlim_t = 1_048_576;
            rlim.rlim_cur = rlim.rlim_max.min(max_nofile);
            if rlim.rlim_cur < 4096 {
                rlim.rlim_cur = 4096;
            }
            assert!(
                libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0,
                "setrlimit(RLIMIT_NOFILE) failed"
            );

            let nofile_rlimit = format!(
                "RLIMIT_NOFILE={}:{}",
                rlim.rlim_cur,
                rlim.rlim_max.min(max_nofile),
            );
            let nofile_cstr = CString::new(nofile_rlimit).unwrap();
            let rlimits: [*const std::ffi::c_char; 2] = [nofile_cstr.as_ptr(), std::ptr::null()];
            let rc = libkrun_sys::krun_set_rlimits(ctx as u32, rlimits.as_ptr());
            assert!(rc == 0, "krun_set_rlimits failed: {rc}");
        }
        // On macOS, raise RLIMIT_NOFILE on the host (virtio-fs needs it).
        // Do NOT call krun_set_rlimits — RLIM_INFINITY overflows the aarch64
        // 2048-byte kernel cmdline, and libkrun 1.19.4 may not support it.
        // On Linux, krun_set_rlimits is called above with a capped value.
        #[cfg(target_os = "macos")]
        {
            let mut rlim: libc::rlimit = std::mem::zeroed();
            assert!(
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) == 0,
                "getrlimit(RLIMIT_NOFILE) failed"
            );
            rlim.rlim_cur = rlim.rlim_max;
            assert!(
                libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0,
                "setrlimit(RLIMIT_NOFILE) failed"
            );
        }

        // Root filesystem via virtio-fs.
        let root_cstr = CString::new(rootfs_path).unwrap();
        let rc = libkrun_sys::krun_set_root(ctx as u32, root_cstr.as_ptr());
        assert!(rc == 0, "krun_set_root failed: {rc}");

        // Working directory.
        let workdir_cstr = CString::new("/").unwrap();
        let rc = libkrun_sys::krun_set_workdir(ctx as u32, workdir_cstr.as_ptr());
        assert!(rc == 0, "krun_set_workdir failed: {rc}");

        // Workspace virtio-fs device for Linux (root + workspace = 2 devices, works).
        #[cfg(target_os = "linux")]
        {
            let ws_cstr = CString::new(workspace_path).unwrap();
            let tag_cstr = CString::new("workspace").unwrap();
            let rc =
                libkrun_sys::krun_add_virtiofs(ctx as u32, tag_cstr.as_ptr(), ws_cstr.as_ptr());
            assert!(rc == 0, "krun_add_virtiofs failed: {rc}");
        }
        // Workspace: on Linux a virtio-fs device; on macOS libkrun@1.19.4 hits
        // EMSGSIZE with >1 device, so workspace is a plain directory inside rootfs.
        #[cfg(target_os = "macos")]
        {
            let _ = &workspace_path; // consumed above on Linux; suppress unused warning
        }
        // Create the workspace directory inside the rootfs so the guest
        // can mount to it later (Linux: virtio-fs root, macOS: dir in rootfs).
        let _ = std::fs::create_dir_all(format!("{rootfs_path}/workspace"));

        // Port forward: host:ssh_port -> guest:22.
        if ssh_port > 0 {
            let port_map_str = format!("{ssh_port}:22");
            let pm_cstr = CString::new(port_map_str).unwrap();
            let port_maps: [*const std::ffi::c_char; 2] = [pm_cstr.as_ptr(), std::ptr::null()];
            let rc = libkrun_sys::krun_set_port_map(ctx as u32, port_maps.as_ptr());
            assert!(rc == 0, "krun_set_port_map failed: {rc}");
        }

        // Set the command to execute inside the VM.
        if let Some(cmd) = &exec_cmd {
            let exec_path = CString::new(cmd[0].as_bytes()).unwrap();
            let argv: Vec<CString> = cmd
                .iter()
                .skip(1)
                .map(|s| CString::new(s.as_bytes()).unwrap())
                .collect();
            let mut argv_ptrs: Vec<*const std::ffi::c_char> =
                argv.iter().map(|s| s.as_ptr()).collect();
            argv_ptrs.push(std::ptr::null());

            // Build environment for the guest: PATH, HOSTNAME, HOME.
            let hostname = CString::new("HOSTNAME=microvm").unwrap();
            let home = CString::new("HOME=/root").unwrap();
            let path =
                CString::new("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                    .unwrap();
            let env: Vec<CString> = vec![path, hostname, home];
            let mut env_ptrs: Vec<*const std::ffi::c_char> =
                env.iter().map(|s| s.as_ptr()).collect();
            env_ptrs.push(std::ptr::null());

            // Do NOT pass NULL envp — libkrun captures all host env vars and
            // serialises them into the kernel cmdline, overflowing the 2048-byte
            // aarch64 limit.
            let rc = libkrun_sys::krun_set_exec(
                ctx as u32,
                exec_path.as_ptr(),
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            );
            assert!(rc == 0, "krun_set_exec failed: {rc}");
        }

        // Boot the VM — blocks until guest exits.
        let rc = libkrun_sys::krun_start_enter(ctx as u32);
        assert!(rc == 0, "krun_start_enter failed: {rc}");
    }
}
