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

`initrd-checksum` copies itself to the target and runs there. In paranoid
mode it first uploads a static BusyBox bundle to `/run/initrd-checksum/bin`
and uses only those tools.

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
