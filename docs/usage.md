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
luks-ssh-unlock tui --filter rofl               # only targets whose name or host contains "rofl"
```

Each target is described by an environment file in systemd
`EnvironmentFile=` syntax, the same files the NixOS module writes. Reading SSH
keys and passphrases usually needs root.
Started as a normal user in a terminal, the command re-runs itself with
`sudo` when it cannot read them (`--no-sudo` or `LUKS_SSH_UNLOCK_NO_SUDO=1`
turn this off). Services and scripts never do this.

On a narrow or tall terminal (under 80 columns, or taller than about two
thirds of its width, such as a phone) the panes are stacked instead: targets
on top, then the activity. `Shift`+`Up`/`Down` moves the
focus between them.

The target list (as wide as its content needs) takes the full height on the
left; each target shows its state and, when it has a healthcheck, a green or
red `hc ●`. Next to it, the activity: the service's journal
(`journalctl -u luks-ssh-unlock-<name>.service`, live) interleaved with what
the dashboard itself does (check results, unlocks), marked `svc` and `tui`.
The extra command pane, if configured, is below the activity. `i` shows the
selected target's details (route, LUKS method, healthcheck, integrity checks,
host keys, uptime) in a dialog.

| Key | Action |
| --- | --- |
| `Up`/`Down`, `j`/`k` | Select a target (scroll, when the activity has the focus) |
| `Shift`+arrows, `Tab`/`Shift`+`Tab` | Move the focus to another pane (highlighted border) |
| `i` | Details of the selected target, in a dialog; any key closes it |
| Arrows | Also move the focus when the focused pane has no use for them |
| `l` | Show the activity full screen; `l` or `Esc` goes back |
| `C` | Show the console (extra command pane) full screen; `C` or `Esc` goes back |
| `?` | Show the keys; any key closes it |
| `/` | Filter the targets by name or host while typing; `Enter` keeps the filter, `Esc` clears it |
| `c` | Next extra command (see below) |
| `r` / `R` | Check the selected target / every target now |
| `u` | Unlock the selected target, after confirmation. All checks still apply. |
| `F` | Force unlock, after confirmation. Skips the integrity and host-key checks. |
| `S` | Stop the selected target's systemd service, or start it when it is not running (needs root). The dashboard warns about a stopped service. |
| `PageUp`/`PageDown`, `Home`/`End` | Scroll the activity, oldest/newest |
| `q`, `Esc` | Quit (`Esc` closes the full-screen activity first) |
| `Ctrl+]`, `Ctrl+R`/`F5` | In an interactive console, which has the keyboard otherwise: back to the dashboard, restart the console (see [Interactive consoles](#interactive-consoles)) |

With a mouse: click a target to select it, double-click to unlock it (the
confirmation dialog has clickable **Yes** and **Cancel** buttons), click a
pane to focus it, scroll the wheel over the target list or the activity, and
drag the borders between the panes to resize them.

### Extra command pane

`--extra-command '[LABEL=][@]COMMAND'` (repeatable) adds an optional pane below
the activity that shows the output of a command for the selected target, for
example a cloud console log:

```sh
luks-ssh-unlock tui --extra-command 'console=openstack::console-log {name}'
```

`{host}` and `{name}` are replaced by the target's host and name
(shell-quoted, also exported as `LUKS_SSH_UNLOCK_HOST` and
`LUKS_SSH_UNLOCK_NAME`). The command runs with `sh -c` (so it must be an
executable on `PATH`, not a shell function; wrap it in `zsh -lc '...'` for
those), as the user who ran `sudo` when the dashboard runs as root through it.

The output (stdout and stderr, colors kept, last 2000 lines) is streamed line
by line, so commands that follow a log work. It runs while its target is
selected and stops when another one is selected, `c` switches, or the dashboard
exits. A command that ends is started again every refresh interval; `r` starts
it again at once. The pane scrolls when it has the focus (like the activity)
and its border can be dragged; `c` switches between several commands. Without
the option, commands are read from `/etc/luks-ssh-unlock/tui-commands` (one
`[LABEL=][@]COMMAND` per line, `#` comments), which the NixOS module writes from
`services.luks-ssh-unlock.tui.extraCommands`.

