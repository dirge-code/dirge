# MicroVM Sandbox — Security Model

## Isolation guarantees

The microVM provides **hardware-level isolation** through KVM. The guest
runs in a separate virtual address space with its own kernel, page tables,
and process tree.

| Attack vector | Host impact |
|--------------|-------------|
| `rm -rf /` inside VM | Guest rootfs destroyed; host untouched |
| Malicious binary execution | Contained in VM; can't escape to host |
| Fork bomb | Guest OOM; host unaffected |
| Kernel exploit in guest | Needs a KVM breakout to reach host |
| Resource exhaustion (CPU) | Shared host CPU; can slow host but not crash it |
| Resource exhaustion (memory) | Capped at `memory_mib`; VM OOMs, host survives |

## What is shared with the host

The following are deliberately shared — they're the bridge that makes
the sandbox useful:

### Workspace directory (virtio-fs)

Your project directory is mounted at `/workspace` inside the VM. A
compromised guest process can:

- **Read** any file in your workspace
- **Write** any file in your workspace (including `.git/`, `~/.ssh/`
  if your workspace is `$HOME`)
- **Delete** any file in your workspace

> **Mitigation:** Run dirge from a project-specific directory, not from
> `$HOME`. The workspace is the *current working directory* when dirge
> starts.

### Network (TSI)

The guest has **full outbound network access** through the host's network
stack. A compromised guest process can:

- Reach any host on the internet
- Access services on localhost (the host's loopback)
- Exfiltrate workspace files to a remote server

> **Mitigation:** There is currently **no network filtering**. This is
> tracked as `TODO(sandbox-net)` in the config module. Until domain/IP
> allowlisting is implemented, assume the VM can reach any network
> endpoint the host can.

### SSH port forward

The host binds a TCP listener on `127.0.0.1:<port>` that forwards to
guest port 22. This is bound to localhost only — remote hosts cannot
connect to the VM's SSH. The ephemeral SSH key is generated per session
and discarded on exit.

## What is NOT shared

- **Host filesystem** (except the workspace directory)
- **Host processes** (separate PID namespace)
- **Host devices** (except `/dev/kvm` through libkrun)
- **Host environment variables**
- **SSH agent, GPG agent, or other Unix sockets**
- **Host user's dotfiles** (`.bashrc`, `.gitconfig`, etc. — unless they're
  in the workspace directory)

## Current limitations

### No network filtering

The guest can connect to any host. A malicious `curl | sh` payload can
download and execute arbitrary code, then exfiltrate data. Until
allowlisting is implemented, treat the VM as having the same network
access as the host.

### No filesystem write protection

The workspace is mounted read-write. There is no mechanism to mark
specific files or directories as read-only inside the VM. The agent
can modify, delete, or encrypt any file in the workspace.

### No resource quotas (beyond memory_mib)

There are no CPU, disk I/O, or network bandwidth limits. A VM can
consume 100% of available host CPU.

### No snapshot integrity verification

Snapshots are directory copies. There is no checksumming or signing
to verify a snapshot hasn't been tampered with.

### SSH StrictModes disabled

The default images use `-o StrictModes=no` because virtio-fs maps the
workspace as root-owned. This relaxes sshd's permission checks on
`authorized_keys` and home directories. In practice this is low-risk
because only the ephemeral key can authenticate, and it's discarded
after the session.

## Comparison: bwrap vs microvm

| Property | bwrap (bubblewrap) | microvm (libkrun) |
|----------|-------------------|-------------------|
| Isolation type | Namespaces + seccomp | Hardware (KVM) |
| Kernel | Host kernel | Guest kernel |
| Memory overhead | ~0 MB | ~512 MB (configurable) |
| Boot time | Instant | ~500ms-2s |
| Network isolation | Can drop all caps | Full TSI (no filtering yet) |
| Filesystem isolation | Bind mounts, tmpfs | virtio-fs shared rootfs |
| Escape surface | Kernel vulns, misconfigured caps | KVM breakout, virtio bugs |
| Resource capping | cgroups (not configured) | memory_mib hard limit |

bwrap is faster and lighter but shares the host kernel. microvm is
slower to boot but provides a separate kernel — a kernel exploit in
the guest doesn't automatically compromise the host.

## Hardening roadmap

Planned improvements (tracked as `TODO(sandbox-net)` in the codebase):

1. **Domain/IP allowlisting** — restrict outbound connections to a
   configurable allowlist. Default: deny all, allow specific hosts.

2. **Read-only workspace option** — mount the workspace read-only so
   the agent can read files but not modify them.

3. **Network namespace isolation** — optionally run the VM without TSI,
   network-disabled, for air-gapped workloads.

4. **Snapshot signing** — sign snapshots with a host key so restored
   states can be verified.

5. **Seccomp profiles for the runner** — restrict the host-side runner
   process's syscalls beyond what libkrun already does.

## Threat model

The microVM is designed to protect against:

- **Accidental damage**: a buggy script that deletes files, a build
  that corrupts state, a tool that modifies the wrong directory.
  (Workspace is still writable, but host system files are safe.)

- **Untrusted dependencies**: a compromised npm package, PyPI package,
  or Cargo crate that tries to read/write outside the project directory.

- **Noisy neighbors**: a memory-hungry build that would OOM the host
  is contained to the VM's `memory_mib` limit.

It is NOT designed to protect against:

- **A malicious agent** that intentionally exfiltrates workspace files.
  The agent has legitimate SSH access — it can `curl` files to a remote
  server if it wants to.

- **KVM breakout exploits**. libkrun and KVM have a good security
  track record, but no hypervisor is bug-free.

- **Side-channel attacks** (cache timing, Spectre/Meltdown variants)
  from the guest to the host.
