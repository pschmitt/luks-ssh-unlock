# 🔐 luks-ssh-unlock

`luks-ssh-unlock` unlocks remote disks by SSHing into a host's initrd or live system and feeding the LUKS passphrase automatically. It supports optional jumphosts, host key pinning, health checks, notifications, and initrd integrity validation.

## ⚡️ Quick start
- Provide the target host, SSH key, and a passphrase or passphrase file.
- Run the main script directly:
  ```bash
  ./luks-ssh-unlock.sh --host 10.0.0.5 --ssh-key ~/.ssh/id_ed25519 \
    --luks-passphrase-file /run/secrets/luks_password_10.0.0.5
  ```
- For verbose logging, add `--debug`.

## 🧭 CLI reference
The main entry point is `luks-ssh-unlock.sh`. It exposes flags for SSH connectivity, jumphosts, LUKS parameters, notifications, health checks, and initrd validation. See `--help` for the full list of options.

## 🧩 Environment variables and flags

| Category | Environment variable | Default | Related flags | Description |
| --- | --- | --- | --- | --- |
| SSH | `SSH_HOSTNAME` | `example.com` | `--host`, `--ssh-host`, `-H` | Target host to unlock over SSH. |
| SSH | `SSH_PORT` | `22` | `--port`, `--ssh-port` | SSH port. |
| SSH | `SSH_USERNAME` | `root` | `--username`, `--user`, `-u` | SSH login user. |
| SSH | `SSH_KEY` | `/run/secrets/ssh_key` | `--ssh-key`, `--key`, `--private-key`, `--pkey` | Private key for authentication. |
| SSH | `SSH_CONNECTION_TIMEOUT` | `5` | _n/a_ | Connection timeout in seconds. |
| SSH | `SSH_KNOWN_HOSTS` | _(empty)_ | `--ssh-known-hosts` | Inline `known_hosts` content to pin the target host keys. |
| SSH | `SSH_KNOWN_HOSTS_FILE` | _(empty)_ | `--ssh-known-hosts-file` | Path to `known_hosts` enforcing host keys. |
| SSH | `SSH_INITRD_KNOWN_HOSTS` | _(empty)_ | `--ssh-initrd-known-hosts` | `known_hosts` content specifically for initrd host keys. |
| SSH | `SSH_INITRD_KNOWN_HOSTS_FILE` | _(empty)_ | `--ssh-initrd-known-hosts-file` | File containing initrd-only host keys. |
| SSH | `FORCE_IPV4` | _(empty)_ | `--force-ipv4`, `--ipv4`, `-4` | Prefer IPv4. |
| SSH | `FORCE_IPV6` | _(empty)_ | `--force-ipv6`, `--ipv6`, `-6` | Prefer IPv6. |
| Jumphost | `SSH_JUMPHOST` | _(empty)_ | `--ssh-jumphost`, `--jumphost`, `-J` | Optional bastion to reach the target. |
| Jumphost | `SSH_JUMPHOST_USERNAME` | `root` | `--ssh-jumphost-username`, `--jusername`, `--ju`, `-U` | Jumphost SSH user. |
| Jumphost | `SSH_JUMPHOST_PORT` | Inherits `SSH_PORT` | `--ssh-jumphost-port`, `--jport`, `--jp` | Jumphost SSH port. |
| Jumphost | `SSH_JUMPHOST_KEY` | Inherits `SSH_KEY` | `--ssh-jumphost-key`, `--jkey`, `--jk`, `-K` | Jumphost SSH key. |
| LUKS | `LUKS_TYPE` | `raw` | `--luks-type`, `--type`, `-t` | Unlock strategy (`raw`, `systemd`, `systemd-tool`, `luks-mount`). |
| LUKS | `LUKS_PASSPHRASE` | _(empty)_ | `--luks-passphrase`, `--luks-password`, `--password`, `-p` | Plaintext passphrase. |
| LUKS | `LUKS_PASSPHRASE_FILE` | `/run/secrets/luks_password_<host>` | `--luks-passphrase-file`, `--luks-password-file`, `-F` | File containing the passphrase. |
| Initrd validation | `INITRD_CHECKSUM_FILE` | _(empty)_ | `--initrd-checksum-file` | Expected checksum snapshot to verify before unlocking. |
| Initrd validation | `INITRD_CHECKSUM_DIR` | _(empty)_ | `--initrd-checksum-dir` | Directory to store fetched checksum snapshots. |
| Initrd validation | `INITRD_CHECKSUM_SCRIPT` | `/app/bin/initrd-checksum` | _n/a_ | Helper used to collect initrd hashes. |
| Initrd validation | `PARANOID` | _(empty)_ | `PARANOID=1` / `--paranoid` | Hardened initrd-checksum mode using a minimal remote bundle. |
| Health checks | `HEALTHCHECK_PORT` | _(empty)_ | _n/a_ | Optional TCP port probe before attempting unlock. |
| Health checks | `HEALTHCHECK_REMOTE_CMD` | _(empty)_ | `--remote-check`, `--healthcheck-remote-cmd`, `--remote-command`, `--remote-cmd`, `--rcmd` | Remote command used to confirm reachability. |
| Health checks | `HEALTHCHECK_REMOTE_HOSTNAME` | _(empty)_ | `--healthcheck-host`, `--hc-host` | Hostname used by the remote check. |
| Health checks | `HEALTHCHECK_REMOTE_USERNAME` | _(empty)_ | `--healthcheck-user`, `--hc-user` | SSH user for the remote check. |
| Health checks | `SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE` | `default` | `--healthcheck-known-hosts-type` | Which host key set to use for health-check SSH (`default` or `initrd`). |
| Notifications | `APPRISE_URL` | _(empty)_ | `--apprise-url`, `--apprise`, `-a` | Apprise endpoint for push notifications. |
| Notifications | `APPRISE_TAG` | _(empty)_ | `--apprise-tag`, `--tag` | Apprise tag value. |
| Notifications | `APPRISE_TITLE` | _(empty)_ | `--apprise-title`, `--title` | Notification title. |
| Email | `MSMTP_ACCOUNT` | _(empty)_ | `--msmtp-email-account`, `--email-account` | Named msmtp account to use when sending mail. |
| Email | `EMAIL_RECIPIENT` | _(empty)_ | `--email-recipient`, `--email-to` | Recipient email address. |
| Email | `EMAIL_FROM` | _(empty)_ | `--email-from`, `--email-sender` | Sender email address. |
| Email | `EMAIL_SUBJECT` | _(empty)_ | `--email-subject` | Subject used in email notifications. |
| Logging & control | `DEBUG` | _(empty)_ | `--debug`, `-D` | Verbose logging and debug output. |
| Logging & control | `EVENTS_FILE` | _(empty)_ | `--event-file`, `--event`, `-e` | File path to append unlock events. |
| Logging & control | `SLEEP_INTERVAL` | `10` | `--sleep-interval`, `--sleep`, `--interval`, `-i` | Seconds to wait between retries. |
| Logging & control | `SKIP_SSH_PORT_CHECK` | _(empty)_ | `--skip-ssh-port-check`, `--skip-port-check` | Skip preflight SSH port probing. |
| Runtime | `TMPDIR` | `/tmp` | _n/a_ | Working directory for temporary files. |

