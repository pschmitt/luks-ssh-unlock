# Usage

`luks-ssh-unlock [COMMAND] [OPTIONS]`. Options go before or after the
command, and every option has an environment variable (see
[configuration](configuration.md)).

## Commands

| Command | What it does |
| --- | --- |
| `run` | Watch the target and unlock it whenever it waits in its initrd. The default. |
| `run --once` | Make one attempt and exit. |
| `status` | Show the configuration and what the target is doing, without sending anything. |
| `status --json` | The same report as JSON. |
| `tui [FILES...]` | The dashboard, see below. |
| `completions SHELL` | Shell completions for bash, zsh, fish, elvish or PowerShell. |

`--force` (only with `--once`) skips the initrd checksum, its signature and
the SSH host-key checks. The client key is still required.

`--debug` logs every check, the remote output (sanitized) and the SSH
commands' results.

## Exit codes

| Code | `run --once` | `status` |
| --- | --- | --- |
| 0 | The host is up, was unlocked, or answers but is not in its initrd | The host is up, or waits in its initrd and may be unlocked |
| 1 | Unreachable, SSH login failed, integrity check failed, unlock failed or timed out | Anything else |
| 2 | Invalid configuration (missing host, unknown method, `--force` without `--once`, unreadable key) | Same |
| 3 | The passphrase file is not readable | |

## Dashboard

```sh
sudo luks-ssh-unlock tui                        # every /etc/luks-ssh-unlock/*.env
luks-ssh-unlock tui hosts/ --refresh 10         # every *.env in a directory
luks-ssh-unlock tui --host target.example.com   # one target from the options
```

Each target is described by an environment file in systemd
`EnvironmentFile=` syntax, the same files the NixOS module writes. Reading SSH
keys and passphrases usually needs root.
Started as a normal user in a terminal, the command re-runs itself with
`sudo` when it cannot read them (`--no-sudo` or `LUKS_SSH_UNLOCK_NO_SUDO=1`
turn this off). Services and scripts never do this.

On a narrow or tall terminal (under 80 columns, or taller than about two
thirds of its width, such as a phone) the panes are stacked instead: targets
on top, then the details, then the activity.

The left pane lists the targets and their state, the right pane shows the
selected target's details and its activity: the service's journal
(`journalctl -u luks-ssh-unlock-<name>.service`, live) interleaved with what
the dashboard itself does (check results, unlocks), marked `svc` and `tui`.

| Key | Action |
| --- | --- |
| `Up`/`Down`, `j`/`k` | Select a target |
| `r` / `R` | Check the selected target / every target now |
| `u`, `Enter` | Unlock the selected target, after confirmation. All checks still apply. |
| `F` | Force unlock, after confirmation. Skips the integrity and host-key checks. |
| `PageUp`/`PageDown` | Scroll the activity |
| `q`, `Esc` | Quit |

With a mouse: click a target to select it, double-click to unlock it (the
confirmation dialog has clickable **Yes** and **Cancel** buttons), scroll the
wheel over the target list or the activity.

The dashboard follows each service's journal live (this needs root or the
`systemd-journal` group) and checks a target again as soon as its service logs
something, so state changes show up within a couple of seconds rather than at
the next periodic check. A target whose service is stopped, failed, disabled
or masked is marked with a warning, since nothing will unlock it
automatically.

The periodic checks never send notifications and skip the initrd checksum,
which can be slow in paranoid mode. An unlock always runs it first.
