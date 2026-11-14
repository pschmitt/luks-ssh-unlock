# tmux and terminal detection fixture

Runs the real `luks-ssh-unlock tui` dashboard in a real tmux server (private
socket) with the fake OCI cloud of `../oci_e2e`, and varies what tmux can
learn about the terminal that shows the picture. Nothing leaves the machine.

## What it checks

- A client whose `TERM` is generic (`screen-256color`, `xterm-256color`) is
  recognised through the client's own environment (`KITTY_WINDOW_ID`,
  `WEZTERM_PANE`, ...), read by the unprivileged dashboard; a client that
  says nothing is refused with a message that names the TERM it reported and
  `LUKS_SSH_UNLOCK_GRAPHICS`, and no console connection is made.
- `LUKS_SSH_UNLOCK_GRAPHICS=kitty` accepts such a terminal.
- Terminal hints in the pane's own environment never stand for the client.
- `allow-passthrough` `on` and `all` both work; it is read per pane (pane,
  window and global scope); `off` gives a message that names the option and
  the fix, and Ctrl+R picks up a changed setting without a restart.
- Without `TMUX` in the environment (as after `sudo`) a generic TERM is not
  guessed. With passwordless `sudo`, the dashboard started as root draws
  nothing and makes no connection (it must stay unprivileged); skipped
  otherwise.

## Running it

```
cargo build
python3 tests/tmux_e2e/run.py          # all
python3 tests/tmux_e2e/run.py explore  # EXPLORE_TERM=xterm-256color: show the screen
```

Needs what `../oci_e2e` needs (tmux, ssh, an `xterm-kitty` terminfo entry or
`KITTY_TERMINFO`, a built binary via `LSU_BIN`). Exit status 0 passes, 1
fails, 77 prerequisites missing.