## 🐳 Docker
The repository includes a multi-stage Docker build that bundles the scripts with their dependencies. You can either build locally or pull the published images:

- GitHub Container Registry: `ghcr.io/pschmitt/luks-ssh-unlock`
- Docker Hub: `docker.io/pschmitt/luks-ssh-unlock`

### 🛠️ Build
```bash
docker build -t luks-ssh-unlock:latest .
```

### ▶️ Run
Mount your secrets and pass the relevant variables. For example:
```bash
docker run --rm -it \
  -v /path/to/secrets:/run/secrets:ro \
  -e SSH_HOSTNAME=10.0.0.5 \
  -e SSH_KEY=/run/secrets/id_ed25519 \
  -e LUKS_PASSPHRASE_FILE=/run/secrets/luks_password_10.0.0.5 \
  ghcr.io/pschmitt/luks-ssh-unlock:latest
```
The container entrypoint copies `msmtprc` into place when provided and then execs the main script, so all flags can be passed as CLI arguments or environment variables.

### 🧱 docker-compose
`docker-compose.yaml` now demonstrates multiple unlock targets with health checks, notifications, jumphosts, and initrd validation. Key points:

- Secrets are provided via the Compose `secrets` feature so SSH keys and passphrases stay off the command line.
- Each service mounts a shared `events/` directory for audit logs and exposes health checks via environment variables.
- Jumphost settings, Apprise URLs, and msmtp email metadata are wired in to show how notifications integrate.
- Initrd checksum validation is enabled for one service to illustrate paranoid startup checks.
- When health checks succeed and `INITRD_CHECKSUM_DIR` is set, the initrd snapshot from `/etc/initrd-checksum` is downloaded into `INITRD_CHECKSUM_DIR/<hostname>/` (for example `events/initrd-checksum/vault-east/`) so you can diff future boots.

See the inline comments in the Compose file for guidance when adapting it to your environment.

## ❄️ Nix
A flake is provided for reproducible builds, development shells, and a NixOS module.

- Build the main package: `nix build .#luks-ssh-unlock`
- Build the entrypoint helper: `nix build .#entrypoint`
- Enter a shell with all required tools: `nix develop`
- Pull in static BusyBox utilities: `nix build .#busybox-static`

### 🧩 NixOS module
The `nix/module.nix` output offers a `services.luks-ssh-unlock` module for declarative deployments. Highlights:

- `services.luks-ssh-unlock.enable` toggles the service and packages the CLI.
- `services.luks-ssh-unlock.instances` lets you define multiple targets; each instance emits an environment file under `/etc/luks-ssh-unlock/<name>.env` and a matching systemd unit `luks-ssh-unlock-<name>`.
- Each instance supports SSH (host, user, key, port, force IPv4/IPv6), jumphosts, LUKS type and passphrase inputs, optional initrd checksum validation, and health checks.
- Notification helpers include email fields (`recipient`, `sender`, `subject`) and msmtp wiring, plus host key pinning for both normal and initrd unlock flows.
- `services.luks-ssh-unlock.activationScript.enable` can precompute initrd checksums during activation by using the packaged `initrd-checksum` helper.

Refer to `nix/module.nix` for the full option list and defaults; each option is documented inline in the module for quick reference.

## 🤝 Contributing
- Bash scripts follow strict shell practices; please run `shellcheck` before submitting changes.
- Keep option documentation in sync between the script usage and the environment table above.

## 📜 License
This project is licensed under the GNU General Public License v3.0. See [LICENSE](LICENSE).
