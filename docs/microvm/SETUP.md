# MicroVM Sandbox — Setup

This guide walks through installing the prerequisites, building the runner
binary, and getting a microVM booted.

## Prerequisites

### Required (hard blockers)

| Dependency | Purpose | Install |
|-----------|---------|---------|
| `/dev/kvm` | Hardware virtualization | Load `kvm` kernel module, user must be in `kvm` group |
| `libkrun.so` + `libkrunfw.so` | KVM-based VM runtime | See [libkrun releases](https://github.com/containers/libkrun/releases) |
| `gzip` + `tar` | OCI layer extraction | Already present on most Linux systems |
| `ssh-keygen` | Ephemeral SSH key generation | `apt install openssh-client` |
| `dirge-microvm-runner` | The binary that calls `krun_start_enter` | Built alongside dirge (see below) |

### Optional

| Dependency | When needed | Install |
|-----------|-------------|---------|
| `buildah` | Using `local://` images (the default) | `apt install buildah` |
| `mold` linker | Faster rebuilds of the runner | `apt install mold` |

## Check your system

```bash
dirge sandbox check
```

This prints a report of every dependency and its status. Fix any `ERROR` items
before proceeding. `WARN` items (buildah, mold) are optional.

### What the check covers

1. `/dev/kvm` — exists and accessible
2. `libkrun.so` — found via `ldconfig -p` or common paths
3. `libkrunfw.so` — same
4. `gzip` — on PATH
5. `tar` — on PATH
6. `ssh-keygen` — on PATH
7. `dirge-microvm-runner` — binary found adjacent to dirge or on PATH
8. `buildah` — optional, warns if missing
9. `mold` — optional, warns if missing

## Building

### Build dirge with the sandbox-microvm feature

```bash
cargo build --release --features sandbox-microvm
```

This compiles both `dirge` and `dirge-microvm-runner`. The runner binary lands
at `target/release/dirge-microvm-runner`. dirge finds it by looking next to its
own binary or on `$PATH`.

> **Caveat:** If `libkrun.so` / `libkrunfw.so` are not installed, the runner
> binary will fail to *link* (unresolved symbols). The main `dirge` binary and
> all non-VM tests still compile fine — `build.rs` emits a warning but doesn't
> abort. Install libkrun before building if you need the runner.

### Pre-built binaries

The `sandbox-microvm` feature is **not** in the default feature set (it pulls
in `libkrun-sys` and `ssh2`). You must opt in:

```bash
# From source
cargo install dirge-agent --features sandbox-microvm

# Or clone and build
git clone https://github.com/dirge-code/dirge
cd dirge
cargo build --release --features sandbox-microvm
```

## Setup (one-time)

```bash
dirge sandbox setup
```

This does three things:

1. **Checks dependencies** — same as `dirge sandbox check`
2. **Builds the guest image** — runs `buildah bud` with the Debian Dockerfile
   from `images/debian/Dockerfile`, producing a local image tagged
   `dirge-microvm:debian`. If the image already exists in buildah's local
   storage, this step is skipped.
3. **Writes config.json** — sets `sandbox.mode` to `"microvm"` so dirge
   defaults to microVM isolation on every run.

### Choosing a different image

```bash
# Alpine (smaller, faster to boot)
dirge sandbox setup --image alpine

# Dev image (includes Rust, git, build-essential)
dirge sandbox setup --image dev

# Pull from Docker Hub instead of building locally
dirge sandbox setup --image docker.io/library/fedora:41
```

> **Build time:** The dev image includes Rust and takes ~5-10 minutes to build.
> Debian and Alpine build in ~30-60 seconds on a fast connection.

### Manual config.json

If you prefer to edit config by hand:

```json
{
  "sandbox": {
    "mode": "microvm",
    "image": "alpine",
    "cpus": 2,
    "memory_mib": 1024
  }
}
```

See [CONFIGURATION.md](CONFIGURATION.md) for all keys.

## First boot

```bash
dirge --sandbox microvm
```

On first run, dirge will:

1. **Prepare the rootfs** — exports the guest image from buildah, extracts
   layers into `~/.cache/dirge/microvm/<image>/base/`. This is cached — subsequent
   runs clone this base with `cp -r`.
2. **Generate SSH keys** — ephemeral ed25519 key pair for host→guest auth.
3. **Inject configuration** — writes `authorized_keys`, host keys, and
   `.krun_config.json` into the rootfs.
4. **Spawn the runner** — `dirge-microvm-runner` calls `krun_start_enter()`,
   which boots the VM and blocks until the guest exits.
5. **Wait for SSH** — polls `127.0.0.1:<port>` until sshd accepts connections
   (typically 500ms—2s on modern hardware).
6. **Execute** — every bash tool call from the agent runs as:
   `ssh sandbox@127.0.0.1 -p <port> "cd /workspace && <command>"`

The VM stays running for the entire dirge session. It shuts down when dirge
exits (the runner process is killed on drop).

## Troubleshooting

### "image not known" on first boot

`buildah push dirge-microvm:debian` fails because the image was never built.
Run `dirge sandbox setup` first, or build manually:

```bash
buildah bud --storage-driver vfs --tag dirge-microvm:debian -f images/debian/Dockerfile .
```

### "failed to spawn dirge-microvm-runner"

The runner binary wasn't found. Build it:

```bash
cargo build --release --features sandbox-microvm
# Runner is at target/release/dirge-microvm-runner
cp target/release/dirge-microvm-runner ~/.cargo/bin/
```

### "krun_start_enter failed"

libkrun couldn't boot the VM. Common causes:

- `/dev/kvm` permission denied — add your user to the `kvm` group: `sudo usermod -aG kvm $USER` (log out and back in)
- `libkrun.so` not found — install from [libkrun releases](https://github.com/containers/libkrun/releases)
- Missing CPU virtualization — enable VT-x/AMD-V in BIOS

### SSH timeout

The VM boots but sshd doesn't respond. Check:

- Is the guest image built with `openssh-server`? The default Debian/Alpine
  images include it. Custom images must install and enable sshd.
- Is port forwarding working? The runner maps `host:<port> → guest:22`.
  Verify with `ss -tlnp | grep <port>` — you should see a LISTEN on localhost.

### Slow first boot on btrfs/zfs

The rootfs clone uses `copy_file_range` which creates CoW reflinks on btrfs
and xfs — these are instant. On ext4 it falls back to a full file copy.
The first boot on ext4 may take a few seconds for the clone; subsequent
boots reuse the cache.
