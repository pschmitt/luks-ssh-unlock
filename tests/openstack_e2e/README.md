# OpenStack console end-to-end fixture

Drives the real `luks-ssh-unlock tui` dashboard against a fake OpenStack: no
cloud, credentials or real console is involved, and everything stays on
loopback.

```
dashboard (in tmux, Kitty graphics)
  -> fake `openstack` CLI (`console url show --novnc|--serial`, fresh single-use token each call)
  -> loopback WebSocket proxy (server.py, `binary` subprotocol)
       /websockify?token=T  built-in RFB server (320x200, moving bar, key-reactive stripe)
       /?token=T            serial console that echoes lines
```

## What it checks (32 checks)

- Serial: the dashboard shows the serial view first, calls the CLI with
  `--os-cloud`, `console url show --serial`, the server and the clouds file,
  round-trips a typed line, and restarts with Ctrl+R on a fresh token.
- Graphical: F6 switches to the VNC view, the `--novnc` URL is fetched, the RFB
  handshake runs, a complete cell-aligned RGBA image is transmitted over the
  Kitty protocol, the picture keeps updating, it is shown with an explicit
  placement of a live image (no placeholder cells), keys reach the desktop
  and change the next frame, Ctrl+T makes the view read-only, Ctrl+R
  reconnects with a new token.
- Failure path: a failing CLI is reported in the pane and the dashboard lives.
- Token hygiene: tokens are distinct and single use (a replay is refused), and
  no token shows up in the pane, the terminal stream, any log or any process
  argument list.
- SIGTERM leaves no `console openstack` child behind and the dashboard exits.

## Dependencies

- Python 3 (standard library only) and `tmux`.
- An `xterm-kitty` terminfo entry (`infocmp xterm-kitty` must succeed), for
  example from the `kitty` terminfo package. Otherwise point `KITTY_TERMINFO`
  at a directory that contains it.
- A built `luks-ssh-unlock` binary (debug is fine).

The fixture writes its fake hosts, key, `clouds.yaml`, CLI and logs into a
temporary directory; the key and the LUKS passphrase in it are dummies.

## Running it

Build on a build host as usual (never locally for a full build), then:

```
cargo build
python3 tests/openstack_e2e/run.py
```

Environment: `LSU_BIN` (binary; default `$CARGO_TARGET_DIR` or `target`, then
`debug/luks-ssh-unlock`), `KITTY_TERMINFO`, `LSU_FX_PORT` (default: a free
port), `KEEP=1` (keep the temporary directory). `run.py explore` just starts
the dashboard and prints the screen and logs, which helps when writing new
checks.

Exit status is 0 when all checks pass, 1 when one fails and 77 (skip) when
tmux, the binary or the kitty terminfo is missing. A run takes about a minute.
It is not part of `cargo test`: it needs tmux and a terminfo entry that CI
images may lack, and it is timing based (it waits on screen contents, with
generous limits).

## Limits

- The RFB side is a small fake, not a real VNC server: it covers raw
  encoding and SetPixelFormat, not Tight/ZRLE or clipboard.
- The Kitty output is parsed from the byte stream, not rendered by a real
  terminal, so image quality and placement on screen are not judged.
- It proves the OpenStack path against fakes. It does not show that a real
  cloud's console proxy behaves the same.
