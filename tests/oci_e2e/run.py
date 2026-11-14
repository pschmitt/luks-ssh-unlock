"""End-to-end fixture: dashboard -> fake `oci` CLI -> fake `ssh` tunnel (a Unix
socket with a built-in RFB server). Everything is local and fake; no real
console is contacted. Python standard library only, plus tmux. See README.md.

usage: python3 run.py [all|kill|static|pty|explore]

  all      main scenario, static-frame redraw, PTY hangup, and SIGKILL cleanup
  kill     the dashboard is killed with SIGKILL: its helper must still delete
           the console connection
  static   hold a fake framebuffer steady through a tmux resize/redraw
  pty      close a real PTY master and require the TUI to exit
  explore  start the dashboard and print the screen and the logs

environment:
  LSU_BIN          the luks-ssh-unlock binary (default: $CARGO_TARGET_DIR or
                   <repo>/target, then debug/luks-ssh-unlock)
  KITTY_TERMINFO   directory holding the xterm-kitty terminfo entry, only
                   needed when `infocmp xterm-kitty` does not find it
  KEEP=1           keep the temporary directory (logs, fake CLIs) for a look

exit status: 0 all checks passed, 1 a check failed, 77 prerequisites missing.
"""
import base64, zlib, fcntl, glob, json, os, pty, re, select, shutil, signal, stat, struct, subprocess, sys, tempfile, termios, threading, time, unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(REPO, "src")
BIN = os.environ.get("LSU_BIN") or os.path.join(
    os.environ.get("CARGO_TARGET_DIR") or os.path.join(REPO, "target"), "debug", "luks-ssh-unlock"
)
TERMINFO = os.environ.get("KITTY_TERMINFO")
APC = re.compile(rb"\x1b_G([^;\x1b]*)(?:;([^\x1b]*))?\x1b\\")
SGR = re.compile(r"\x1b\[[0-9;:]*m")

CONN = "ocid1.instanceconsoleconnection.oc1.eu-frankfurt-1.own"
INST = "ocid1.instance.oc1.eu-frankfurt-1.inst"
OTHER = "ocid1.instance.oc1.eu-frankfurt-1.other"
HOST = "instance-console.eu-frankfurt-1.oci.oraclecloud.com"
COMP = "ocid1.compartment.oc1..comp"
KEY_MATERIAL = "FIXTURE-PRIVATE-KEY-MATERIAL-9f3c"
PASSPHRASE = "fixture-not-a-secret"


def prerequisites():
    """Return a reason the fixture cannot run here, or None."""
    if not sys.platform.startswith("linux"):
        return "the OCI viewer is Linux only"
    if shutil.which("tmux") is None:
        return "tmux is not installed"
    if not (os.path.isfile(BIN) and os.access(BIN, os.X_OK)):
        return f"binary not found: {BIN} (build it, or set LSU_BIN)"
    env = dict(os.environ, **({"TERMINFO": TERMINFO} if TERMINFO else {}))
    if subprocess.run(["infocmp", "xterm-kitty"], env=env, capture_output=True).returncode:
        return "no xterm-kitty terminfo (install kitty's terminfo, or set KITTY_TERMINFO)"
    if shutil.which("ssh") is None:
        return "no ssh on PATH (the fake delegates other calls to it)"
    return None


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (" :: " + detail if detail else ""), flush=True)


DIAC_SRC = open(SRC + "/vnc_graphics/kitty.rs").read()
DIAC = [chr(int(x, 16)) for x in re.findall(r"'\\u\{([0-9a-f]+)\}'", DIAC_SRC.split("const DIACRITICS")[1].split("];")[0])]
assert len(DIAC) == 128


def events(data):
    ev, cur = [], None
    for m in APC.finditer(data):
        keys = {}
        for kv in m.group(1).decode().split(","):
            if "=" in kv:
                a, b = kv.split("=", 1)
                keys[a] = b
        payload = m.group(2) or b""
        if keys.get("a") in ("T", "t"):
            cur = {"t": "T", "keys": keys, "b64": bytearray(payload)}
            if keys.get("m", "0") == "0":
                ev.append(cur)
                cur = None
        elif "a" not in keys and cur is not None:
            cur["b64"] += payload
            if keys.get("m", "0") == "0":
                ev.append(cur)
                cur = None
        elif "a" in keys:
            ev.append({"t": keys["a"], "keys": keys})
    return ev


