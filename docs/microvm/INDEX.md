# MicroVM Sandbox

The microVM sandbox runs bash tool calls inside a hardware-isolated virtual machine
powered by [libkrun](https://github.com/containers/libkrun). Every `bash` invocation
executes via SSH inside a minimal Linux guest — the agent's workspace is mirrored
into the VM through virtio-fs, so file reads, writes, builds, and tests all happen
in true isolation.

**Key properties:**

- **Hardware isolation** — each VM is a separate KVM guest with its own kernel,
  memory, and process tree. A `rm -rf /` inside the VM can't touch the host.
- **Workspace mirroring** — the host project directory appears at `/workspace`
  inside the VM via virtio-fs. File changes are visible on both sides instantly.
- **Full outbound network** via TSI (Transparent Socket Impersonation) — the
  guest can `apt install`, `curl`, `git clone`, etc.
- **Snapshot/restore** — save and restore VM rootfs state between sessions.
- **Custom images** — build your own guest images with Dockerfiles to add
  compilers, language runtimes, or tools.

## Document index

| File | What it covers |
|------|----------------|
| [SETUP.md](SETUP.md) | Installing dependencies, building, first boot |
| [ARCHITECTURE.md](ARCHITECTURE.md) | How the pieces fit together (runner, SSH, rootfs, virtio-fs) |
| [CONFIGURATION.md](CONFIGURATION.md) | config.json keys, CLI flags, resource tuning |
| [CUSTOM_IMAGES.md](CUSTOM_IMAGES.md) | Building guest images from Dockerfiles, adding tools |
| [WORKSPACE.md](WORKSPACE.md) | How workspace mirroring works, permissions, caveats |
| [SLASH_COMMANDS.md](SLASH_COMMANDS.md) | `/sandbox attach`, snapshots, reboot |
| [SECURITY.md](SECURITY.md) | Security model, current limitations, hardening roadmap |

## Quick start

```bash
# 1. Check your system
dirge sandbox check

# 2. One-shot setup (builds the guest image, writes config)
dirge sandbox setup

# 3. Run dirge with the microVM sandbox
dirge --sandbox microvm
```

After step 3, every bash tool call runs inside the VM. You can verify with
`/sandbox attach` — it drops you into an interactive shell inside the guest.

## Supported guest images

| Variant | Base | Size | Good for |
|---------|------|------|----------|
| `debian` | Debian Bookworm Slim | ~80 MB | General purpose, apt available |
| `alpine` | Alpine 3.21 | ~10 MB | Minimal footprint, apk available |
| `dev` | Debian Bookworm + Rust | ~1.2 GB | Rust development inside the VM |
