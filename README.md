# 🔐 luks-ssh-unlock

`luks-ssh-unlock` unlocks LUKS-encrypted hosts remotely. It watches a host,
and when the host waits in its initrd for the disk passphrase, it checks that
this is the initrd it expects (pinned SSH host keys, an optional checksum
baseline and its signature) before it sends the passphrase over SSH.

It runs as a service, as a one-shot command, or as an interactive dashboard
for all your hosts.

## ⚡️ Quick start

```bash
luks-ssh-unlock --host 10.0.0.5 --ssh-key ~/.ssh/id_ed25519 \
  --ssh-known-hosts-file ~/.ssh/known_hosts \
  --luks-passphrase-file /run/secrets/luks_password_10.0.0.5 \
  --healthcheck-cmd 'systemctl is-system-running --wait'
```

Without a subcommand it watches the host forever (`run`). The subcommands:

| Command | What it does |
| --- | --- |
| `run` | Watch the target and unlock it whenever it waits in its initrd (default). With `--once`, make one attempt; the exit status tells whether the host is up. |
| `status [--json]` | Show the configuration and what the target is doing right now (booted, waiting in initrd, unreachable, …) without sending anything. Exit status 0 if the host is up or can be unlocked. |
| `tui [FILES…]` | Interactive dashboard of all targets (see below). |
| `completions SHELL` | Shell completions for bash, zsh, fish, elvish or PowerShell. |

Add `--debug` for verbose logs.

## 🖥️ Dashboard

```bash
sudo luks-ssh-unlock tui                    # every /etc/luks-ssh-unlock/*.env
luks-ssh-unlock tui hosts/*.env --refresh 10
luks-ssh-unlock tui --host 10.0.0.5 …       # one target from the options
```

The dashboard lists every target with its live state, the details of the
selected one, and its activity. Keys:

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Select a target |
| `r` / `R` | Check the selected / every target now |
| `u` or `⏎` | Unlock the selected target (asks first; all checks still apply) |
| `F` | Force unlock: skips the checksum and host-key checks (asks first) |
| `l` or `⇥` | Switch between the activity log and the service's journal |
| `q` | Quit |

Targets are described by environment files (`KEY=value`, systemd
`EnvironmentFile=` syntax), the same files the NixOS module writes. Reading
SSH keys and passphrases usually needs root.

## 🛡️ How an unlock is decided

Each check (every `--interval` seconds, aborted after `--tick-timeout`):

1. **Healthcheck.** If `HEALTHCHECK_PORT` is open or `HEALTHCHECK_REMOTE_CMD`
   succeeds over SSH, the host is up: nothing to do. When
   `INITRD_CHECKSUM_DIR` is set, the host's current checksum baseline
   (`/etc/initrd-checksum/`) is copied to `INITRD_CHECKSUM_DIR/<host>/`.
2. **SSH port.** If the port does not answer, the host is down.
3. **Initrd test.** Log in with the initrd host keys and test for
   `/etc/initrd-release`. A failed healthcheck alone never leads to an
   unlock.
4. **Integrity.** With `INITRD_CHECKSUM_FILE`, verify the baseline's
   `ssh-keygen -Y` signature (made by the host with its regular host key)
   and compare the running initrd with it, using the `initrd-checksum`
   helper. A mismatch refuses the unlock.
5. **Unlock.** Send the passphrase with the configured method.

States are logged once when they change, not on every check. Failures
(SSH login, integrity, unlock) are also sent as notifications.

LUKS methods (`--luks-type`):