def rgba(e):
    k = e["keys"]
    raw = base64.b64decode(bytes(e["b64"]))
    if k.get("o") == "z":
        raw = zlib.decompress(raw)
    if k.get("f") == "24":  # opaque RGB: expand to RGBA for the checks
        raw = b"".join(raw[i:i + 3] + b"\xff" for i in range(0, len(raw), 3))
    return int(k["s"]), int(k["v"]), raw


def placeholders(cap):
    cells = {}
    for ln, line in enumerate(cap.split("\n")):
        fg, col, i = None, 0, 0
        while i < len(line):
            m = SGR.match(line, i)
            if m:
                s = m.group(0)
                mm = re.search(r"38[;:]2[;:](\d+)[;:](\d+)[;:](\d+)", s)
                if mm:
                    fg = tuple(int(x) for x in mm.groups())
                elif s in ("\x1b[0m", "\x1b[m") or re.search(r"[\[;]39[;m]", s):
                    fg = None
                i = m.end()
                continue
            ch = line[i]
            i += 1
            j, marks = i, []
            while j < len(line) and unicodedata.category(line[j]) == "Mn":
                marks.append(line[j])
                j += 1
            i = j
            if ch == "\U0010eeee":
                cells[(ln, col)] = (marks, fg)
            col += 1
    return cells


def grid(cap):
    cells = placeholders(cap)
    rows = cols = bad = 0
    for marks, _fg in cells.values():
        if len(marks) < 2 or marks[0] not in DIAC or marks[1] not in DIAC:
            bad += 1
            continue
        rows = max(rows, DIAC.index(marks[0]) + 1)
        cols = max(cols, DIAC.index(marks[1]) + 1)
    return len(cells), rows, cols, bad


def write_exec(path, text):
    with open(path, "w") as f:
        f.write(text)
    os.chmod(path, 0o755)


