<div align="center">
  <a href="assets/fresco-full.jpg"><img src="assets/fresco.jpg" width="800" alt="A medieval fresco: a procession rides to the LUKS castle, led by a banner reading SSH, a knight with a terminal and a knight with a Tux shield, while a crowned king raises a golden key towards the padlocked gate"></a>
  <p align="right"><sub><em>Per clavem secretam, porta aperitur.</em></sub></p>
  <h1 align="center">luks-ssh-unlock</h1>
  <p>
    <a href="https://nixbot.brkn.lol/repos/github/pschmitt/luks-ssh-unlock"><img src="https://nixbot.brkn.lol/repos/github/pschmitt/luks-ssh-unlock/badge.svg?branch=main" alt="nixbot"></a>
    <a href="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/build.yaml"><img src="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/build.yaml/badge.svg" alt="build"></a>
    <a href="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/test.yaml"><img src="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/test.yaml/badge.svg" alt="test"></a>
    <a href="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/docker.yaml"><img src="https://github.com/pschmitt/luks-ssh-unlock/actions/workflows/docker.yaml/badge.svg" alt="docker"></a>
  </p>
  <p align="center">Unlock LUKS-encrypted hosts remotely: wait for their initrd, verify it, send the passphrase over SSH.</p>
</div>

A host with an encrypted root disk stops at boot until someone types the
passphrase. luks-ssh-unlock watches such hosts and answers their initrd over
SSH, but only after it has checked that this is the initrd it expects: pinned
initrd host keys, and optionally a signed checksum of the initrd's contents.

## Features

- **Watch mode**: checks each host every few seconds, unlocks it when it waits
  in its initrd, and logs each state change once. See
  [how an unlock is decided](docs/how-it-works.md).
- **Four unlock methods**: stdin of a forced command (dropbear,
  `cryptroot-unlock`), systemd's password agent on a remote TTY,
  `cryptsetup luksOpen` plus the cryptsetup unit, and `luks-mount`.
- **Initrd integrity**: compares the running initrd with a baseline the host
  signed with its SSH host key (`ssh-keygen -Y`), optionally with a minimal
  BusyBox bundle uploaded to the target.
- **Dashboard**: `luks-ssh-unlock tui` shows every target's live state,
  details and logs, and unlocks one after confirmation; an interactive
  console pane (`@COMMAND`) takes the keyboard while focused (`Ctrl+]` leaves
  it, `Ctrl+R`/`F5` restarts it). See
  [the dashboard](docs/usage.md#dashboard).
- **Status reports**: `status` and `status --json` show what a host is doing
  without sending anything.
- **Notifications**: Apprise, mail (sendmail or msmtp) and an events file.
- **DHCP trigger**: starts a validated one-shot unlock as soon as the target's
  initrd gets a DHCP lease.
- **NixOS module**: one service, environment file and helper command per
  target. See [Nix](docs/nix.md).

## Install

```nix
{
  inputs.luks-ssh-unlock.url = "github:pschmitt/luks-ssh-unlock";
  # in your NixOS configuration:
  imports = [ inputs.luks-ssh-unlock.nixosModules.default ];
  services.luks-ssh-unlock.enable = true;
}
```

Container images: `ghcr.io/pschmitt/luks-ssh-unlock` and
`docker.io/pschmitt/luks-ssh-unlock` (see [container](docs/container.md)).
From source: `cargo install --path .` (needs OpenSSH at run time).

## Quick start

```sh
luks-ssh-unlock status --host target.example.com \
  --ssh-key ~/.ssh/id_ed25519 \
  --ssh-initrd-known-hosts-file ~/.ssh/known_hosts_initrd   # what is it doing?
luks-ssh-unlock --host target.example.com \
  --ssh-key ~/.ssh/id_ed25519 \
  --ssh-initrd-known-hosts-file ~/.ssh/known_hosts_initrd \
  --luks-passphrase-file /run/secrets/luks_target \
  --healthcheck-cmd 'systemctl is-system-running'           # watch and unlock
luks-ssh-unlock tui                                         # all targets in /etc/luks-ssh-unlock
```

Every option also has an environment variable; see
[configuration](docs/configuration.md).

## Documentation

| Topic | |
| --- | --- |
| [Usage](docs/usage.md) | Commands, one-shot runs, exit codes and the dashboard |
| [How it works](docs/how-it-works.md) | The checks before an unlock, unlock methods, what is logged |
| [Configuration](docs/configuration.md) | Every option and environment variable |
| [Nix](docs/nix.md) | Flake outputs and the NixOS module |
| [Container](docs/container.md) | Images, volumes and docker-compose |
| [Development](docs/development.md) | Building, tests and checks |
| [Changelog](CHANGELOG.md) | Release notes |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
