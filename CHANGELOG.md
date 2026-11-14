# Changelog

## 0.2.0 (unreleased)

Rewritten in Rust. `luks-ssh-unlock.sh` and `dhcp-listener.py` are replaced
by one binary; `initrd-checksum.sh` stays a shell script because it runs on
the target. The command line, the environment variables, the NixOS module
options and the container interface are compatible.

### New

- `tui`: embedded VNC console support uses an in-process RFB client and Kitty
  graphics renderer, including tmux passthrough support.
- `tui`: typed per-target console settings support OCI and OpenStack serial
  consoles and command consoles. OpenStack graphical consoles are also
  supported, including through the NixOS module's `vnc.type = "openstack"`.
  OCI connection management runs in LSU using the OCI CLI and OpenSSH; only
  the connection created by LSU is removed on exit.
- `tui`: restore terminal state and clean up owned Kitty images on SIGTERM,
  SIGHUP or terminal-input hangup as well as normal exit.

- `tui`: OCI graphical consoles are supported through the built-in viewer and
  the NixOS module's `vnc.type = "oci"`.
  It requests a console connection, validates the `vnc-connection-string`
  strictly, rebuilds the `ssh` command and tunnels port 5900 of the instance
  to a Unix socket in a private (`0700`) directory, so that other local users
  cannot take the console (OCI serves one client at a time). The caller gets a
  connected stream from `VncSession::connect`, after the socket is checked to
  be ours and served by our `ssh` (`SO_PEERCRED`). Readiness is read from
  `/proc/net/unix` (a probe connection would use OCI's single client slot),
  and the tunnel, its socket directory and the connection are cleaned up on
  normal close or cancellation. A crash can leave the socket directory behind.

- `tui`: interactive consoles. An extra command marked with `@`
  (`--extra-command 'oci=@COMMAND'`, or in `tui-commands`) runs on a real
  pseudo-terminal of the pane's size, shown through a terminal emulator:
  prompts without a newline, line editing, colors, resizing and scrollback
  (`Shift`+`PageUp`, the wheel) work. While the console pane has the focus it
  captures the keyboard, so `q`, `Esc`, `Ctrl+C` and the arrows reach the
  program. `Ctrl+]` gives the keyboard back to the dashboard (so do
  `Shift`+arrows and clicking another pane), `Ctrl+R` and `F5` restart the
  console. Commands without `@` stream line by line as before.
- `tui`: the target details moved from a pane into a dialog (`i`), which
  leaves more room for the activity and the extra command pane. In wide
  terminals the console spans the full width below the targets and activity;
  healthcheck details remain available in the target details dialog.
- `tui`: `/` filters the targets by name or host as you type (`Enter` keeps
  the filter, `Esc` clears it); `--filter TEXT` starts with one. `--extra-command '[LABEL=]COMMAND'`
  (repeatable) adds an optional pane below the activity that shows
  the selected target's output of that command, streamed live with its
  colors (`{host}` and `{name}` are substituted); `c` switches between
  commands, `C` shows it full screen, and the pane's border can be dragged.
  `?` shows the keys, and `S` stops or starts the target's service. A target can override
  or remove them in `<name>.tui-commands` (NixOS: instance option `tuiCommands`).
- `tui`: dashboard of all targets (`/etc/luks-ssh-unlock/*.env` by default)
  with live state, details and activity, and confirmed
  unlocks and forced unlocks. The module's per-instance command gains `tui`.
  The dashboard supports the mouse: click to select a target, double-click
  to unlock it (after confirmation), and scroll the target list and the
  activity. The activity pane shows the service's journal, live, together
  with the dashboard's own actions (every check result on state changes and
  manual refreshes, unlocks), so it is no longer empty before the first
  unlock. The dashboard re-checks a target
  as soon as its service logs something, instead of waiting for the next
  periodic check. It warns (in the list, the header and the details) when a
  target's systemd service is stopped, failed, disabled or masked.
  On a narrow or tall terminal (a phone) the panes are stacked and the header
  and key hints are shortened. The target list is sized to its content, the
  borders between panes can be dragged with the mouse, `Shift`+arrows or `Tab`
  move the focus between panes, and `l` shows the activity full screen.
  The details show the healthcheck result as a green or red dot next to the
  state, instead of repeating the command; the activity marks service lines
  (magenta) and dashboard lines (blue) with different colors.
- `tui` and the provider consoles never run as root and never re-run under
  `sudo`; `tui` refuses to start as root. OCI and OpenStack use the invoking
  user's own keys and environment. Targets whose configuration, key or
  passphrase only root can read are served by the new privileged helper
  (`luks-ssh-unlock helper serve`, socket-activated per connection by the NixOS
  option `services.luks-ssh-unlock.helper`, with a basic and an admin group).
  It answers with closed states and coarse error codes only, never a secret.
  See "Privileged helper" in `docs/usage.md` for the setup, limitations and a
  recipe to test with a real configuration. Other commands keep the `sudo`
  re-run when interactive (`--no-sudo` or `LUKS_SSH_UNLOCK_NO_SUDO=1` disable
  it); without a terminal nothing changes.
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