class Fx:
    def __init__(self, cols=120, rows=40, static=False):
        self.dir = tempfile.mkdtemp(prefix="ocfx")
        self.cols, self.rows = cols, rows
        self.static = static or os.environ.get("LSU_FX_STATIC") == "1"
        self.sock = self.dir + "/tmux.sock"
        self.out = bytearray()
        self.lock = threading.Lock()
        self.run_dir = self.dir + "/run"  # XDG_RUNTIME_DIR of the dashboard
        for d in ("hosts", "bin", "run", "home"):
            os.mkdir(f"{self.dir}/{d}", 0o700)

    # ---- fixture files
    def write_files(self):
        d = self.dir
        if self.static:
            open(d + "/static", "w").close()
        with open(d + "/hosts/t1.env", "w") as f:
            f.write(f"SSH_HOSTNAME=127.0.0.1\nSSH_PORT=1\nSSH_KEY={d}/key\nLUKS_PASSPHRASE={PASSPHRASE}\nSSH_INITRD_KNOWN_HOSTS=127.0.0.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureFixtureFixtureFixtureFixtureFixtureFix\n")
        open(d + "/key", "w").write("fixture\n")
        os.chmod(d + "/key", 0o600)
        # the OCI identity: an RSA key pair, as far as the CLI is concerned
        open(d + "/id_rsa", "w").write(f"-----BEGIN FIXTURE-----\n{KEY_MATERIAL}\n-----END FIXTURE-----\n")
        os.chmod(d + "/id_rsa", 0o600)
        open(d + "/id_rsa.pub", "w").write("ssh-rsa AAAAB3 test@host\n")
        json.dump(
            {"type": "oci", "parameters": {"identity": d + "/id_rsa", "profile": "FIXTURE", "configFile": d + "/oci-config-missing"}},
            open(d + "/hosts/t1.vnc.json", "w"),
        )
        json.dump(
            {"data": [{"id": INST, "display-name": "t1", "lifecycle-state": "RUNNING", "compartment-id": COMP}]},
            open(d + "/instances.json", "w"),
        )
        write_exec(
            d + "/bin/oci",
            f"""#!/bin/sh
# Fake `oci` CLI: a console connection per `create`, `get` says ACTIVE with
# the graphical connection string, `delete` is logged.
d={d}
echo "$*" >> $d/oci.log
case "$*" in
  *"instance-console-connection list"*)
    case "$*" in *"--query"*"luks-ssh-unlock"*) ;; *) exit 97 ;; esac
    exit 0 ;;
  *"instance-console-connection create"*)
    if [ -e $d/ocifail ]; then echo "ServiceError: NotAuthorizedOrNotFound (HTTP 404)" >&2; exit 1; fi
    n=$(($(cat $d/n 2>/dev/null || echo 0)+1)); echo $n > $d/n
    echo "{CONN}$n" ;;
  *"instance-console-connection get"*)
    id=$(printf '%s\\n' "$*" | sed -n 's/.*--instance-console-connection-id=\\([^ ]*\\).*/\\1/p')
    echo '{{"data":{{"id":"'$id'","lifecycle-state":"ACTIVE","vnc-connection-string":"ssh -o ProxyCommand='"'"'ssh -W %h:%p -p 443 '$id'@{HOST}'"'"' -N -L localhost:5900:{INST}:5900 {INST}"}}}}' ;;
  *"instance-console-connection delete"*) echo "$*" >> $d/deleted.log ;;
  *"compute instance list"*) cat $d/instances.json ;;
  *"compute instance get"*) echo '{COMP}' ;;
  *) echo unexpected >&2; exit 98 ;;
esac
""",
        )
        real_ssh = shutil.which("ssh")
        write_exec(
            d + "/bin/ssh",
            f"""#!/bin/sh
# exec: the listener of the tunnel socket must be this very process
LSU_FX_DIR={d} LSU_FX_REAL_SSH={real_ssh} exec {sys.executable} {HERE}/fake_ssh.py "$@"
""",
        )

    def tmux(self, *a, check=True):
        r = subprocess.run(["tmux", "-S", self.sock, "-f", "/dev/null", *a], capture_output=True, text=True)
        if check and r.returncode:
            raise RuntimeError(f"tmux {a}: {r.stderr}")
        return r.stdout

    def start(self):
        self.write_files()
        path = f"{self.dir}/bin:{os.environ.get('PATH', '/usr/bin:/bin')}"
        cmd = f"env PATH={path} XDG_RUNTIME_DIR={self.run_dir} HOME={self.dir}/home {BIN} tui --no-sudo -k /dev/null --refresh 3600 {self.dir}/hosts 2>{self.dir}/lsu.err"
        self.tmux(
            "new-session", "-d", "-x", str(self.cols), "-y", str(self.rows), "-s", "lsu",
            "-e", "TERM=tmux-256color", cmd,
        )
        self.tmux("set", "-g", "allow-passthrough", "on")
        self.tmux("set", "-g", "escape-time", "0")
        self.tmux("set", "-ga", "terminal-features", ",xterm-kitty:RGB")
        self.tmux("set", "-g", "remain-on-exit", "on")
        pid, fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = "xterm-kitty"
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

    def _read(self):
        while True:
            try:
                d = os.read(self.fd, 65536)
            except OSError:
                return
            if not d:
                return
            with self.lock:
                self.out += d

    def mark(self):
        with self.lock:
            return len(self.out)

    def since(self, m):
        with self.lock:
            return bytes(self.out[m:])

    def keys(self, *k, literal=False):
        a = ["send-keys", "-t", "lsu:0"] + (["-l"] if literal else [])
        self.tmux(*a, *k)

    def capture(self, esc=False, scrollback=False):
        a = ["capture-pane", "-p", "-t", "lsu:0"] + (["-e"] if esc else []) + (["-S", "-3000"] if scrollback else [])
        return self.tmux(*a)

    def wait(self, pred, limit=15.0, step=0.25):
        t0 = time.time()
        while time.time() - t0 < limit:
            if pred():
                return True
            time.sleep(step)
        return pred()

    def log(self, name):
        try:
            return open(f"{self.dir}/{name}").read()
        except OSError:
            return ""

    def lsu_pids(self):
        pid = self.tmux("display-message", "-p", "-t", "lsu:0", "#{pane_pid}").strip()
        kids = subprocess.run(["pgrep", "-P", pid], capture_output=True, text=True).stdout.split()
        return [int(k) for k in kids] or [int(pid)]

    def tunnels(self):
        """pids of the fake tunnel ssh processes that have been started."""
        return [int(p) for p in re.findall(r"pid=(\d+) tunnel call", self.log("ssh.log"))]

    def alive(self, pid):
        try:
            with open(f"/proc/{pid}/stat") as f:
                return f.read().rsplit(")", 1)[1].split()[0] != "Z"
        except OSError:
            return False

    def socket_dirs(self):
        return sorted(glob.glob(self.run_dir + "/lsu-vnc-*"))

    def stop(self):
        self.tmux("kill-server", check=False)
        for p in self.tunnels():
            if self.alive(p):
                try:
                    os.kill(p, signal.SIGKILL)
                except OSError:
                    pass
        try:
            os.close(self.fd)
        except (OSError, AttributeError):
            pass

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)


