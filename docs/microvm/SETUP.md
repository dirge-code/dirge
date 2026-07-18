# MicroVM Sandbox — Setup

This guide walks through installing the prerequisites, building the runner
binary, and getting a microVM booted.

## Prerequisites

### Required (hard blockers)

| Dependency | Linux | macOS |
|-----------|-------|-------|
| Hardware virtualization | `/dev/kvm` (load `kvm` module, user in `kvm` group) | Hypervisor.framework (built-in on Apple Silicon) |
| VM runtime | `libkrun.so` + `libkrunfw.so` (see [libkrun releases](https://github.com/containers/libkrun/releases)) | `libkrun.dylib` + `libkrunfw.5.dylib` (`brew install libkrun libkrunfw`) |
| Image building (optional) | `buildah` (`apt install buildah`) | Not needed — uses built-in OCI puller |
| OCI layer extraction | `gzip` + `tar` | `gzip` + `tar` (pre-installed) |
| SSH key generation | `ssh-keygen` (`apt install openssh-client`) | `ssh-keygen` (pre-installed) |
| Runner binary | `dirge-microvm-runner` (built alongside dirge) | Same binary, auto-codesigned at build |

### Optional

| Dependency | When needed | Linux | macOS |
|-----------|-------------|-------|-------|
| `buildah` | Using `local://` images (the default) | `apt install buildah` | Not available — use `dirge sandbox setup --image docker.io/...` to pull remote images |
| `mold` linker | Faster rebuilds of the runner | `apt install mold` | Not needed (Xcode linker is fast enough) |

## Check your system

```bash
dirge sandbox check
```

This prints a report of every dependency and its status. Fix any `ERROR` items
before proceeding. `WARN` items (buildah, mold) are optional.

### What the check covers

1. Hardware virtualization — `/dev/kvm` exists and accessible (Linux) or `sysctl kern.hv_support` → 1 (macOS)
2. `libkrun` — found via platform-appropriate paths
3. `libkrunfw` — same
4. `gzip` — on PATH
5. `tar` — on PATH
6. `ssh-keygen` — on PATH
7. `dirge-microvm-runner` — binary found adjacent to dirge or on PATH
8. `buildah` — optional, warns if missing
9. `mold` — optional, warns if missing (Linux only)

## Building

### Build dirge with the sandbox-microvm feature

```bash
cargo build --release --features sandbox-microvm
```

This compiles both `dirge` and `dirge-microvm-runner`. The runner binary lands
at `target/release/dirge-microvm-runner`. dirge finds it by looking next to its
own binary or on `$PATH`.

> **Caveat:** If libkrun (`libkrun.so` on Linux, `libkrun.dylib` on macOS) is not installed, the runner
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

1. **Prepare the rootfs** — exports the guest image (via buildah on Linux, via OCI puller on macOS), extracts
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

The guest image hasn't been built or pulled yet. Run `dirge sandbox setup` first:

```bash
# Linux: builds locally via buildah
dirge sandbox setup

# macOS: pulls from the network via built-in OCI puller
dirge sandbox setup --image docker.io/library/alpine:latest
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

- **Linux**: `/dev/kvm` permission denied — add your user to the `kvm` group: `sudo usermod -aG kvm $USER` (log out and back in)
- **macOS**: runner not codesigned — rebuild with `cargo build --features sandbox-microvm`; the sandbox automatically codesigns on first use via `ensure_runner_signed()`. To force sign at build time: `codesign --force --sign - --entitlements dirge.entitlements target/release/dirge-microvm-runner`. Verify with `codesign -d --entitlements - target/release/dirge-microvm-runner | grep hypervisor`
- libkrun library not found — install from [libkrun releases](https://github.com/containers/libkrun/releases) (Linux) or `brew install libkrun libkrunfw` (macOS)
- Missing CPU virtualization — enable VT-x/AMD-V in BIOS / Apple Silicon has it always on

### SSH timeout

The VM boots but sshd doesn't respond. Check:

- Is the guest image built with `openssh-server`? The default Debian/Alpine
  images include it. Custom images must install and enable sshd.
- Is port forwarding working? The runner maps `host:<port> → guest:22`.
  Verify with `ss -tlnp | grep <port>` (Linux) or `lsof -i -P | grep LISTEN` (macOS) — you should see a LISTEN on localhost.

### Slow first boot on btrfs/zfs

The rootfs clone uses `copy_file_range` which creates CoW reflinks on btrfs
and xfs — these are instant. On ext4 it falls back to a full file copy.
The first boot on ext4 may take a few seconds for the clone; subsequent
boots reuse the cache.
