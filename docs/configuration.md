# Configuration

Every option can be given on the command line or as an environment
variable; the command line wins. Boolean variables are off when empty, `0`,
`false`, `no` or `off`. Run `luks-ssh-unlock --help` for the full list of
flag aliases.

## General

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `DEBUG` | `--debug`, `-D` | off | Debug logging. |
| `RUN_ONCE` | `--once` | off | One attempt, then exit. |
| `FORCE` | `--force`, `-f` | off | Skip the integrity and host-key checks; needs `--once`. |
| `SLEEP_INTERVAL` | `--interval`, `-i` | `10` | Seconds between checks. |
| `TICK_TIMEOUT` | `--tick-timeout` | `120` | Abort a check that takes longer. |
| `EVENTS_FILE` | `--events-file`, `-e` | | Append notification events to this file. |
| `TMPDIR` | | `/tmp` | Where the private scratch directory is created. |

## SSH

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `SSH_HOSTNAME` | `--host`, `-H` | required | Target host. |
| `SSH_PORT` | `--port`, `-P` | `22` | SSH port. |
| `SSH_USERNAME` | `--username`, `-u` | `root` | SSH user. |
| `SSH_KEY` | `--ssh-key`, `-k` | `/run/secrets/ssh_key` | Private key. Copied to a private 0400 file if its permissions are too open for OpenSSH. |
| `SSH_CONNECTION_TIMEOUT` | `--connect-timeout` | `5` | Connect timeout in seconds. |
| `SSH_KNOWN_HOSTS`, `SSH_KNOWN_HOSTS_FILE` | `--ssh-known-hosts[-file]` | | Host keys of the booted system, inline or as a file. Also the allowed signers of the baseline. |
| `SSH_INITRD_KNOWN_HOSTS`, `SSH_INITRD_KNOWN_HOSTS_FILE` | `--ssh-initrd-known-hosts[-file]` | | Host keys of the initrd SSH server. |
| `FORCE_IPV4`, `FORCE_IPV6` | `-4`, `-6` | off | Use one address family. |
| `SKIP_SSH_PORT_CHECK` | `--skip-ssh-port-check` | off | Skip the TCP probe of the SSH port. |
| `SSH_CONNECT_ADDRESS`, `SSH_HOSTKEY_ALIAS` | | | Connect to this address but verify host keys under this name. The DHCP listener sets them. |

Without known_hosts for a key set, host keys of that set are not verified.

## Jumphost

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `SSH_JUMPHOST` | `--jumphost`, `-J` | | Reach the target through this host. |
| `SSH_JUMPHOST_USERNAME` | `--jumphost-username`, `-U` | `root` | Jumphost user. |
| `SSH_JUMPHOST_PORT` | `--jumphost-port` | the SSH port | Jumphost port. |
| `SSH_JUMPHOST_KEY` | `--jumphost-key`, `-K` | the SSH key | Jumphost key. |

The jumphost's host key is checked against both known_hosts sets.

## LUKS

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `LUKS_TYPE` | `--luks-type`, `-t` | `raw` | Unlock method, see [how it works](how-it-works.md#unlock-methods). |
| `LUKS_PASSPHRASE_FILE` | `--luks-passphrase-file`, `-F` | `/run/secrets/luks_password_<host>` | Passphrase file. An empty value disables the default. A systemd credential named `luks-passphrase` takes precedence over the environment. |
| `LUKS_PASSPHRASE` | `--luks-passphrase`, `-p` | | The passphrase itself; visible to other users on the command line. |

## Initrd validation

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `INITRD_CHECKSUM_FILE` | `--initrd-checksum-file` | | Baseline the running initrd must match. |
| `INITRD_CHECKSUM_DIR` | `--initrd-checksum-dir` | | Where baselines fetched from the booted host are stored. |
| `INITRD_CHECKSUM_SCRIPT` | `--initrd-checksum-script` | `initrd-checksum` next to the binary | The checksum helper. |
| `INITRD_CHECKSUM_BUSYBOX_DIR` | | the packaged bundle | BusyBox bundle for paranoid mode. |
| `PARANOID` | `--paranoid` | off | Run the checksum with the uploaded BusyBox bundle. |
| `INITRD_CHECKSUM_REQUIRE_SIGNATURE` | `--require-signature` | off | Refuse unless the baseline has a valid signature. |

## Healthcheck

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `HEALTHCHECK_PORT` | `--healthcheck-port` | | TCP port that only opens once the host has booted. |
| `HEALTHCHECK_REMOTE_CMD` | `--healthcheck-cmd`, `--remote-check` | | Command that only succeeds on the booted system. |
| `HEALTHCHECK_REMOTE_HOSTNAME` | `--healthcheck-host` | the target | Host to run it on. |
| `HEALTHCHECK_REMOTE_USERNAME` | `--healthcheck-user` | the SSH user | User to run it as. |
| `SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE` | `--healthcheck-known-hosts-type` | `default` | Host keys for the healthcheck: `default` or `initrd`. |

## Notifications

| Variable | Flag | Default | Description |
| --- | --- | --- | --- |
| `APPRISE_URL` | `--apprise-url`, `-a` | | Apprise API endpoint. |
| `APPRISE_TAG` | `--apprise-tag` | | Apprise tag. |
| `APPRISE_TITLE` | `--apprise-title` | | Title, with placeholders. |
| `EMAIL_RECIPIENT` | `--email-recipient` | | Mail notifications to this address. |
| `EMAIL_FROM` | `--email-from` | | Sender address. |
| `EMAIL_SUBJECT` | `--email-subject` | `luks-ssh-unlock: <host> <event>` | Subject, with placeholders. |
| `MSMTP_ACCOUNT` | `--msmtp-email-account` | | Send with `msmtp -a ACCOUNT` instead of `sendmail`. |

Placeholders in titles and subjects: `#hostname`, `#username`, `#port`,
`#jumphost`, `#jusername`, `#jport`, `#luks_type`, `#event`, `#event_type`,
`#sleep_interval`, `#remote_cmd`, `#remote_hostname`, `#remote_username`.
