# Changelog

## 0.2.0 (unreleased)

Rewritten in Rust. `luks-ssh-unlock.sh` and `dhcp-listener.py` are replaced
by one binary; `initrd-checksum.sh` stays a shell script because it runs on
the target. The command line, the environment variables, the NixOS module
options and the container interface are compatible.

### New

- `tui`: dashboard of all targets (`/etc/luks-ssh-unlock/*.env` by default)
  with live state, details, activity and the service journal, and confirmed
  unlocks and forced unlocks. The module's per-instance command gains `tui`.
- `status --json`, and a `status` exit status that tells whether the host is
  up or can be unlocked.
- `completions SHELL`; the Nix package installs bash, zsh and fish
  completions.
- `--healthcheck-port`, `--connect-timeout`, `--paranoid`,
  `--require-signature` and `--initrd-checksum-script` options for settings
  that were environment-only.
- Grouped `--help`; every option names its environment variable.
- Nix: crane build with a cached dependency derivation; `nix flake check`
  evaluates the NixOS module; `services.luks-ssh-unlock.package`.
- Container: static musl binary, native amd64/arm64 builds, provenance
  attestations and SBOM.

### Fixed

- `SSH_PORT` is now used for every SSH connection. Before, only scp and the
  port probe used it, so non-22 ports could never be unlocked.
- The jumphost's own SSH (name resolution, port probe) now uses
  `SSH_JUMPHOST_PORT`.
- An unset `LUKS_PASSPHRASE_FILE` defaulted to `-/run/secrets/…` (a stray
  `-`); it is `/run/secrets/luks_password_<host>` as documented.
- An unknown `LUKS_TYPE` reported a successful unlock without sending
  anything; it is now a configuration error.
- When Apprise was configured, mail notifications were never sent.
- Notification mails had no blank line between the headers and the body.
- `systemd-tool`: escaped unit names (`luks\x2d…`) were passed unquoted to the
  remote shell and used as the mapper name; the mapper name is now unescaped
  and every argument quoted. Several cryptsetup units no longer produce a
  multi-line device name.
- Inline known_hosts of several instances shared one file in `$TMPDIR`; each
  run now has a private scratch directory that is removed on exit.
- A failed initrd checksum was notified on every check; like other failures,
  it is now reported once per change.
- `dig +short` output starting with a CNAME made the jumphost probe connect
  to the CNAME's name.
- The container entrypoint failed when no msmtp configuration was mounted.
- While a host was up, its whole initrd baseline was copied on every check
  (about 150 KB every 30 s per target); it is now copied only when its
  fingerprint (sizes, modification times, signature) changes.
- The daily image rebuild ran every minute between 16:00 and 16:59.
- The module's `status` looked for a log message that was never written, so
  the last successful unlock was always "none recorded".

### Changed

- SSH runs with `BatchMode=yes`: a missing key fails instead of prompting.
- `--once` runs are also bounded by `TICK_TIMEOUT`.
- Boolean variables are off when `0`, `false`, `no` or `off` (before, any
  non-empty value was on).
- `SSH_HOSTNAME` is required; the `example.com` default is gone.
- SSH keys are only copied to a private 0400 file when their permissions are
  too open for OpenSSH.
- The jumphost's host key is checked against both known_hosts sets.
- Debug mail no longer includes the environment.
- The README is a short overview; the details moved to `docs/`.
- Status probes (CLI `status` and the dashboard) never send notifications.
  The dashboard skips the initrd checksum while probing; it always runs
  before an unlock.
