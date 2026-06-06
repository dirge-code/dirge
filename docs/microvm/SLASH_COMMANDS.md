# MicroVM Sandbox — Slash Commands

The `/sandbox` command manages the microVM from within dirge's TUI.

## `/sandbox help`

Prints available subcommands:

```
sandbox commands:
  /sandbox attach           —   shell into the microVM
  /sandbox reboot/start     —   boot/restart the microVM
  /sandbox snapshot save <name>   —   save VM state
  /sandbox snapshot list          —   list saved snapshots
  /sandbox snapshot restore <name> —  restore (VM must be stopped)
  /sandbox snapshot delete <name>  —  delete a snapshot
```

> `/sandbox ssh` is a hidden alias for `/sandbox attach`.

## `/sandbox attach`

Drops you into an interactive shell inside the VM.

```
/sandbox attach
```

What happens under the hood:

1. **Stop the input reader** — crossterm's raw-mode event reader is paused
   so it doesn't race for stdin bytes with the SSH session.

2. **Suspend the TUI** — leaves the alt screen, disables mouse reporting
   and bracketed paste. Keeps raw mode so readline/emacs bindings work.

3. **Drain buffered stdin** — captures keystrokes that arrived during
   the suspend window. These are reinjected into the PTY after the SSH
   session starts, so no typed characters are lost.

4. **Spawn SSH on a PTY** — `ssh -t sandbox@127.0.0.1 -p <port>` with its
   stdio attached to a pseudo-terminal pair. The `-t` flag forces PTY
   allocation so readline, colors, and interactive programs work correctly.

5. **Relay I/O** — a dedicated thread copies bytes between the PTY primary
   and `/dev/tty`. Priority is set to `-10` (highest CFS) so the relay
   doesn't starve.

6. **On exit** — restarts the input reader, restores the TUI, reinjects
   drained keystrokes.

### Tips

- Run `exit` or press Ctrl-D to leave the VM shell.
- The shell starts in `/workspace` (your project directory).
- Your host `TERM` variable is forwarded.
- Ctrl-C, Ctrl-Z, and job control work inside the VM.
- The VM stays running after you exit — subsequent bash tool calls reuse it.

### Troubleshooting attach

**"VM not running yet"** — run any bash command first to boot the VM, then
attach. The VM starts lazily on the first bash tool call.

**"microVM sandbox not active"** — you started dirge without `--sandbox microvm`.
Restart dirge with the flag or set `sandbox.mode` in config.json.

**"SSH pre-flight failed"** — the SSH connection is misconfigured. The error
message includes the exact `ssh` command to try manually for debugging.

## `/sandbox reboot` (alias: `/sandbox start`)

Stops the current VM and starts a fresh one.

```
/sandbox reboot
```

This re-clones the rootfs from the cached base, discarding any in-VM
changes (installed packages, modified configs, files in `/tmp`).

Use this when:
- The VM is in a bad state (hung processes, full disk).
- You want to switch to a snapshot-restored rootfs.
- You changed the image config and want to pick it up.

> Workspace files are unaffected — only the VM's rootfs is reset.

## Snapshots

Snapshots save and restore the VM's rootfs state. They're stored as
directory copies under `<cache_dir>/snapshots/<name>/`.

### `/sandbox snapshot save <name>`

Saves the current VM's rootfs. The VM must be running.

```
/sandbox snapshot save before-deps-upgrade
```

All installed packages, modified configs, and files in the VM's rootfs
are captured. Workspace files are NOT included (they're already on the host).

> Snapshot names must match `[a-zA-Z0-9._-]+`.

### `/sandbox snapshot list`

Lists saved snapshots.

```
/sandbox snapshot list
```

### `/sandbox snapshot restore <name>`

Replaces the cached base rootfs with the snapshot. The VM must be stopped
(reboot first if it's running).

```
/sandbox snapshot restore before-deps-upgrade
/sandbox reboot  # starts fresh from the restored snapshot
```

> This overwrites the cached base — the snapshot becomes the new template
> for subsequent sessions and reboots.

### `/sandbox snapshot delete <name>`

Deletes a saved snapshot.

```
/sandbox snapshot delete old-experiment
```

## Common workflows

### Testing a risky command safely

```
/sandbox snapshot save safe-point
# Agent runs a potentially destructive command...
# If something breaks:
/sandbox snapshot restore safe-point
/sandbox reboot
```

### Pinning a known-good environment

```
# Build an image with all your dependencies
buildah bud --storage-driver vfs --tag dirge-microvm:my-env -f images/my-env/Dockerfile .
dirge --microvm-image my-env

# Or after installing packages interactively:
/sandbox attach
$ sudo apt install ...   # inside the VM
$ exit
/sandbox snapshot save with-deps
```

### Switching between images mid-session

```
# Config/CLI changes take effect on next reboot:
/sandbox reboot
```

The reboot picks up the current `config.image` setting.