def title(fx):
    for ln in fx.capture().split("\n"):
        m = re.search(r"(vnc|console) · t1", ln)
        if m:
            return m.group(1)
    return None


def explore():
    fx = Fx()
    fx.start()
    try:
        time.sleep(4)
        print(fx.capture())
        print("title:", title(fx))
        print(fx.log("oci.log"), fx.log("ssh.log"))
        print(open(fx.dir + "/lsu.err").read())
    finally:
        fx.stop()
        fx.cleanup()


def near(c, t, tol=14):
    return all(abs(a - b) <= tol for a, b in zip(c, t))


def pixels(e, rows):
    w, h, raw = rgba(e)
    return w, h, [tuple(raw[(y * w + x) * 4:(y * w + x) * 4 + 3]) for y in rows for x in range(w)]


def last_images(fx, since=0):
    return [e for e in events(fx.since(since)) if e["t"] == "T"]


def view_model(fx, label):
    """tmux shows the picture with an explicit Kitty placement (not Unicode
    placeholder cells): the newest placement must show an image that was
    uploaded and not deleted since, at a non-empty cell size, and the pane must
    hold no stray placeholder cells."""
    live, placed = set(), None
    for e in events(fx.since(0)):
        k = e["keys"]
        if e["t"] == "T":
            live.add(k.get("i"))
        elif e["t"] == "d" and k.get("d") == "A":
            live.clear()
        elif e["t"] == "d" and k.get("d") == "I":
            live.discard(k.get("i"))
        elif e["t"] == "p":
            placed = (k.get("i"), int(k.get("c", 0)), int(k.get("r", 0)))
    n, rows, cols, bad = grid(fx.capture(esc=True))
    ok = placed is not None and placed[0] in live and placed[1] > 0 and placed[2] > 0 and n == 0
    check(label, ok, f"placement (i, c, r)={placed} live={sorted(live)} placeholder cells={n}")


def count(text, needle):
    return text.count(needle)


def calls(fx, word):
    return [l for l in fx.log("oci.log").splitlines() if word in l]


