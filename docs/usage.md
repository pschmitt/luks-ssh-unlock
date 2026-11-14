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
luks-ssh-unlock tui                             # every /etc/luks-ssh-unlock/*.env
luks-ssh-unlock tui hosts/ --refresh 10         # every *.env in a directory
luks-ssh-unlock tui --host target.example.com   # one target from the options
luks-ssh-unlock tui --filter rofl               # only targets whose name or host contains "rofl"
```

Each target is described by an environment file in systemd
`EnvironmentFile=` syntax, the same files the NixOS module writes.

The dashboard runs as the user who started it and refuses to run as root.
OCI and OpenStack consoles, the VNC viewer and the extra commands therefore
use that user's own keys and environment. It never re-runs itself under
`sudo`. What only root can read goes through the privileged helper (see
"Privileged helper" below); without the helper, such a target shows an error
instead of a state.

On a narrow or tall terminal (under 80 columns, or taller than about two
thirds of its width, such as a phone) the panes are stacked instead: targets
on top, then the activity. `Shift`+`Up`/`Down` moves the
focus between them.

The target list (as wide as its content needs) and the activity share the
upper part of the screen, side by side; the extra command pane, if configured,
takes the full width below them. Each target shows its state and, when it has
a healthcheck, a green or red `●` next to it. The activity is the service's
journal (`journalctl -u luks-ssh-unlock-<name>.service`, live) interleaved
with what the dashboard itself does (check results, unlocks), marked `svc` and
`tui`. `i` shows the selected target's details (route, LUKS method,
healthcheck, integrity checks, host keys, uptime) in a dialog.

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
| `S` | Stop the selected target's systemd service, or start it when it is not running (needs the helper's admin group when the target is helper-backed; otherwise the unit must be controllable by the user). The dashboard warns about a stopped service. |
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
those), as the user who started the dashboard.

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
`tuiCommands` remain supported.

`type = "openstack"` is the serial console of an OpenStack server, reached
through the `openstack` CLI (`openstack console url show --serial`) and the
console proxy's WebSocket. A fresh single-use token is requested every time the
console starts, so `Ctrl+R` always gets a working one:

```nix
console = {
  type = "openstack";
  parameters = {
    server = "example-server"; # Defaults to the target name.
    cloud = "example-cloud";   # --os-cloud
    cloudsFile = "/run/secrets/clouds.yaml"; # a runtime path, never a store path
  };
};
```

Optional parameters are `server`, `cloud`, `cloudsFile`, `caFile` (CA bundle
for the proxy), `allowInsecure` and `openstackBin` (absolute path of the CLI);
file paths must be absolute. A plain `ws://` proxy is only accepted on a
loopback host unless `allowInsecure` is set, and the console then warns that it
is unencrypted. The session can also be started directly with
`luks-ssh-unlock console openstack --server example-server` (`Ctrl+]`
detaches). Tokens and console URLs are never put on a command line, in a log or
in an error message. The console pane renders text terminal output only. The
graphical desktop of a target comes from `<name>.vnc.json` instead (see
"Built-in VNC viewer" below), for OCI, OpenStack or a direct VNC endpoint.

The dashboard follows each service's journal live (this needs the
`systemd-journal` group or root-equivalent access, since the dashboard never runs as root) and checks a target again as soon as its service logs
something, so state changes show up within a couple of seconds rather than at
the next periodic check. A target whose service is stopped, failed, disabled
or masked is marked with a warning, since nothing will unlock it
automatically.

The periodic checks never send notifications and skip the initrd checksum,
which can be slow in paranoid mode. An unlock always runs it first.

## Privileged helper

A target whose environment file, SSH key or passphrase file the user cannot
read is served by a small root helper instead of by the dashboard. The
dashboard stays the invoking user's process; only the helper reads the
protected material, and it answers with closed values (a state, which checks
were in force, whether the passphrase was accepted). It never returns a
configuration value, a key or a passphrase, and its error codes are coarse.