#### Interactive consoles

A command marked with `@` (`console=@COMMAND`) is interactive: it runs on a
pseudo-terminal the size of the pane and the pane shows its terminal screen,
so prompts without a newline (`login: `), line editing, colors and full-screen
programs work. The size follows the pane (the command gets `SIGWINCH`), and
`TERM` is `xterm-256color`. Commands without `@` keep streaming their output
line by line, as before.

```sh
luks-ssh-unlock tui --extra-command 'oci=@oci-console {name}'
```

While such a console has the focus (click it, or move the focus with
`Shift`+arrows or `Tab`; its border turns yellow) it captures the keyboard:
every key goes to the program, including `q`, `Esc`, `Ctrl+C`, `Tab` and the
arrows, so nothing can quit or change the dashboard by accident. Only these
keys stay with the dashboard:

| Key | Action |
| --- | --- |
| `Ctrl+]` | Give the keyboard back: the target list gets the focus, the console keeps running. Also leaves the full-screen console. |
| `Ctrl+R`, `F5` | Restart the console: the program is stopped (and may clean up first), then started again. Also works when it has ended. |
| `Shift`+arrows | Move the focus to another pane, as everywhere. |
| `Shift`+`PageUp`/`PageDown`, wheel | Scroll back through the console's history; typing goes back to the end. |

Clicking another pane also releases the keyboard. The footer shows the escape
key while the console captures the keyboard. A console that ends (the program
exits, or `Ctrl+R` stops it) shows `[exited]` (or `[exit N]`, `[killed]`) and the keyboard returns to the
dashboard, whose keys work as usual; it is not started again on its own, press
`Ctrl+R` or `F5` for that. Pasting is sent as typed keys (no bracketed paste),
and the mouse is not forwarded to the program.

A target can have its own commands in `<name>.tui-commands` next to its
environment file (same format; the NixOS module writes it from the instance's
`tuiCommands`). They replace the common command of the same label, add new
ones, and an empty command (`console=`) removes that label for the target.
This is how a host that is not an OpenStack VM gets a different console
command, or none.

A target can instead specify a typed console in `<name>.console.json` next to
its environment file. It replaces the `console` command from either the target
or the common commands. The NixOS module writes this file from the instance's
`console` option:

```nix
console = {
  type = "oci";
  parameters = {
    instance = "example-instance"; # Defaults to the target name.
    identity = "/home/user/.ssh/id_rsa";
    profile = "DEFAULT";
  };
};
```

OCI supports serial consoles and requires the OCI CLI and OpenSSH in the
console user's PATH, plus an RSA private key and its `.pub` file. Optional
parameters are `instance`, `identity`, `configFile`, `cliRcFile`, `profile`,
`compartmentId`, and `keep`. LSU creates a console connection and attempts to
remove only that connection when the session ends; `keep = true` leaves it in
place. A connection can also be started directly with
`luks-ssh-unlock console oci --instance example-instance`.

For existing shell commands, use `type = "command"` with
`parameters.command` and optionally `parameters.interactive = true`. Legacy
`tuiCommands` remain supported. The `openstack` type is reserved for future
support and is not yet implemented. The console pane currently renders text
terminal output only; graphical terminal protocols such as Kitty graphics are
not passed through, so a graphical VNC viewer cannot yet be embedded there.

The dashboard follows each service's journal live (this needs root or the
`systemd-journal` group) and checks a target again as soon as its service logs
something, so state changes show up within a couple of seconds rather than at
the next periodic check. A target whose service is stopped, failed, disabled
or masked is marked with a warning, since nothing will unlock it
automatically.

The periodic checks never send notifications and skip the initrd checksum,
which can be slow in paranoid mode. An unlock always runs it first.
