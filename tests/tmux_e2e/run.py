"""Terminal and tmux detection of the built-in graphical viewer, against a real
tmux server on a private socket. Everything is local and fake; no real
console is contacted. Python standard library only, plus tmux. See README.md.

The dashboard runs in a tmux pane with the fake `oci` CLI and `ssh` of
../oci_e2e (a loopback RFB server behind a Unix socket), and a pseudo-terminal
client is attached to the session. What is varied is what tmux can learn about
that client: its terminal name, its environment, and `allow-passthrough` at
global, window and pane scope.

usage: python3 run.py [all|explore]

environment:
  LSU_BIN          the luks-ssh-unlock binary (default: $CARGO_TARGET_DIR or
                   <repo>/target, then debug/luks-ssh-unlock)
  KITTY_TERMINFO   directory holding the xterm-kitty terminfo entry
  KEEP=1           keep the temporary directories

exit status: 0 all checks passed, 1 a check failed, 77 prerequisites missing.
"""
import fcntl, importlib.util, os, pty, re, shutil, signal, struct, subprocess, sys, termios, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("oci_run", os.path.join(os.path.dirname(HERE), "oci_e2e", "run.py"))
oci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oci)

check, RESULTS, last_images, BIN, TERMINFO = oci.check, oci.RESULTS, oci.last_images, oci.BIN, oci.TERMINFO

# What a terminal puts into the environment of the programs started in it.
HINTS = ("KITTY_WINDOW_ID", "WEZTERM_PANE", "WEZTERM_EXECUTABLE", "GHOSTTY_RESOURCES_DIR", "GHOSTTY_BIN_DIR",
         "KONSOLE_VERSION", "KONSOLE_DBUS_SERVICE", "TERM_PROGRAM", "TMUX", "TMUX_PANE", "LUKS_SSH_UNLOCK_GRAPHICS")


def clean_env():
    return {k: v for k, v in os.environ.items() if k not in HINTS}


def prerequisites():
    why = oci.prerequisites()
    if why:
        return why
    return None


def sudo_works():
    return shutil.which("sudo") is not None and subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0


def flat(text):
    return re.sub(r"\s+", " ", text)


class Tx(oci.Fx):
    """The OCI fixture with the terminal as a parameter."""

    def __init__(self, client_term="xterm-kitty", client_env=None, global_pt="on", pane_env=None, dash_env=(), wrap=""):
        super().__init__()
        self.client_term, self.client_env = client_term, client_env or {}
        self.global_pt, self.pane_env, self.dash_env, self.wrap = global_pt, pane_env or {}, tuple(dash_env), wrap

    def tmux(self, *a, check=True):
        r = subprocess.run(["tmux", "-S", self.sock, "-f", "/dev/null", *a], capture_output=True, text=True, env=clean_env())
        if check and r.returncode:
            raise RuntimeError(f"tmux {a}: {r.stderr}")
        return r.stdout

    def start(self):
        self.write_files()
        path = f"{self.dir}/bin:{os.environ.get('PATH', '/usr/bin:/bin')}"
        env = " ".join([*self.dash_env, f"PATH={path}", f"XDG_RUNTIME_DIR={self.run_dir}", f"HOME={self.dir}/home"])
        cmd = f"{self.wrap}env {env} {BIN} tui --no-sudo -k /dev/null --refresh 3600 {self.dir}/hosts 2>{self.dir}/lsu.err"
        extra = []
        for k, v in self.pane_env.items():
            extra += ["-e", f"{k}={v}"]
        self.tmux("new-session", "-d", "-x", str(self.cols), "-y", str(self.rows), "-s", "lsu", "-e", "TERM=tmux-256color", *extra, cmd)
        if self.global_pt is not None:
            self.tmux("set", "-g", "allow-passthrough", self.global_pt)
        self.tmux("set", "-g", "escape-time", "0")
        self.tmux("set", "-ga", "terminal-features", ",xterm-kitty:RGB")
        self.tmux("set", "-g", "remain-on-exit", "on")
        child_env = clean_env()
        pid, fd = pty.fork()
        if pid == 0:
            os.environ.clear()
            os.environ.update(child_env)
            os.environ["TERM"] = self.client_term
            os.environ.update(self.client_env)
            if TERMINFO:
                os.environ["TERMINFO"] = TERMINFO
            os.execvp("tmux", ["tmux", "-S", self.sock, "attach", "-t", "lsu"])
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        try:
            os.kill(pid, signal.SIGWINCH)
        except ProcessLookupError:
            pass
        threading.Thread(target=self._read, daemon=True).start()
        time.sleep(1.5)


def run(label, fn, **kw):
    fx = Tx(**kw)
    try:
        fx.start()
        fn(fx)
    finally:
        fx.stop()
        if os.environ.get("KEEP"):
            print("kept", fx.dir)
        else:
            fx.cleanup()


def picture(fx, limit=20):
    return fx.wait(lambda: len(last_images(fx)) >= 2, limit)


def no_picture(fx, wait=6):
    """The dashboard never sent an image, and never connected."""
    time.sleep(wait)
    return not last_images(fx) and "instance-console-connection create" not in fx.log("oci.log")


def pane(fx):
    return flat(fx.capture())


def says(fx, *words, limit=12):
    return fx.wait(lambda: all(w in pane(fx) for w in words), limit)


def restart(fx):
    """Ctrl+R restarts the viewer, and only while the console has the keyboard."""
    fx.keys("S-Down")
    fx.keys("C-r")


def says_any(fx, *words, limit=12):
    return fx.wait(lambda: any(w in pane(fx) for w in words), limit)