| Method | Aliases | How |
| --- | --- | --- |
| `raw` | `direct` | Write the passphrase to the SSH session (dropbear, `cryptroot-unlock` as forced command). |
| `systemd` | `dracut`, `dracut-sshd`, `dracut-systemd`, `alt` | Answer `systemd-tty-ask-password-agent` on a remote TTY, once echo is off. |
| `systemd-tool` | `arch` | `cryptsetup luksOpen` the device of the `systemd-cryptsetup@` unit, then restart that unit. |
| `luks-mount` | | Pipe the passphrase into [`luks-mount`](https://github.com/pschmitt/luks-mount.sh). |

The passphrase never appears in logs, notifications or the process list
(unless you pass `--luks-passphrase` yourself). Remote output is logged at
debug level with the passphrase, terminal escapes and password masks
removed. SSH runs in batch mode, so a missing key fails instead of
prompting.

## 🧩 Options and environment variables

Every option can also be set through its environment variable. The command
line wins. Boolean variables are off when empty, `0`, `false`, `no` or
`off`.

| Category | Variable | Default | Flags | Description |
| --- | --- | --- | --- | --- |
| General | `DEBUG` | off | `--debug`, `-D` | Debug logging. |
| General | `RUN_ONCE` | off | `--once` | One attempt, then exit. |
| General | `FORCE` | off | `--force`, `-f` | Skip the checksum/signature and host-key checks (needs `--once`). Client-key authentication still applies. |
| General | `SLEEP_INTERVAL` | `10` | `--interval`, `--sleep`, `-i` | Seconds between checks. |
| General | `TICK_TIMEOUT` | `120` | `--tick-timeout`, `--timeout` | Abort a check that takes longer. |
| General | `EVENTS_FILE` | | `--events-file`, `-e` | Append notification events to this file. |
| SSH | `SSH_HOSTNAME` | (required) | `--host`, `--ssh-host`, `-H` | Target host. |
| SSH | `SSH_PORT` | `22` | `--port`, `--ssh-port`, `-P` | SSH port. |
| SSH | `SSH_USERNAME` | `root` | `--username`, `--user`, `-u` | SSH user. |
| SSH | `SSH_KEY` | `/run/secrets/ssh_key` | `--ssh-key`, `--key`, `-k` | Private key. Copied to a private 0400 file if its permissions are too open for OpenSSH. |
| SSH | `SSH_CONNECTION_TIMEOUT` | `5` | `--connect-timeout` | SSH connect timeout in seconds. |
| SSH | `SSH_KNOWN_HOSTS` / `SSH_KNOWN_HOSTS_FILE` | | `--ssh-known-hosts[-file]` | Host keys of the booted system (inline or file). Also the allowed signers of the baseline signature. |
| SSH | `SSH_INITRD_KNOWN_HOSTS` / `SSH_INITRD_KNOWN_HOSTS_FILE` | | `--ssh-initrd-known-hosts[-file]` | Host keys of the initrd SSH server. |
| SSH | `FORCE_IPV4` / `FORCE_IPV6` | off | `-4` / `-6` | Use one address family; the target is resolved once per check and pinned. |
| SSH | `SKIP_SSH_PORT_CHECK` | off | `--skip-ssh-port-check` | Skip the TCP probe of the SSH port. |
| SSH | `SSH_CONNECT_ADDRESS` / `SSH_HOSTKEY_ALIAS` | | | Connect to this address but verify host keys under this name (set by the DHCP listener). |
| Jumphost | `SSH_JUMPHOST` | | `--jumphost`, `-J` | Reach the target through this host. |
| Jumphost | `SSH_JUMPHOST_USERNAME` | `root` | `--jumphost-username`, `-U` | Jumphost user. |
| Jumphost | `SSH_JUMPHOST_PORT` | the SSH port | `--jumphost-port`, `--jport` | Jumphost port. |
| Jumphost | `SSH_JUMPHOST_KEY` | the SSH key | `--jumphost-key`, `-K` | Jumphost key. Its host key is checked against both known_hosts sets. |
| LUKS | `LUKS_TYPE` | `raw` | `--luks-type`, `-t` | Unlock method, see above. |
| LUKS | `LUKS_PASSPHRASE_FILE` | `/run/secrets/luks_password_<host>` | `--luks-passphrase-file`, `-F` | Passphrase file. Empty disables the default. A systemd credential `luks-passphrase` takes precedence. |
| LUKS | `LUKS_PASSPHRASE` | | `--luks-passphrase`, `-p` | The passphrase itself (insecure on the command line). |
| Initrd validation | `INITRD_CHECKSUM_FILE` | | `--initrd-checksum-file` | Baseline the running initrd must match. |
| Initrd validation | `INITRD_CHECKSUM_DIR` | | `--initrd-checksum-dir` | Where baselines fetched from the booted host are stored. |
| Initrd validation | `INITRD_CHECKSUM_SCRIPT` | `initrd-checksum` next to the binary | `--initrd-checksum-script` | The checksum helper. |
| Initrd validation | `INITRD_CHECKSUM_BUSYBOX_DIR` | packaged bundle | | BusyBox bundle for paranoid mode. |
| Initrd validation | `PARANOID` | off | `--paranoid` | Hash with a minimal BusyBox bundle uploaded to the target. |
| Initrd validation | `INITRD_CHECKSUM_REQUIRE_SIGNATURE` | off | `--require-signature` | Refuse unless the baseline has a valid signature. |
| Healthcheck | `HEALTHCHECK_PORT` | | `--healthcheck-port` | TCP port that only opens once the host has booted. |
| Healthcheck | `HEALTHCHECK_REMOTE_CMD` | | `--healthcheck-cmd`, `--remote-check`, `--rcmd` | Command that only succeeds on the booted system. |
| Healthcheck | `HEALTHCHECK_REMOTE_HOSTNAME` | the target | `--healthcheck-host`, `--hc-host` | Host to run it on. |
| Healthcheck | `HEALTHCHECK_REMOTE_USERNAME` | the SSH user | `--healthcheck-user`, `--hc-user` | User to run it as. |
| Healthcheck | `SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE` | `default` | `--healthcheck-known-hosts-type` | Host keys for the healthcheck: `default` or `initrd`. |
| Notifications | `APPRISE_URL` | | `--apprise-url`, `-a` | Apprise API endpoint. |
| Notifications | `APPRISE_TAG` | | `--apprise-tag` | Apprise tag. |
| Notifications | `APPRISE_TITLE` | | `--apprise-title` | Title; see placeholders below. |
| Notifications | `EMAIL_RECIPIENT` | | `--email-recipient`, `--email-to` | Mail notifications to this address. |
| Notifications | `EMAIL_FROM` | | `--email-from` | Sender address. |
| Notifications | `EMAIL_SUBJECT` | `luks-ssh-unlock: <host> <event>` | `--email-subject` | Subject; see placeholders below. |
| Notifications | `MSMTP_ACCOUNT` | | `--msmtp-email-account` | Send with `msmtp -a ACCOUNT` instead of `sendmail`. |
| Runtime | `TMPDIR` | `/tmp` | | Where the private scratch directory is created. |

Titles and subjects may contain `#hostname`, `#username`, `#port`,
`#jumphost`, `#jusername`, `#jport`, `#luks_type`, `#event`, `#event_type`,
`#sleep_interval`, `#remote_cmd`, `#remote_hostname` and `#remote_username`.

## ❄️ Nix

```bash
nix build .#luks-ssh-unlock   # the binary, initrd-checksum, the DHCP listener, completions
nix develop                   # cargo, clippy, rustfmt, cargo-deny, nixfmt, …
nix flake check               # package build with unit tests, NixOS module check
```

The package is built with [crane](https://github.com/ipetkov/crane): its
dependencies are a derivation of their own, so source changes and version
bumps reuse them from the cache.

### NixOS module

Import `nixosModules.default` from this flake; it uses the flake's package
(`services.luks-ssh-unlock.package`).

- `services.luks-ssh-unlock.instances.<name>` defines a target. Each instance
  gets an environment file `/etc/luks-ssh-unlock/<name>.env` and a service
  `luks-ssh-unlock-<name>`. `passphraseFile` is passed as a systemd
  credential.
- Each instance also gets a `luks-ssh-unlock-<name>` command (it runs sudo
  itself when needed): no argument makes one validated unlock attempt;
  `--force`/`-f` skips the checksum/signature and host-key checks; `status`
  shows the target, the service and its recent logs; `tui` opens the
  dashboard; `logs`/`l` follows the journal; `start`/`stop` control the
  service.
- `dhcpListener.enable` watches DHCP on an interface and starts a one-shot
  unlock as soon as the target's initrd gets a lease and answers on SSH
  (needs `CAP_NET_RAW`, which the unit grants).
- `activationScript.enable` computes the initrd checksum baseline of the host
  itself on activation and signs it with its SSH host key, so that an
  unlocker elsewhere can verify it. Enable `initrdCheck.requireSignature` on
  the unlocker once every target has a signed baseline.

See `nix/module.nix` for every option.

## 🐳 Container

```bash
docker run --rm \
  -v /path/to/secrets:/run/secrets:ro \
  -e SSH_HOSTNAME=10.0.0.5 \
  -e SSH_KEY=/run/secrets/id_ed25519 \
  -e SSH_KNOWN_HOSTS_FILE=/run/secrets/known_hosts \
  -e LUKS_PASSPHRASE_FILE=/run/secrets/luks_password_10.0.0.5 \
  ghcr.io/pschmitt/luks-ssh-unlock:latest
```

Images: `ghcr.io/pschmitt/luks-ssh-unlock` and
`docker.io/pschmitt/luks-ssh-unlock` (amd64, arm64). An msmtp configuration
mounted at `/config/msmtprc` is installed for mail notifications.
`docker-compose.yaml` shows several targets with health checks,
notifications, a jumphost and initrd validation.

## 🤝 Development

```bash
nix develop -c cargo test
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt
nix develop -c cargo deny check
shellcheck initrd-checksum.sh entrypoint.sh
```

`initrd-checksum.sh` stays a shell script: it copies itself to the target
and runs there, in initrds that have nothing but a shell.

## 📜 License

GNU General Public License v3.0. See [LICENSE](LICENSE).
