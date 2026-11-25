# How it works

## The checks before an unlock

Each check runs at most `TICK_TIMEOUT` seconds; then it is aborted and the
next one starts after `SLEEP_INTERVAL`.

1. **Healthcheck.** If `HEALTHCHECK_PORT` is open or `HEALTHCHECK_REMOTE_CMD`
   succeeds over SSH, the host has booted and there is nothing to do. With
   `INITRD_CHECKSUM_DIR`, the host's current baseline (`/etc/initrd-checksum/`)
   is copied to `INITRD_CHECKSUM_DIR/<host>/` when its fingerprint (sizes,
   modification times, signature) changed.
2. **SSH port.** If the port does not answer, the host is down. Through a
   jumphost, the jumphost connects and looks for the SSH banner.
3. **Initrd test.** Log in with the initrd host keys and test for
   `/etc/initrd-release`. A failed healthcheck alone never leads to an unlock.
   A host that answers but has no `/etc/initrd-release` is still booting or
   shutting down.
4. **Integrity.** With `INITRD_CHECKSUM_FILE`, verify the baseline's signature
   with `ssh-keygen -Y verify` against the host's regular host keys, then
   compare the running initrd with the baseline using the `initrd-checksum`
   helper. A missing baseline, a bad signature or a difference refuses the
   unlock.
5. **Unlock.** Send the passphrase with the configured method.

With `--ipv4` or `--ipv6`, the target is resolved once per check (through the
jumphost, if there is one) and SSH connects to that address while verifying
host keys under the host name.

## Unlock methods

| `LUKS_TYPE` | Aliases | How the passphrase is delivered |
| --- | --- | --- |
| `raw` | `direct` | Written to stdin of the SSH session; for a forced command such as dropbear's `cryptroot-unlock`. |
| `systemd` | `dracut`, `dracut-sshd`, `dracut-systemd`, `alt` | Typed into `systemd-tty-ask-password-agent` on a remote TTY, once the TTY has echo turned off. |
| `systemd-tool` | `arch` | `cryptsetup luksOpen` of the device in the `systemd-cryptsetup@` unit, then a restart of that unit. |
| `luks-mount` | | Written to stdin of [`luks-mount`](https://github.com/pschmitt/luks-mount.sh). |

## Initrd baselines

The host computes its baseline itself with `initrd-checksum check` and signs
it with its SSH host key (`ssh-keygen -Y sign -n initrd-checksum`). The NixOS
module does this on activation (`activationScript.enable`). The unlocker
copies the baseline while the host is up and uses it at the next boot.
Optionally, the host also pushes its signed baseline to the controller during
activation. The push has a deadline and failure does not block activation;
the normal polling fetch remains the fallback.

The measuring is done by `initrd-measure`, a static binary (x86_64 or
aarch64, picked by the target's `uname -m`). For a remote check,
`initrd-checksum` sends it over the SSH connection's stdin to a private
directory in `/run` and runs it there. It then removes it. Nothing else of the
target's userland measures: only its shell, `mkdir`, `cat` and `chmod` put
the binary in place.

- In the initrd (`/etc/initrd-release` exists) it hashes the live `/`: every
  regular file, plus (format 2) every symlink target and every directory on
  another file system. It skips `/dev`, `/proc`, `/sys`, `/run` and `/tmp`.
- On a booted host it hashes the initrd image: it unpacks the image in
  memory the way the kernel does (uncompressed, gzip and zstd segments in
  any order, hard links, later entries replacing earlier ones), so the
  image's measurement is what the initrd that boots from it shows.
- A short ignore list leaves out what differs at every boot: the machine ID,
  the initrd SSH host keys, initrd secrets, the early microcode,
  `/etc/resolv.conf`, and what the systemd initrd adds once the root file
  system is mounted (`/sysroot`, `/nixos-closure`, `/etc/switch-root.conf`).

A baseline in format 2 starts with `# initrd-measure format 2`; the checker
measures in the format of the baseline it compares against. Format 1 lists
regular files only and is what versions before the static measurer wrote.

What this does not protect against: the measurer runs on the system it
measures, so a target whose kernel or SSH server was replaced can show it a
clean initrd, for example by running it against a pristine copy. The kernel
and its command line are not measured either. The check makes tampering with
the initrd's files visible; it is not an attestation. Measured boot with a
TPM is the tool for that.

## What is logged and sent

- States (up, down, in initrd, unlocked, failed) are logged when they change,
  not on every check.
- Failed SSH logins, failed integrity checks, failed unlocks and successful
  unlocks are also sent as notifications. Status checks never notify.
- The passphrase never appears in logs, notifications or the process list,
  unless it is given with `--luks-passphrase`. Remote output is logged at
  debug level without the passphrase, terminal escapes or password masks.
- SSH runs with `BatchMode=yes`, so a missing key fails instead of prompting.
- Under systemd, log lines carry a syslog priority, so journald colors and
  filters them.
