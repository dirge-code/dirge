# MicroVM Sandbox — Architecture

## Overview

```
┌─────────────────────────────────────────────────────────┐
│  dirge (host process)                                   │
│                                                         │
│  ┌──────────────┐    SSH (ssh2 crate)    ┌───────────┐  │
│  │ bash tool    │───────────────────────→│ microVM   │  │
│  │ (agent loop) │←───────────────────────│ guest     │  │
│  └──────────────┘    stdout/stderr/code  │           │  │
│                                          │  sshd     │  │
│  ┌──────────────┐                        │  /workspace│  │
│  │ /sandbox     │  PTY relay (ssh -t)    │  (virtiofs)│  │
│  │ attach       │───────────────────────→│           │  │
│  └──────────────┘                        └───────────┘  │
│                                                ↑         │
│  ┌──────────────────────┐                      │         │
│  │ dirge-microvm-runner │  krun_start_enter()  │         │
│  │ (child process)      │──────────────────────┘         │
│  └──────────────────────┘                                │
│         │                                                │
│         │ spawns, passes JSON config                     │
│         ▼                                                │
│  ┌──────────────────────────────────────────────────┐   │
│  │  libkrun (KVM)                                   │   │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────────────┐   │   │
│  │  │ virtiofs │  │ virtiofs│  │ TSI (network)   │   │   │
│  │  │ rootfs   │  │ workspace│  │ port forward    │   │   │
│  │  └─────────┘  └─────────┘  └─────────────────┘   │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Components

### 1. Runner binary (`dirge-microvm-runner`)

A small (~200 line) binary in `src/bin/dirge-microvm-runner.rs`. It:

1. Receives a JSON config on argv[1] with `rootfs_path`, `workspace_path`,
   `ssh_port`, `cpus`, `memory_mib`
2. Creates a libkrun context (`krun_create_ctx`)
3. Sets vCPU count and RAM (`krun_set_vm_config`)
4. Raises `RLIMIT_NOFILE` to max for both host and guest (TSI and virtio-fs
   consume file descriptors per operation)
5. Configures the rootfs as a virtio-fs mount (`krun_set_root`)
6. Adds the workspace as a second virtio-fs mount tagged `workspace`
   (`krun_add_virtiofs`)
7. Sets up port forwarding `host:ssh_port → guest:22` (`krun_set_port_map`)
8. Calls `krun_start_enter()` — **blocks until the guest shuts down**

> **Important:** We do NOT call `krun_set_exec()`. Instead, libkrun's built-in
> init process reads `.krun_config.json` from the rootfs to determine what to
> execute. This matches the go-microvm approach and avoids baking the init
> command into the binary.

### 2. Root filesystem

Managed by `src/sandbox/microvm/rootfs.rs`. Two sourcing modes:

**Local images** (`local://` prefix — the default):
- `dirge sandbox setup` builds a Dockerfile with `buildah bud`
- On first use, `prepare_local()` exports the image as an OCI archive
  (`buildah push → oci-archive:.tar`), parses the OCI manifest to get
  layer order, then extracts each layer with `gzip -dc | tar -x`
- All files end up owned by the host user (tar `--no-same-owner`)

**Remote images** (any other ref — Docker Hub, GHCR, etc.):
- The pure-Rust OCI puller in `src/sandbox/microvm/oci.rs` handles
  authentication (bearer token flow), manifest resolution (multi-arch,
  picks `linux/amd64` or `linux/arm64`), and layer download
- Layers are cached by digest in `<cache_dir>/blobs/<algo>/<hex>`
- Extraction uses the same `gzip | tar` pipeline

**Caching:**
- The extracted rootfs is cached at `<cache_dir>/<image_safe>/base/`
- On subsequent boots, `cp_r()` clones the cache with `copy_file_range`
  (CoW reflinks on btrfs/xfs, full copy on ext4)
- Each session gets its own clone at `<cache_dir>/<image_safe>/session-<pid>/`
- The clone is cleaned up on drop

### 3. SSH communication

`src/sandbox/microvm/ssh.rs` manages:

**Port forwarding** — `krun_set_port_map` maps `host:ssh_port → guest:22`.
libkrun binds the host side to 127.0.0.1 only; the VM's SSH is never
network-exposed. The host key is verified on every handshake.

**Ephemeral keys** — `ssh-keygen -t ed25519` generates a key pair on every
session. The public key is injected into the rootfs's
`/home/sandbox/.ssh/authorized_keys` before boot. The private key is held
in a temp directory cleaned up on drop.

**Host keys** — generated on the host and injected into `/etc/ssh/` before
boot. This avoids ownership issues from extracting OCI layers as non-root:
the host key files appear root-owned inside the VM (libkrun's init runs as
root), satisfying sshd's permission checks. Stale host keys from the image
build are removed during injection.