Directly readable targets never use the helper, so their OCI and OpenStack
consoles keep working with the user's own credentials. The provider and VNC
sidecar files (`<name>.console.json`, `<name>.vnc.json`) are always read by
the dashboard itself, as the user.

### Setup (NixOS)

```nix
services.luks-ssh-unlock.helper = {
  enable = true;
  users = [ "alice" ];       # list, check and unlock
  adminUsers = [ "bob" ];    # also forced unlock and service start/stop
  protectConfig = false;     # true: make the env files root-only
};
```

systemd starts one root process per connection (`Accept=yes`) on two sockets
under `/run/luks-ssh-unlock`: `basic.sock` for the `luks-ssh-unlock` group and
`admin.sock` for `luks-ssh-unlock-admin`. Which socket was used decides what a
connection may do; nothing in a request can raise it. Log in again after
changing group membership. Each request is audited in the journal (uid, pid,
verb, target, outcome; never output).

The dashboard only talks to a socket that is a socket owned by root (not a
symlink) and whose peer is root (`SO_PEERCRED`); owning the path is not
enough. The helper in turn refuses to serve on a socket that is not root-owned
and closed to everyone but the group.

### Testing against a real configuration

1. Enable the helper as above for your user and switch to a new login so the
   group applies.
2. Run `luks-ssh-unlock tui` as that user. Protected targets are listed from
   the helper and show "via privileged helper" as their route.
3. For a hand-started helper, `LUKS_SSH_UNLOCK_HELPER_SOCKET` points the
   dashboard at another socket; the peer must still be root.

### Limitations

- A helper-backed target reports less: no uptime, remote baseline date or
  notes, and its route and LUKS method are fixed texts.
- A plain unlock through the helper is stricter than the command line: it
  needs a checksum baseline and pinned initrd host keys, otherwise it is
  `denied`. Skipping those checks needs the admin socket.
- Following the journal and reading the unit's state run as the user.
- Sidecars that the user cannot read are absent for a helper-backed target.
- The helper's service sandbox sets `ProtectHome=read-only`, so keys under
  `/root` or `/home` must be readable by root. It needs `ssh`, `ssh-keygen`
  and `systemctl` in `/run/current-system/sw/bin`. The sandbox has not been
  exercised in a NixOS VM test; adjust it with `helper.serviceConfig`.
- Paths that are not root-owned all the way down can be swapped between the
  helper's check and use. `ssh`, `ssh-keygen` and `systemctl` are found through
  `PATH`; the `ProxyCommand` runs through `/bin/sh`; the checksum script runs
  commands on the target that are not vetted; `ssh` reads the global
  `known_hosts`; with unpinned initrd host keys a probe does not verify the
  host (`StrictHostKeyChecking=no`); the healthcheck and jump host are trusted
  configuration. A literal passphrase in the environment file is held in
  memory without being wiped.

## Built-in VNC viewer