- `tui`: a target can open with its graphical console: `"default": true` in
  its `vnc.json` or `console.json`, from the NixOS module's per-instance
  `defaultView` or the global `tui.defaultView`.
- `tui`: keys reach QEMU-based VNC consoles (OpenStack, OCI) as the scancodes
  that type each character in the guest's keyboard layout (QEMU's extended key
  event). A guest with a German console layout no longer gets `y` for `z` or
  wrong symbols. The layout is the VNC parameter `keymap` (`de` or `us`), by
  default the dashboard machine's console layout.
- `tui`: OCI consoles make room for themselves. OCI allows one console
  connection per instance, so a leftover or the other view's connection made a
  new one hang on "requesting a new console connection". The other
  connections are now deleted (setting `closeOthers`, on by default; `Ctrl+R`
  always does it), and with `closeOthers = false` LSU says the instance is busy
  at once instead of retrying for minutes.
- `tui`: the OpenStack serial console falls back to the console log
  (`openstack console log show`, polled, only new lines) when the server has no
  serial console; per target `mode = "auto"` (default), `"serial"` or `"log"`.
- `tui`: every console view (VNC, serial, console log, commands) shows the
  same loading screen (spinner, current step, elapsed time, a hint when it
  takes long) until the console is there. LSU's own console commands report
  their steps to it instead of printing them into the pane.
- `tui`: panes are framed by their title line only, and the pane hints sit in
  that line, so consoles and pictures get the space the boxes took.
- `tui`: no permanent target list. The top line names the selected target and
  its state and counts the fleet's states; `Ctrl+P` or `/` (or a click on the
  top line) opens a host switcher popup (type to filter, `Enter` switches,
  `Esc` goes back). The console and the activity take the full width, the
  console on top.
- `tui`: the streamed OpenStack console log keeps its colours: Nova drops the
  ESC bytes of the guest's escape sequences, LSU puts them back for colours
  and drops what is left of every other control sequence, redraws and lines
  with nothing to show.
- `tui`: in tmux, the VNC picture leaves with its window. Explicit Kitty
  placements are the terminal's, and tmux drops graphics sent from a hidden
  pane (`allow-passthrough on`), so the picture stayed over the other window;
  the delete now goes straight to the terminals that showed the pane.
- `tui`: the console takes 80% of the space by default (the border can still
  be dragged), beside the activity on a wide terminal and above it on a
  narrow or tall one.
- `tui`: a console that only shows output (the OpenStack console log) is
  marked READ-ONLY and never takes the keyboard; nothing typed is echoed.
- helper: the sockets accept 64 connections, 32 per user (were 16 and 8).
  Two dashboards checking every target at once went over the limit, systemd
  dropped connections, and checks failed with "the helper gave an unexpected
  answer". A connection closed unanswered is now reported as such, and the
  read-only requests (list, check) are tried again a few times.
- `tui`: the details dialog (`i`) of a target behind the privileged helper
  shows the route, LUKS method, healthcheck, uptime and baseline dates. The
  helper sends them as values (filtered names, numbers, times), never as text
  that came from the target or a command.

### Fixed

- `tui`: VNC uses explicit Kitty placements in tmux, including normal panes,
  avoiding blank frames from rapidly changing virtual placeholders. Placements
  remain pane-sized and are redrawn when the tmux layout changes. On pane resize,
  old placements are deleted in the same synchronized frame as their replacements
  to avoid a blank interval.
- `tui`: VNC frames are sent to the terminal zlib-compressed (Kitty `o=z`), as
  RGB instead of RGBA for tmux virtual placements. A typical console frame
  shrinks from about 4 MB to well under 100 kB, so a typed character no
  longer queues dozens of 64 kB writes through tmux before it shows.
- `tui`: an unchanged VNC picture is no longer placed again on every pass of
  the event loop (about 50 times a second).
- `tui`: when the terminal or tmux layout changes, the VNC picture stays up
  until its rescaled replacement is placed, instead of being deleted first and
  leaving the pane blank during the upload.
- `tui`: VNC scaling averages source pixel coverage when shrinking and
  interpolates between pixels when enlarging, rather than selecting only
  nearest-neighbor samples. The viewer uses terminal-reported cell dimensions
  for aspect fitting and pointer mapping, with a safe fallback when unavailable;
  the captured-key footers only advertise F6 when another view is available.
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
- `nixosModules.default` can be imported by several modules of one system;
  it is imported once.
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