**`ssh_exec()`** — the workhorse. Uses the `ssh2` crate (libssh2 bindings)
to:
1. Open a TCP connection to `127.0.0.1:<port>`
2. Perform SSH handshake
3. Authenticate with the ephemeral private key as user `sandbox`
4. Open a session channel, execute `cd /workspace && <command>`
5. Read stdout and stderr separately, return exit code

**`wait_for_ssh()`** — polls TCP connect with a timeout. Used after spawning
the runner to ensure sshd is ready before the first exec.

### 4. Workspace mirroring (virtio-fs)

The host's current working directory is exported to the guest as a virtio-fs
filesystem mounted at `/workspace`. The mount tag is `workspace`.

Inside `.krun_config.json`, the init command includes:
```json
{
  "Cmd": ["/bin/sh", "-c", "...
    mkdir -p /workspace
    && mount -t virtiofs workspace /workspace
    && ..."]
}
```

All bash tool commands are prefixed with `cd /workspace &&` so they execute
in the mirrored directory.

**Important behaviors** (see [WORKSPACE.md](WORKSPACE.md) for details):
- Files appear root-owned inside the guest (libkrun maps the virtio-fs
  share as root). The `sandbox` user can read/write them because virtio-fs
  doesn't enforce Unix permissions by default.
- File changes are visible on both sides immediately — virtio-fs is a
  live shared filesystem, not a one-shot copy.
- Symlinks, hardlinks, and special files may behave differently inside
  the VM.

### 5. The bash tool backend

`src/sandbox/backend.rs` defines the `SandboxBackend` trait. The
`MicrovmBackend` implementation:

```rust
async fn exec(&self, command: &str, timeout_secs: u64) -> Result<InterleavedOutput, ToolError> {
    // Lock the VM, start it if not running, get SSH port + key
    // Drop the lock before the blocking SSH call
    // Run ssh_exec on a spawn_blocking thread
}
```

The VM starts lazily — the first bash call triggers `start()`. Subsequent
calls reuse the running VM. The VM is stopped when dirge exits (Drop impl
kills the runner child process).

### 6. PTY relay (interactive attach)

`/sandbox attach` doesn't use the `ssh2` crate — it spawns the real `ssh`
CLI with `-t` (force PTY allocation) so readline, colors, and interactive
programs work correctly. The SSH process runs on a PTY pair managed by
`src/ui/pty_relay.rs`:

1. Stop the crossterm input reader (so it doesn't race for stdin)
2. Suspend the TUI (leave alt screen, disable mouse)
3. Drain any keystrokes buffered in stdin during the suspend window
4. Spawn `ssh -t sandbox@127.0.0.1 ...` with its stdio on the PTY secondary
5. A relay thread copies bytes between the PTY primary and `/dev/tty`
6. When SSH exits, restart the input reader, restore the TUI, and
   reinject drained keystrokes

### 7. Scheduler isolation

To prevent KVM vCPU threads from starving dirge's input-reader thread
(causing typing stutter during `/sandbox attach`):

- The runner process is `renice`d to +19 (lowest CFS priority) immediately
  after spawn
- The input-reader thread sets `setpriority(PRIO_PROCESS, 0, -10)` (highest
  CFS priority among non-root processes)
- This gives the input reader ~50× scheduling weight over KVM threads

## Lifecycle

```
dirge start
  └─ Sandbox::new(Microvm)
       └─ MicrovmSandbox::new(config)   # just stores config, no VM yet

first bash call
  └─ MicrovmBackend::exec("ls")
       └─ guard.start().await
            ├─ rootfs::prepare(image, cache_dir)
            │    ├─ if local://: buildah push → OCI archive → extract
            │    └─ else: oci::pull → extract
            ├─ EphemeralKeys::generate()     # ssh-keygen -t ed25519
            ├─ HostKeys::generate()           # for /etc/ssh/
            ├─ Inject: authorized_keys, host keys, .krun_config.json
            ├─ TcpListener::bind("127.0.0.1:0")  # pick free port
            ├─ spawn dirge-microvm-runner
            ├─ renice +19 runner
            └─ wait_for_ssh(port, 30s)
       └─ ssh_exec("127.0.0.1", port, key, "cd /workspace && ls")

subsequent bash calls
  └─ MicrovmBackend::exec("cargo build")
       └─ ssh_exec(...)  # VM already running, SSH port cached

/sandbox attach
  └─ cmd_sandbox_attach()
       ├─ stop input reader
       ├─ suspend TUI
       ├─ spawn ssh -t (on PTY relay)
       └─ restore TUI, restart reader

dirge exit
  └─ MicrovmSandbox::drop()
       └─ stop() → kill child process → wait
       └─ rootfs session clone deleted
```
