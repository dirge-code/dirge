//! dirge-microvm-runner — boots a microVM using libkrun and blocks until exit.
//!
//! This binary is spawned as a child process by dirge. It calls
//! `krun_start_enter()` which blocks until the guest VM shuts down.
//!
//! IMPORTANT: We do NOT call krun_set_exec(). Instead, libkrun's built-in
//! init process reads /.krun_config.json from the rootfs to determine
//! what to execute. This matches go-microvm's approach.
//!
//! Usage: dirge-microvm-runner '<json-config>'
//!
//! JSON config fields:
//!   rootfs_path:     path to the guest root filesystem (virtio-fs)
//!   workspace_path:  path to mount as /workspace inside the VM
//!   ssh_port:        host port for SSH forwarding (guest:22)
//!   cpus:            number of vCPUs
//!   memory_mib:      RAM in MiB

use std::ffi::CString;

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

    unsafe {
        libkrun_sys::krun_set_log_level(0);

        let ctx = libkrun_sys::krun_create_ctx();
        assert!(ctx >= 0, "krun_create_ctx failed: {ctx}");

        let rc = libkrun_sys::krun_set_vm_config(ctx as u32, cpus, memory_mib);
        assert!(rc == 0, "krun_set_vm_config failed: {rc}");

        // ── fd limit ──────────────────────────────────────────
        // virtio-fs and TSI each consume host fds per guest
        // operation. Raise RLIMIT_NOFILE before creating virtio-fs
        // devices, and tell libkrun to set the same limit in the
        // guest via krun_set_rlimits. Without this, the guest sees
        // soft=1024/hard=4096 and hits EMFILE immediately under any
        // real workload (cargo downloads open dozens of concurrent
        // TCP connections, each proxied through a host fd by TSI).
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

        let nofile_rlimit = format!(
            "{}={}:{}",
            libc::RLIMIT_NOFILE,
            rlim.rlim_cur,
            rlim.rlim_max
        );
        let nofile_cstr = to_cstr(&nofile_rlimit);
        let rlimits: [*const std::ffi::c_char; 2] = [nofile_cstr.as_ptr(), std::ptr::null()];
        let rc = libkrun_sys::krun_set_rlimits(ctx as u32, rlimits.as_ptr());
        assert!(rc == 0, "krun_set_rlimits failed: {rc}");

        // Root filesystem via virtio-fs.
        let root_cstr = to_cstr(rootfs_path);
        let rc = libkrun_sys::krun_set_root(ctx as u32, root_cstr.as_ptr());
        assert!(rc == 0, "krun_set_root failed: {rc}");

        // Workspace mount via virtio-fs.
        let ws_cstr = to_cstr(workspace_path);
        let tag_cstr = to_cstr("workspace");
        let rc = libkrun_sys::krun_add_virtiofs(ctx as u32, tag_cstr.as_ptr(), ws_cstr.as_ptr());
        assert!(rc == 0, "krun_add_virtiofs failed: {rc}");

        // Port forward: host:ssh_port -> guest:22.
        if ssh_port > 0 {
            let port_map_str = format!("{ssh_port}:22");
            let pm_cstr = to_cstr(&port_map_str);
            let port_maps = [pm_cstr.as_ptr(), std::ptr::null()];
            let rc = libkrun_sys::krun_set_port_map(ctx as u32, port_maps.as_ptr());
            assert!(rc == 0, "krun_set_port_map failed: {rc}");
        }

        // NOTE: No krun_set_exec() call. libkrun reads /.krun_config.json
        // from the rootfs. The parent writes this file before spawning us.

        // Boot the VM — blocks until guest exits.
        let rc = libkrun_sys::krun_start_enter(ctx as u32);
        assert!(rc == 0, "krun_start_enter failed: {rc}");
    }
}

fn to_cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap())
}