def scenarios():
    # --- the named terminal is not what identifies it behind tmux ---------
    def t1(fx):
        check("T1 client TERM=xterm-kitty (the plain case): the picture arrives", picture(fx))
    run("t1", t1)

    def t2(fx):
        check("T2 client TERM=screen-256color, KITTY_WINDOW_ID in the client's environment: the picture arrives", picture(fx), pane(fx)[:200])
    run("t2", t2, client_term="screen-256color", client_env={"KITTY_WINDOW_ID": "1"})

    def t3(fx):
        check("T3 client TERM=xterm-256color, WezTerm in the client's environment: the picture arrives", picture(fx), pane(fx)[:200])
    run("t3", t3, client_term="xterm-256color", client_env={"WEZTERM_PANE": "0"})

    def t4(fx):
        ok = says(fx, "not known to show images", "xterm-256color", "LUKS_SSH_UNLOCK_GRAPHICS")
        check("T4 a client that says nothing but xterm-256color gets a message naming that TERM and the override, and no picture", ok and no_picture(fx), pane(fx)[:240])
    run("t4", t4, client_term="xterm-256color")

    def t5(fx):
        check("T5 ... and LUKS_SSH_UNLOCK_GRAPHICS=kitty accepts it", picture(fx), pane(fx)[:200])
    run("t5", t5, client_term="xterm-256color", dash_env=["LUKS_SSH_UNLOCK_GRAPHICS=kitty"])

    def t6(fx):
        check("T6 hints in the PANE's environment do not stand for the client's terminal", says(fx, "not known to show images") and no_picture(fx), pane(fx)[:200])
    run("t6", t6, client_term="screen-256color", pane_env={"KITTY_WINDOW_ID": "1", "TERM_PROGRAM": "ghostty"})

    # --- allow-passthrough: on, all, and the scope it is set at ----------
    def p1(fx):
        check("P1 allow-passthrough all (global) works", picture(fx))
    run("p1", p1, global_pt="all")

    def p2(fx):
        check("P2 allow-passthrough off: the message names the option and the fix, no picture", says(fx, "allow-passthrough", "`off`") and no_picture(fx), pane(fx)[:240])
        fx.tmux("set-option", "-p", "-t", "lsu:0", "allow-passthrough", "on")
        restart(fx)
        check("P3 setting it for the pane and pressing Ctrl+R brings the picture (pane scope, no restart)", picture(fx, 25), pane(fx)[:200])
    run("p2", p2, global_pt="off")

    def p5(fx):
        # the console opened with passthrough on; now close it for this pane only
        picture(fx)
        m, creates = fx.mark(), fx.log("oci.log").count("instance-console-connection create")
        fx.tmux("set-option", "-p", "-t", "lsu:0", "allow-passthrough", "off")
        restart(fx)
        ok = says(fx, "allow-passthrough", "`off`")
        time.sleep(4)
        quiet = not last_images(fx, m) and fx.log("oci.log").count("instance-console-connection create") == creates
        check("P4 allow-passthrough on globally but off for the pane: the pane's setting counts (message, no new picture, no new connection)", ok and quiet, pane(fx)[:240])

    fx = Tx(global_pt="on")
    try:
        fx.start()
        p5(fx)
    finally:
        fx.stop()
        fx.cleanup()

    def p6(fx):
        check("P5 allow-passthrough set only for the window works (global off)", picture(fx, 25), pane(fx)[:200])
    fx = Tx(global_pt="off")
    try:
        fx.start()
        fx.tmux("set-option", "-w", "-t", "lsu:0", "allow-passthrough", "all")
        restart(fx)
        p6(fx)
    finally:
        fx.stop()
        fx.cleanup()

    # --- no tmux (sudo drops TMUX and TMUX_PANE) --------------------------
    def e1(fx):
        ok = says(fx, "not known to show images")
        check("E1 without TMUX in the environment (as after sudo) a generic TERM gets the plain message and no picture", ok and no_picture(fx), pane(fx)[:200])
    run("e1", e1, client_term="xterm-kitty", dash_env=["-u", "TMUX", "-u", "TMUX_PANE", "TERM=tmux-256color"])

    # --- really elevated ---------------------------------------------------
    if sudo_works():
        def r1(fx):
            ok = says_any(fx, "privileged", "root")
            check("R1 elevated inside tmux (TMUX kept): refuses or says it cannot ask tmux as a privileged process; no picture, no connection", ok and no_picture(fx), pane(fx)[:240])
        run("r1", r1, wrap="sudo -n ", dash_env=["TMUX=$TMUX", "TMUX_PANE=$TMUX_PANE"])

        def r2(fx):
            ok = says_any(fx, "not known to show images", "privileged", "root")
            check("R2 elevated with the environment dropped (plain sudo): fails safe (plain message or refusal); no picture, no connection", ok and no_picture(fx), pane(fx)[:240])
        run("r2", r2, wrap="sudo -n ")
    else:
        print("SKIP R1/R2: passwordless sudo is not available here")


def explore():
    fx = Tx(client_term=os.environ.get("EXPLORE_TERM", "xterm-256color"))
    try:
        fx.start()
        time.sleep(5)
        print(fx.capture())
        print(open(fx.dir + "/lsu.err").read())
    finally:
        fx.stop()
        fx.cleanup()


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "all"
    why = prerequisites()
    if why:
        print("SKIP: " + why, file=sys.stderr)
        sys.exit(77)
    if mode == "explore":
        explore()
    else:
        scenarios()
        bad = [r for r in RESULTS if not r[1]]
        print(f"\n{len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed")
        sys.exit(1 if bad else 0)
