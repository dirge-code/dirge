# MicroVM Sandbox — Configuration

## config.json

All keys live under `"sandbox"` in dirge's `config.json`
(`~/.config/dirge/config.json`).

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

### Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mode` | string | `"off"` | `"off"`, `"bwrap"`, or `"microvm"` |
| `image` | string | `"debian"` | Image reference (see below) |
| `cpus` | u8 | `1` | Number of vCPUs for the VM |
| `memory_mib` | u32 | `512` | RAM in MiB |

### Backward compatibility

The old nested form is still accepted (and transparently flattened):

```json
{
  "sandbox": {
    "mode": "microvm",
    "microvm": {
      "image": "alpine",
      "cpus": 2
    }
  }
}
```

Similarly, a top-level `"microvm_image"` key works as a fallback when
`sandbox.image` is not set.

## Image references

### Shorthand names (local images)

Bare names without `/` or `://` resolve to `local://dirge-microvm:<name>`:

```json
{ "sandbox": { "image": "alpine" } }
// → local://dirge-microvm:alpine
```

Built-in variants: `debian`, `alpine`, `dev`. See
[CUSTOM_IMAGES.md](CUSTOM_IMAGES.md) for adding your own.

### Full local references

```json
{ "sandbox": { "image": "local://my-custom-image:latest" } }
```

The image must exist in buildah's local storage (`buildah images`).

### Docker Hub and other registries

Uses the pure-Rust OCI puller (no buildah needed):

```json
{ "sandbox": { "image": "docker.io/library/fedora:41" } }
{ "sandbox": { "image": "ghcr.io/owner/repo:tag" } }
```

> Private registries: set the `REGISTRY_TOKEN` environment variable for
> bearer auth. Docker Hub public images don't need authentication.

## CLI flags

CLI flags override config.json values:

```bash
# Enable microVM sandbox
dirge --sandbox microvm

# Also valid (explicit "off" for bwrap)
dirge --sandbox          # defaults to "none" → Off
dirge --sandbox bwrap    # bubblewrap
dirge --sandbox microvm  # microVM
dirge --sandbox off      # no sandbox
```

```bash
# Override the guest image
dirge --sandbox microvm --microvm-image alpine

# Pull from Docker Hub
dirge --sandbox microvm --microvm-image docker.io/library/ubuntu:24.04
```

### Full CLI reference

```
--sandbox [<MODE>]
    Run bash in an isolated sandbox:
    'bwrap' (bubblewrap), 'microvm' (hardware VM via libkrun),
    or 'none' (default, no sandbox)

--microvm-image <IMAGE>
    OCI image or local reference for the microVM sandbox
    (e.g. 'docker.io/library/alpine:3.21', 'local://my-image:tag')
```

### `dirge sandbox` subcommand

```
dirge sandbox check
    Print a report of sandbox dependencies

dirge sandbox setup [--image <IMAGE>]
    Check deps, update config.json, pre-pull/build OCI image
```

## Resource tuning

### vCPUs

More vCPUs help with parallel builds (`cargo build -j4`, `make -j`).
One vCPU is fine for shell scripts and single-threaded tools.

```json
{ "sandbox": { "cpus": 4 } }
```

> The host's CPU is shared — the VM doesn't get dedicated cores. Setting
> `cpus` higher than the host's physical core count is harmless but won't
> improve throughput.

### Memory

512 MiB is enough for most shell use. Bump to 1024-2048 MiB for `rustc`
(which needs ~500 MiB per invocation) or memory-hungry test suites.

```json
{ "sandbox": { "memory_mib": 2048 } }
```

> libkrun uses memory overcommit — the VM's RAM isn't pre-allocated. The
> guest sees `memory_mib` as available RAM but the host only allocates
> pages as they're touched.

### File descriptor limits

The runner raises `RLIMIT_NOFILE` to the hard limit before boot, and
libkrun propagates this to the guest. TSI (network) and virtio-fs each
consume host file descriptors per guest operation. Under heavy workloads
(parallel downloads, large builds), the default 1024 soft limit is
insufficient. The `images/*/Dockerfile` images set `* - nofile 1048576`
in `/etc/security/limits.conf` as a guest-side safety net.

## Cache directory

Rootfs images are cached at `~/.cache/dirge/microvm/` (or `$XDG_CACHE_HOME/dirge/microvm/`).

Layout:
```
~/.cache/dirge/microvm/
├── local_docker.io_library_debian_bookworm-20250224-slim/
│   ├── .lock                ← advisory lock (serializes concurrent pulls)
│   ├── .staging/            ← new rootfs extracted here, then atomically renamed to base/
│   ├── base/                ← extracted rootfs (shared, read-only template)
│   └── session-12345/       ← per-session clone (deleted on exit)
├── local_dirge-microvm_alpine/
│   ├── .lock
│   ├── .staging/
│   ├── base/
│   └── session-12346/
├── blobs/                   ← OCI layer cache (for remote images)
│   └── sha256/
│       └── abcd1234...
└── snapshots/               ← saved VM states
    └── before-risky-change/
```

To clear the cache and force a fresh pull:
```bash
rm -rf ~/.cache/dirge/microvm/
```

The next boot will re-extract the image. This is safe — only the rootfs
cache is affected. Your workspace and snapshots are untouched.
