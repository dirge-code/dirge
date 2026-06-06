# MicroVM Sandbox — Workspace Mirroring

The host's current working directory (where you ran `dirge`) is mirrored
into the VM at `/workspace` via virtio-fs. This means the agent can read,
write, build, and test your project files from inside the VM — and you
see the results on the host immediately.

## How it works

1. When the runner starts, it calls `krun_add_virtiofs(ctx, "workspace", <host_path>)`
   to register the host directory as a virtio-fs share tagged `workspace`.

2. libkrun's init process runs `.krun_config.json`, which includes:
   ```json
   {
     "Cmd": ["/bin/sh", "-c", "...
       mkdir -p /workspace
       && mount -t virtiofs workspace /workspace
       && ..."]
   }
   ```

3. Every bash tool command is prefixed with `cd /workspace &&`:
   ```
   ssh sandbox@127.0.0.1 "cd /workspace && cargo build"
   ```

4. The `/sandbox attach` interactive shell also starts in `/workspace`:
   ```
   ssh -t sandbox@127.0.0.1 "cd /workspace && exec $SHELL -l"
   ```

## File ownership and permissions

**Inside the VM,** all workspace files appear owned by `root:root` because
libkrun maps the virtio-fs share as root. The `sandbox` user (uid 1000)
can still read and write them — virtio-fs's default security model uses
the host's DAC (Discretionary Access Control) for actual access checks.

**On the host,** files created inside the VM are owned by your host user.
The virtio-fs daemon runs as the host user that spawned the runner, so
new files get that user's uid/gid.

### Practical implications

- **`git` inside the VM works normally.** git operations use the workspace
  files through virtio-fs. The git config, credentials, and hooks are
  whatever exists in your workspace's `.git/`.

- **Build artifacts land on the host.** `target/`, `node_modules/`,
  `__pycache__/`, etc. appear in your real project directory. They're
  owned by your host user and persist after the VM shuts down.

- **File permissions are preserved.** `chmod +x script.sh` inside the VM
  works and the executable bit is visible on the host.

- **Symlinks work** for paths within `/workspace`. Symlinks pointing
  outside `/workspace` (e.g., to `/usr`) won't resolve inside the VM
  because `/usr` in the VM is the guest's `/usr`, not the host's.

## What is NOT shared

Only the workspace directory is shared. Everything else in the VM is
the guest's own filesystem:

- `/home/sandbox/` — the sandbox user's home (VM-local, ephemeral)
- `/tmp/` — VM-local tmpfs
- `/etc/`, `/usr/`, `/var/` — the guest's own system files
- Installed packages (`apt install`, `pip install`, etc.) — these go
  into the guest's rootfs, not the host

Files written to `/home/sandbox/` or `/tmp/` inside the VM are lost
when the VM shuts down.

## Performance

virtio-fs is a FUSE-based shared filesystem optimized for VM↔host
file sharing. It uses DAX (Direct Access) windows to map the host's
page cache directly into the guest, avoiding copies.

**Typical throughput** on a modern NVMe drive: ~1-3 GB/s for sequential
reads, ~500 MB/s for random reads.

**Latency** is higher than native: ~20-50 µs per operation vs ~5-10 µs
native. This matters for workloads that do millions of small file
operations (`npm install`, `cargo check` on a cold cache). In practice,
the overhead is modest because the VM's page cache absorbs most of it.

### Tuning for build-heavy workloads

1. **Use a CoW filesystem** (btrfs, xfs) for the cache directory.
   The rootfs clone uses `copy_file_range` which creates instant
   reflinks on these filesystems.

2. **Keep `target/` on the host.** Build artifacts persist across
   VM sessions, so `cargo build` is incremental by default.

3. **Mount the workspace with `cache=always`** if your virtio-fs
   version supports it. libkrun's defaults are usually fine.

## Caveats

### File locking

virtio-fs supports POSIX locks (`fcntl F_SETLK`) but they're proxied
to the host. Some applications (notably `sqlite` and certain build
tools) may see unexpected locking behavior. The agent's bash tool
runs commands sequentially, so concurrent access conflicts are rare
in practice.

### inotify / file watching

`inotify` events from the virtio-fs mount don't always propagate
reliably. File watchers like `cargo watch`, `nodemon`, or `fswatch`
may miss events or fire spuriously. Prefer polling-based approaches
inside the VM.

### Hardlinks across mount points

Hardlinks between `/workspace` and other directories don't work
(different filesystems). This can affect build tools that use
hardlinks for caching (e.g., some `nix` configurations).

### Large files

Files larger than the VM's available RAM may cause memory pressure
during virtio-fs transfers. The DAX window maps files directly,
so sequential access is fine, but random access to very large files
(multiple GB) may be slower than native.

### sccache and build caches

If you use `sccache` with a shared cache directory inside the
workspace, it works normally through virtio-fs. The cache files
are visible on both sides.

## Accessing workspace files from the host during a session

Workspace files are always accessible from the host — virtio-fs
provides live sharing, not a snapshot. You can edit files in your
editor while the VM is running, and the agent inside the VM will
see the changes on its next read.

Similarly, files the agent writes inside the VM (build outputs,
test results, generated code) are immediately visible on the host.
