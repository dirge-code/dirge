# MicroVM Sandbox — Custom Guest Images

The VM runs from an OCI image. You can customize the guest by writing your
own Dockerfile — add compilers, language runtimes, debugging tools, or
pre-installed packages.

## How images are built

Dirge uses [buildah](https://buildah.io/) to build images from Dockerfiles
in `images/<name>/Dockerfile`. The build command is:

```bash
buildah bud --storage-driver vfs --tag dirge-microvm:<name> -f images/<name>/Dockerfile .
```

The image is tagged `dirge-microvm:<name>` in buildah's local storage.
At boot time, dirge exports the image as an OCI archive and extracts
the rootfs into the cache directory.

## Creating a custom image

### Step 1: Create the Dockerfile

```
images/
├── debian/
│   └── Dockerfile
├── alpine/
│   └── Dockerfile
├── dev/
│   └── Dockerfile
└── my-image/          ← your custom image
    └── Dockerfile
```

### Step 2: Write the Dockerfile

Your image MUST include an SSH server. Dirge communicates with the VM
exclusively via SSH.

**Minimal template (Alpine-based):**

```dockerfile
FROM alpine:3.21

# /var/empty must exist for sshd's privilege separation
RUN mkdir -p /var/empty && chmod 755 /var/empty

# Install SSH and generate host keys
RUN apk add --no-cache openssh-server \
    && ssh-keygen -A

# Create the sandbox user (uid 1000)
RUN adduser -D -u 1000 sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh

# dirge's init script mounts /workspace — create the mount point
RUN mkdir -p /workspace

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
```

**Minimal template (Debian-based):**

```dockerfile
FROM debian:bookworm-slim

RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apt-get update \
    && apt-get install -y --no-install-recommends openssh-server \
    && rm -rf /var/lib/apt/lists/* \
    && ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh

RUN mkdir -p /workspace

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
```

### Step 3: Build and use

```bash
# Build the image
buildah bud --storage-driver vfs --tag dirge-microvm:my-image -f images/my-image/Dockerfile .

# Use it
dirge --sandbox microvm --microvm-image my-image
# or in config.json:
# { "sandbox": { "image": "my-image" } }
```

## Adding tools

### Example: Python development image

```dockerfile
FROM debian:bookworm-slim

RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        openssh-server \
        python3 python3-pip python3-venv \
        git curl \
    && rm -rf /var/lib/apt/lists/* \
    && ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh \
    && mkdir -p /workspace

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
```

### Example: Node.js image

```dockerfile
FROM debian:bookworm-slim

RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        openssh-server \
        curl gnupg \
    && rm -rf /var/lib/apt/lists/*

# Install Node.js 22 via NodeSource
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

RUN ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh \
    && mkdir -p /workspace

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
```

## Requirements and invariants

Every guest image must satisfy these. If any are missing, the VM won't
boot or SSH will fail.

| Requirement | Why |
|-------------|-----|
| `openssh-server` installed | dirge communicates exclusively via SSH |
| `sshd` configured with `-D -e` | Foreground mode with stderr logging |
| `sandbox` user (uid 1000) | SSH authenticates as this user |
| `/home/sandbox/.ssh/` with mode 0700 | dirge injects `authorized_keys` here |
| `/home/sandbox/` mode 0700 | sshd's StrictModes requires non-group-writable home |
| `/var/empty/` exists with mode 0755 | sshd privilege separation chroot |
| `/workspace/` directory exists | Mount point for virtio-fs workspace mirror |
| `PasswordAuthentication no` | Only key-based auth (dirge generates ephemeral keys) |
| `PermitRootLogin no` | Root login is never needed |

> **Note on StrictModes:** The default images use `-o StrictModes=no` in the
> init command because virtio-fs maps files as root-owned. If your image
> has the `sandbox` user owning their home directory inside the actual
> rootfs (not just at mount time), StrictModes can stay enabled. The
> built-in images use StrictModes=no to be safe.

## Networking assumptions

The guest has **full outbound network access** via TSI (Transparent Socket
Impersonation). This means:

- `apt install`, `pip install`, `npm install`, `cargo build` (downloading
  crates), `git clone`, `curl` — all work inside the VM
- There is **no network filtering** — the guest can reach any host
- The host's network is used transparently (the guest's connections appear
  to come from the host's IP)

If you need to restrict outbound access, configure firewall rules on the
host (the guest shares the host's network namespace via TSI).

## Size considerations

| Base | Compressed size | Extracted size | Boot time |
|------|----------------|----------------|-----------|
| Alpine | ~3 MB | ~10 MB | ~500ms |
| Debian slim | ~30 MB | ~80 MB | ~800ms |
| Debian + build-essential | ~150 MB | ~400 MB | ~1s |
| Debian + Rust (dev image) | ~400 MB | ~1.2 GB | ~2s |

Larger images take longer to extract on first boot (the cache clone is
fast regardless of size if you're on btrfs/xfs with reflink support).

## Pre-built images from Docker Hub

You can pull any image directly from Docker Hub (or any OCI registry)
without building locally:

```bash
# Fedora
dirge --sandbox microvm --microvm-image docker.io/library/fedora:41

# Ubuntu
dirge --sandbox microvm --microvm-image docker.io/library/ubuntu:24.04

# Arch
dirge --sandbox microvm --microvm-image docker.io/library/archlinux:latest
```

> **Caveat:** These images may not include an SSH server. You'll need to
> build a derived image that adds `openssh-server` and the sandbox user.
> The pure-Rust OCI puller downloads and caches layers automatically —
> no buildah needed for remote images.

## The built-in dev image

`images/dev/Dockerfile` is a Debian-based image with:

- `build-essential`, `pkg-config`, `libssl-dev` (C compilation)
- `git`, `curl`, `vim-tiny`
- Rust stable with `rustfmt`, `clippy`, and `rust-analyzer`

Build it with:
```bash
dirge sandbox setup --image dev
```

This takes 5-10 minutes on first build. The resulting image is ~1.2 GB
extracted. Use it when the agent needs to run `cargo build`, `cargo test`,
or other Rust tooling inside the VM.
