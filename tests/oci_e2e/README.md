# OCI graphical console end-to-end fixture

Drives the real `luks-ssh-unlock tui` dashboard against a fake Oracle Cloud:
no cloud, credentials or real console is involved, and nothing leaves the
machine (not even loopback TCP: the tunnel is a Unix socket).

```
dashboard (in tmux, Kitty graphics, PATH = fixture bin/ first)
  -> fake `oci` CLI   (bin/oci: instance lookup, console connection create/get/delete)
  -> fake `ssh`       (bin/ssh -> fake_ssh.py: listens on the -L Unix socket,
                       built-in RFB server: 320x200, moving bar, key-reactive stripe)
```

The target's `<name>.vnc.json` is `{"type": "oci", ...}` with a dummy RSA
identity (a file and its `.pub`). Every `ssh` call that is not the console
tunnel is handed to the real `ssh`.

## What it checks

`run.py all` (main scenario, static-frame resize, real PTY hangup, then SIGKILL):

- The graphical view starts on its own, looks the instance up by the target's
  name, creates one tagged console connection with our public key, reads that
  connection, and starts one `ssh` tunnel into a Unix socket in a mode 0700
  `lsu-vnc-*` directory under `XDG_RUNTIME_DIR` (socket mode 0600).
- The RFB handshake runs over it, a complete cell-aligned RGBA image reaches
  the terminal through the Kitty protocol (zlib-compressed), the picture keeps
  updating, it is shown with an explicit placement of a live image (no
  placeholder cells), keys reach the
  instance and change the next frame, Ctrl+T makes the view read-only.
- A static fake framebuffer remains available through a forced tmux resize and
  redraw; this is the non-animated source fixture for separate Kitty screenshot
  validation (`run.py static`).
- Closing the master side of a real PTY after the TUI has started terminates the
  dashboard promptly (`run.py pty`), after allowing Crossterm to enter its event
  poll; this exercises EOF handling and process cleanup rather than only the
  closed-pipe helper.
- Ctrl+R creates a new connection and tunnel, deletes the old connection (only
  it, with `--force`), ends the old tunnel and removes its socket directory.
- A console that is in use (the tunnel's `administratively prohibited`) is
  reported with the fixed message, never with `ssh`'s output; a failing `oci`
  CLI is reported and the dashboard lives; a restart afterwards works.
- Neither the identity's contents nor the LUKS passphrase show up in the pane,
  the terminal stream, stderr or any log, and the connection string does not
  reach the pane.
- SIGTERM: the dashboard exits, the tunnel is gone, the last connection is
  deleted, no socket directory is left.
- SIGKILL (`run.py kill`): the helper process still deletes the connection and
  the tunnel `ssh` ends with the dashboard.

## Dependencies

- Linux (the OCI viewer needs `/proc/net/unix`), Python 3 (standard library
  only), `tmux`, and `ssh` on PATH.
- An `xterm-kitty` terminfo entry (`infocmp xterm-kitty` must succeed), for
  example from the `kitty` terminfo package; otherwise point `KITTY_TERMINFO`
  at a directory that contains it.
- A built `luks-ssh-unlock` binary (debug is fine).

## Running it

Build on a build host as usual (never locally for a full build), then:

```
cargo build
python3 tests/oci_e2e/run.py          # all, as above
python3 tests/oci_e2e/run.py kill     # only the SIGKILL scenario
python3 tests/oci_e2e/run.py static   # static framebuffer through tmux resize
python3 tests/oci_e2e/run.py pty      # real PTY master hangup lifecycle
python3 tests/oci_e2e/run.py explore  # start it and print the screen and logs
```

Environment: `LSU_BIN` (binary; default `$CARGO_TARGET_DIR` or `target`, then
`debug/luks-ssh-unlock`), `KITTY_TERMINFO`, `KEEP=1` (keep the temporary
directory with the logs `oci.log`, `ssh.log`, `deleted.log`).

Exit status is 0 when all checks pass, 1 when one fails and 77 (skip) when
tmux, the binary, ssh or the kitty terminfo is missing. A run takes a couple of
minutes. It is not part of `cargo test` (it needs tmux and a terminfo entry that
CI images may lack, and it is timing based).

## Limits

- It proves the OCI path against fakes. A real OCI service may differ in the
  tunnel's output, how long a console slot stays taken after a client leaves,
  and how long a connection takes to become active. The fake `ssh` does not
  speak SSH and checks no host key or key.
- The RFB side is a small fake (raw encoding, SetPixelFormat), copied from the
  OpenStack fixture's server (`../openstack_e2e/server.py`), not a real VNC
  server.
- The Kitty output is parsed from the byte stream, not rendered by a real
  terminal. This PTY fixture cannot generate genuine pointer reports; mouse
  clicks and held-button drags need the separate real Kitty/Xvfb visual harness.