def scenario():
    fx = Fx()
    fx.start()
    try:
        check("O0 dashboard shows the graphical OCI view of the target", fx.wait(lambda: title(fx) == "vnc"), f"title={title(fx)}")
        check("O1 the picture arrives: Kitty transmits for the OCI-fed RFB", fx.wait(lambda: len(last_images(fx)) >= 2, 20), f"{len(last_images(fx))} transmits")
        log = fx.log("oci.log")
        lst = calls(fx, "compute instance list")
        check("O2 the instance is looked up by display name (the target name)", bool(lst) and "--display-name=t1" in lst[0], lst[0][:120] if lst else "no list call")
        cre = calls(fx, "instance-console-connection create")
        check("O3 one console connection is created for that instance with our key and tag", len(cre) == 1 and f"--instance-id={INST}" in cre[0] and f"--ssh-public-key-file={fx.dir}/id_rsa.pub" in cre[0] and '"luks-ssh-unlock"' in cre[0] and "--profile=FIXTURE" in cre[0], cre[0][:160] if cre else "no create call")
        check("O4 only that connection is read; nothing is listed or deleted yet", count(log, "instance-console-connection list") == 0 and count(log, "instance-console-connection delete") == 0 and f"--instance-console-connection-id={CONN}1" in log)
        sl = fx.log("ssh.log")
        tun = re.findall(r"tunnel call", sl)
        check("O5 one ssh tunnel was started, forwarding to a Unix socket (no TCP listener)", len(tun) == 1 and "listening on a unix socket" in sl, sl.strip().replace("\n", " | ")[:200])
        dirs = fx.socket_dirs()
        mode = stat.S_IMODE(os.stat(dirs[0]).st_mode) if dirs else None
        sock = glob.glob(dirs[0] + "/vnc.sock") if dirs else []
        smode = stat.S_IMODE(os.stat(sock[0]).st_mode) if sock else None
        check("O6 the socket lives in a private lsu-vnc-* directory (0700) under XDG_RUNTIME_DIR, mode 0600", len(dirs) == 1 and mode == 0o700 and smode == 0o600, f"dirs={len(dirs)} dir mode={mode and oct(mode)} sock mode={smode and oct(smode)}")
        check("O7 the RFB handshake ran over it", "vnc serverinit sent" in sl and "vnc setpixelformat" in sl)
        T = last_images(fx)
        if T:
            w, h, raw = rgba(T[-1])
            check("O8 payload is a complete cell-aligned RGBA image", len(raw) == w * h * 4 and w % 8 == 0 and h % 16 == 0, f"s={w} v={h}")
            mid = pixels(T[-1], [h // 2])[2]
            check("O9 decoded pixels come from the fake framebuffer (gradient with b~90)", any(80 <= p[2] <= 100 for p in mid), f"sample={mid[len(mid)//2]}")
        time.sleep(2.0)
        T2 = last_images(fx)
        top = lambda e: pixels(e, [2])[2]
        check("O10 the picture keeps updating (moving bar differs between transmits)", len(T2) >= 3 and any(top(a) != top(b) for a, b in zip(T2, T2[1:])), f"{len(T2)} transmits")
        view_model(fx, "O11 the picture is placed (direct placement of a live image)")
        stripe = lambda e: pixels(e, [rgba(e)[1] - 3])[2]
        check("O12 before the key the stripe is the initial colour", any(near(p, (200, 40, 40), 30) for p in stripe(last_images(fx)[-1])))
        fx.keys("S-Down")  # the console pane (the graphical view) takes the keyboard
        time.sleep(0.5)
        fx.keys("C")
        check("I0 C expands the graphical console to fullscreen", fx.wait(lambda: "Activity" not in fx.capture() and "vnc" in fx.capture()))
        fx.keys("a", literal=True)
        check("O13 keys reach the instance in fullscreen (RFB key event logged)", fx.wait(lambda: "vnc key down=1 a" in fx.log("ssh.log")))
        check("O13b the server takes scancodes, so the key goes with its scancode", "vnc key down=1 a sc=0x1e" in fx.log("ssh.log"), " | ".join(l for l in fx.log("ssh.log").splitlines() if "key down=1 a" in l))
        check("O14 and the new frame shows the reaction (stripe colour changed)", fx.wait(lambda: any(near(p, (4, 130, 70), 30) for p in stripe(last_images(fx)[-1]))), "stripe now ~(4,130,70)")
        # This fixture drives tmux through a PTY; writing SGR bytes to that PTY
        # does not generate a real Kitty mouse event. Pointer input is covered by
        # the Kitty/Xvfb visual regression instead.
        fx.keys("S-Up")
        check("I2 Shift+Up returns from fullscreen to the dashboard", fx.wait(lambda: "Activity" in fx.capture()))
        fx.keys("b", literal=True)
        time.sleep(0.4)
        check("I3 dashboard has the keyboard after leaving the console", "vnc key down=1 b" not in fx.log("ssh.log"))
        fx.keys("S-Down")
        fx.keys("C-t")
        time.sleep(0.8)
        fx.keys("b", literal=True)
        time.sleep(1.0)
        check("O15 Ctrl+T makes the view read-only: no key event for 'b'", "vnc key down=1 b" not in fx.log("ssh.log"))
        fx.keys("C-t")
        time.sleep(0.5)

        # --- restart: a new connection, the old one deleted and its tunnel gone
        old = fx.tunnels()[0]
        old_dirs = fx.socket_dirs()
        m = fx.mark()
        fx.keys("C-r")
        check("O16 Ctrl+R creates a second console connection and a second tunnel", fx.wait(lambda: len(calls(fx, "instance-console-connection create")) == 2 and len(fx.tunnels()) == 2, 20), f"creates={len(calls(fx, 'instance-console-connection create'))} tunnels={len(fx.tunnels())}")
        check("O17 the first connection (and only it) was deleted, with --force", fx.wait(lambda: f"--instance-console-connection-id={CONN}1 --force" in fx.log("deleted.log")) and f"{CONN}2" not in fx.log("deleted.log"), fx.log("deleted.log").strip()[-120:])
        check("O18 the first tunnel is gone and its socket directory was removed", fx.wait(lambda: not fx.alive(old) and not any(os.path.exists(d) for d in old_dirs), 10), f"alive={fx.alive(old)} dirs={fx.socket_dirs()}")
        check("O19 the picture is back after the restart", fx.wait(lambda: len(last_images(fx, m)) >= 1, 15))
        view_model(fx, "O20 the picture is placed after the restart")

        # --- a busy console is named, not guessed at
        open(fx.dir + "/busy", "w").close()
        fx.keys("C-r")
        check("O21 a console that is in use is reported in the pane with its fixed message", fx.wait(lambda: "in use by another client" in re.sub(r"\s+", " ", fx.capture()), 20), [l for l in fx.capture().split("\n") if "use" in l][:2].__repr__())
        os.remove(fx.dir + "/busy")
        m = fx.mark()
        fx.keys("C-r")
        check("O22 Ctrl+R after it is free shows the desktop again", fx.wait(lambda: len(last_images(fx, m)) >= 1, 20))

        # --- the CLI failing is reported, and the dashboard lives
        open(fx.dir + "/ocifail", "w").close()
        fx.keys("C-r")
        check("O23 a failing oci CLI is reported in the pane, dashboard still alive", fx.wait(lambda: bool(re.search(r"failed|could not|cannot|error|denied|not.?found|authorized", fx.capture(), re.I)), 15) and "t1" in fx.capture(), [l for l in fx.capture().split("\n") if re.search(r"failed|could not|cannot|error|authorized", l, re.I)][:2].__repr__())
        os.remove(fx.dir + "/ocifail")
        m = fx.mark()
        fx.keys("C-r")
        check("O24 and a restart afterwards works", fx.wait(lambda: len(last_images(fx, m)) >= 1, 20))

        # --- secret hygiene
        haystacks = {
            "pane scrollback": fx.capture(scrollback=True),
            "terminal stream": fx.since(0).decode("latin1"),
            "lsu stderr": fx.log("lsu.err"),
            "oci log": fx.log("oci.log"),
            "ssh log": fx.log("ssh.log"),
        }
        leaks = [f"{n}:{s}" for n, t in haystacks.items() for s in (KEY_MATERIAL, PASSPHRASE) if s in t]
        check("H1 neither the private key's contents nor the LUKS passphrase appear in the pane, the stream or any log", not leaks, str(leaks))
        check("H2 the connection string never reaches the pane or the stream", "ProxyCommand" not in haystacks["pane scrollback"] + haystacks["terminal stream"] + haystacks["lsu stderr"])

        # --- cleanup on SIGTERM
        live = [p for p in fx.tunnels() if fx.alive(p)]
        last = int(open(os.path.join(fx.dir, "n")).read().strip())  # the fake's connection counter
        pids = fx.lsu_pids()
        for p in pids:
            os.kill(p, signal.SIGTERM)
        check("C1 the dashboard exits on SIGTERM", fx.wait(lambda: all(not fx.alive(p) for p in pids), 8))
        check("C2 the tunnel ssh is gone", fx.wait(lambda: all(not fx.alive(p) for p in live), 8), str([p for p in live if fx.alive(p)]))
        check("C3 the last connection was deleted", fx.wait(lambda: f"--instance-console-connection-id={CONN}{last} --force" in fx.log("deleted.log"), 10), fx.log("deleted.log").strip()[-100:])
        check("C4 no socket directory is left behind", fx.wait(lambda: not fx.socket_dirs(), 5), str(fx.socket_dirs()))
    finally:
        fx.stop()
        if os.environ.get("KEEP"):
            print("kept", fx.dir)
        else:
            fx.cleanup()


def static_frame():
    """Hold the RFB pixels constant while the tmux pane is redrawn/resized."""
    fx = Fx(static=True)
    fx.start()
    try:
        check("S0 static framebuffer reaches the tmux Kitty pane", fx.wait(lambda: title(fx) == "vnc") and fx.wait(lambda: len(last_images(fx)) >= 1, 20))
        view_model(fx, "S1 static frame is placed")
        count_before = len(last_images(fx))
        time.sleep(1.5)
        check("S2 fake server keeps the framebuffer static", len(last_images(fx)) == count_before, f"transmits={len(last_images(fx))} initial={count_before}")
        fx.tmux("resize-window", "-t", "lsu:0", "-x", str(fx.cols - 4), "-y", str(fx.rows - 2))
        check("S3 retained static frame redraws after terminal resize", fx.wait(lambda: len(last_images(fx)) > count_before, 8), f"transmits={len(last_images(fx))} initial={count_before}")
        view_model(fx, "S4 resized pane still shows a placed image")
    finally:
        fx.stop()
        if os.environ.get("KEEP"):
            print("kept", fx.dir)
        else:
            fx.cleanup()


def pty_hangup():
    """A real terminal hangup must stop the TUI promptly and reap it."""
    root = tempfile.mkdtemp(prefix="ocpty")
    hosts = os.path.join(root, "hosts")
    os.mkdir(hosts, 0o700)
    err_r, err_w = os.pipe()
    fd, slave = pty.openpty()
    env = dict(os.environ, TERM="xterm-kitty")
    if TERMINFO:
        env["TERMINFO"] = TERMINFO
    child = subprocess.Popen(
        [BIN, "tui", "--no-sudo", "-k", "/dev/null", "--refresh", "3600", hosts],
        stdin=slave,
        stdout=slave,
        stderr=err_w,
        env=env,
        start_new_session=True,
        close_fds=True,
    )
    pid = child.pid
    os.close(slave)
    os.close(err_w)

    reaped = False
    started = False
    exited = False
    status = None
    output = bytearray()
    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        deadline = time.time() + 5
        while time.time() < deadline and not started:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if ready:
                try:
                    output.extend(os.read(fd, 65536))
                except OSError:
                    break
                started = bool(output)
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                reaped = True
                break
        if reaped:
            check("P0 TUI starts under a real PTY", False, f"exit status={status}")
            return
        check("P0 TUI starts under a real PTY", started, f"terminal bytes={len(output)}")
        # Keep the master open long enough for the Crossterm reader to enter
        # its poll path; closing it in that window used to trap the UI in EOF.
        time.sleep(0.2)
        os.close(fd)
        fd = -1
        deadline = time.time() + 5
        while time.time() < deadline:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                reaped = exited = True
                break
            time.sleep(0.05)
        code = os.waitstatus_to_exitcode(status) if status is not None else None
        err = b""
        ready, _, _ = select.select([err_r], [], [], 0.2)
        if ready:
            err = os.read(err_r, 65536)
        check("P1 closing the PTY master ends the TUI cleanly", exited and code == 0 and not err, f"wait status={status} exit code={code} stderr={err.decode(errors='replace')[-500:]}")
    finally:
        os.close(err_r)
        if fd >= 0:
            os.close(fd)
        if not reaped:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        shutil.rmtree(root, ignore_errors=True)


def killed():
    fx = Fx()
    fx.start()
    try:
        check("K0 the picture arrives", fx.wait(lambda: len(last_images(fx)) >= 2, 20))
        tun = fx.tunnels()
        pids = fx.lsu_pids()
        for p in pids:
            os.kill(p, signal.SIGKILL)
        check("K1 the connection is still deleted by the helper after SIGKILL", fx.wait(lambda: f"--instance-console-connection-id={CONN}1 --force" in fx.log("deleted.log"), 20), fx.log("deleted.log").strip()[-100:])
        check("K2 the tunnel ssh dies with the dashboard", fx.wait(lambda: all(not fx.alive(p) for p in tun), 10), str([p for p in tun if fx.alive(p)]))
        print("info: socket directories left after SIGKILL:", fx.socket_dirs())
    finally:
        fx.stop()
        if os.environ.get("KEEP"):
            print("kept", fx.dir)
        else:
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
        if mode == "all":
            scenario()
            static_frame()
            pty_hangup()
            killed()
        elif mode == "kill":
            killed()
        elif mode == "static":
            static_frame()
        elif mode == "pty":
            pty_hangup()
        else:
            print("usage: python3 run.py [all|kill|static|pty|explore]", file=sys.stderr)
            sys.exit(2)
        bad = [r for r in RESULTS if not r[1]]
        print(f"\n{len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed")
        sys.exit(1 if bad else 0)