The dashboard contains a VNC client: the remote desktop of a target is shown
as a picture inside the console pane, next to the serial console, without
starting any other program. It is configured per target, independent of the
console provider, in `<name>.vnc.json` next to the environment file (the NixOS
module writes it from the instance's `vnc` option):

```json
{"type": "vnc", "parameters": {"host": "127.0.0.1", "port": 5901, "passwordFile": "/run/secrets/vnc"}}
```

```nix
vnc = {
  port = 5901;
  passwordFile = "/run/secrets/vnc"; # a runtime path, never a store path
  viewOnly = false;
};
```

All parameters are optional: `host` (default `127.0.0.1`), `port` (default
5900), `passwordFile`, `viewOnly` (start without forwarding input), `quality`
and `compression` (Tight, 0 to 9), `resizeRemote` (ask the remote desktop to
follow the pane's size; off by default, applied after the pane has held still
for a moment) and `allowRemote`.

The desktop of an OpenStack server (its noVNC console) is `type = "openstack"`.
The dashboard asks the `openstack` CLI for a console URL, opens the proxy's
WebSocket (binary frames, subprotocol `binary`) and speaks RFB over it, so no
tunnel and no extra program are needed; every start and every `Ctrl+R` fetches
a new single-use token:

```json
{"type": "openstack", "parameters": {"server": "example-server", "cloud": "example-cloud", "cloudsFile": "/run/secrets/clouds.yaml"}}
```

It takes `server` (default: the target name), `cloud`, `cloudsFile`, `caFile`,
`allowInsecure` and `openstackBin`, as the `openstack` console above, plus the
shared `passwordFile`, `viewOnly`, `quality`, `compression` and `resizeRemote`
(not `host`, `port` or `allowRemote`). The NixOS module takes the same fields
in the instance's `vnc` option with `vnc.type = "openstack"`.

The graphical console of an OCI instance is `type = "oci"`. The dashboard has
the `oci` CLI create a graphical console connection for the instance, starts an
`ssh` tunnel through the provider's console service into a private Unix socket
(mode 0600, in a mode 0700 directory under `$XDG_RUNTIME_DIR`; no TCP port is
opened on this machine) and speaks RFB over that socket. The connection is
deleted again when the viewer ends (not with `keep`), and by a helper process
even if the dashboard is killed. Every start and every `Ctrl+R` creates a new
connection:

```json
{"type": "oci", "parameters": {"instance": "example-instance", "profile": "DEFAULT", "identity": "/home/user/.ssh/id_rsa"}}
```

It takes `instance` (default: the target name), `identity`, `configFile`,
`cliRcFile`, `profile`, `compartmentId` and `keep`, as the OCI serial console
above (paths absolute; `identity` may also start with `~/`), plus the shared
`passwordFile`, `viewOnly`, `quality`, `compression` and `resizeRemote` (not
`host`, `port` or `allowRemote`). The NixOS module takes the same fields in
the instance's `vnc` option with `vnc.type = "oci"`. It needs the OCI CLI and
OpenSSH in the PATH and an RSA key with its `.pub` file, as the serial console
does, and outbound TCP port 443. OCI allows one console client at a time: a
client that died can hold the slot for about five minutes, and the pane then
says that the console is in use (`Ctrl+R` tries again). Failures of the tunnel
are shown as one of a few fixed messages (host key rejected, key refused,
console busy, instance without a VNC server, service unreachable); the output
of `ssh` is never shown.

When a target has both a VNC endpoint and a serial console, `F6` (or `c` in the
dashboard) shows the next view. The pane's keys, while it has the focus and the
desktop has the keyboard:

| Key | Action |
| --- | --- |
| `Ctrl+]` | Give the keyboard back to the dashboard (the session keeps running). |
| `Ctrl+T` | Switch between the desktop having the keyboard and mouse, and view-only (the dashboard's keys work again). |
| `Ctrl+R`, `F5` | Restart the viewer: terminal graphics are looked for again and the connection is made anew. |
| `F6` | Show the next view of the target. |
| `Shift`+arrows | Move the focus to another pane. |

Everything else is sent to the remote desktop, and so is the mouse (clicks,
drags, wheel) over the picture; the click that gives the pane the focus is the
dashboard's. Keys are dropped until the first picture has arrived. A key or
button release is never dropped: if the connection cannot keep up, the session
ends rather than leave a key held down on the remote desktop.

Security:

- VNC is not encrypted, and its password authentication is weak (the password
  is at most eight characters long and protects against nothing but casual
  access). The endpoint therefore has to be a loopback address, such as the
  local end of an SSH tunnel. A host that is not loopback is refused unless
  `allowRemote` is set, and the pane is then marked `UNENCRYPTED`. An OpenStack
  console normally runs over TLS (`wss`); a plain `ws://` proxy on a
  non-loopback host needs `allowInsecure` and is marked `UNENCRYPTED` too. The
  console token is never shown, logged or put on a command line, and the
  provider's errors are reported with fixed messages. An OCI console reaches
  the viewer through a private Unix socket (checked to be ours, and served by
  our own `ssh`) and an SSH tunnel; it is not marked unencrypted. No private
  key, token or connection string appears in a command line, a log or a
  message.
- The password is read from `passwordFile` by the dashboard when a session
  starts: the file must be a regular file (not a symbolic link, not in the Nix
  store, at most 4096 bytes), mode 0600 or stricter, owned by the user running
  the dashboard, root, or the user who ran `sudo`. The password is never put on a
  command line, in the configuration or in a message; errors do not repeat it.
- The dashboard is unprivileged and stays so: it never carries tmux variables
  (`TMUX`, `TMUX_PANE`) across `sudo`, and it never runs `tmux` as root, so root
  never talks to another user's tmux server. Inside tmux, the dashboard asks
  tmux (as the user who runs it, on the socket named by its own `TMUX`) for the
  pane's `allow-passthrough` and for the clients that show the pane's window:
  their `TERM`, their `XTVERSION` answer and, when it is readable (same user),
  their process environment. Only those client facts count; hints in the pane's
  own environment are ignored, since the tmux server may have been started in
  another terminal. No command or socket path from the environment is trusted
  as root. A process that is root and has `TMUX` is trusted only when the
  socket belongs to root or to the user who ran `sudo` and `TMUX_PANE` looks
  like `%N`, and even then it does not ask tmux: the pane says that the
  terminal cannot be asked from a privileged process, and no connection is
  made. Run the dashboard as the user who owns the tmux session (the viewer then
  waits up to two seconds, in a background thread, for tmux to answer, the
  pane showing `starting...` meanwhile).

Requirements and limitations:

- The terminal must support the Kitty graphics protocol (kitty, WezTerm,
  Ghostty or Konsole). Otherwise the pane says so, naming the `TERM` it was
  given, and no connection is made. Behind tmux, `TERM` is usually `screen-*`
  or `tmux-*` and says nothing: the terminal is then recognised from the tmux
  client (a name such as `xterm-kitty`, its `XTVERSION` answer, or its
  environment: `KITTY_WINDOW_ID`, `WEZTERM_PANE`, `GHOSTTY_*`, `KONSOLE_*`,
  `TERM_PROGRAM`). A terminal that cannot be recognised (a generic `TERM`
  forwarded over ssh, for instance) is accepted with
  `LUKS_SSH_UNLOCK_GRAPHICS=kitty`; `LUKS_SSH_UNLOCK_GRAPHICS=off` never draws
  pictures. Ctrl+R asks tmux again.
- In tmux, `allow-passthrough` must be `on` or `all` for the pane (the setting
  that counts is the pane's effective one, whether set globally, for the window
  or for the pane; `tmux set -p allow-passthrough on`). The picture is then
  shown with Unicode placeholders; the pane says what is wrong when it is `off`.
  The detection is tested against a real tmux server on a private socket (see
  `tests/tmux_e2e`), but not in every tmux and terminal combination.
- There is no active terminal query: a terminal that neither tmux nor the
  environment names is not probed. As root there is no tmux detection (see
  Security).
- The remote framebuffer is limited to 16 million pixels. The clipboard and the
  mouse cursor shape are not forwarded.
- A direct VNC endpoint, an OpenStack server's noVNC console and an OCI
  instance's graphical console are supported. The OCI viewer is Linux only
  (it needs `/proc/net/unix`).
- The OpenStack and OCI paths are tested end to end against a fake CLI and a
  fake proxy (WebSocket and RFB server for OpenStack, `oci` and `ssh` scripts
  with an RFB server on a Unix socket for OCI), on loopback only; neither has
  been run against a real cloud. The OpenStack path needs a console proxy that
  offers the `binary` WebSocket subprotocol (noVNC's websockify does).
- A real OCI service may behave differently from the fake in ways the tests
  cannot show: the tunnel's output, how long a console slot stays taken, and
  how long a connection takes to become active.
