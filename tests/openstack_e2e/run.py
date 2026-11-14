"""End-to-end fixture: dashboard -> fake `openstack` CLI -> loopback WebSocket
proxy -> built-in RFB / serial server. Everything is loopback and fake; no real
console is contacted. Python standard library only, plus tmux. See README.md.

usage: python3 run.py [all|explore]

environment:
  LSU_BIN          the luks-ssh-unlock binary (default: $CARGO_TARGET_DIR or
                   <repo>/target, then debug/luks-ssh-unlock)
  KITTY_TERMINFO   directory holding the xterm-kitty terminfo entry, only
                   needed when `infocmp xterm-kitty` does not find it
  LSU_FX_PORT      loopback port for the fake proxy (default: a free one)
  KEEP=1           keep the temporary directory (logs, fake CLI) for a look

exit status: 0 all checks passed, 1 a check failed, 77 prerequisites missing.
"""
import base64, zlib, os, pty, re, shutil, signal, socket, struct, subprocess, sys, tempfile, threading, time, unicodedata, fcntl, termios

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(REPO, "src")
BIN = os.environ.get("LSU_BIN") or os.path.join(
    os.environ.get("CARGO_TARGET_DIR") or os.path.join(REPO, "target"), "debug", "luks-ssh-unlock"
)
TERMINFO = os.environ.get("KITTY_TERMINFO")
TEST_TERM = os.environ.get("LSU_TEST_TERM", "xterm-kitty")
FORCE_KITTY = os.environ.get("LSU_TEST_FORCE_KITTY") == "1"
APC = re.compile(rb"\x1b_G([^;\x1b]*)(?:;([^\x1b]*))?\x1b\\")
SGR = re.compile(r"\x1b\[[0-9;:]*m")


def free_port():
    if os.environ.get("LSU_FX_PORT"):
        return int(os.environ["LSU_FX_PORT"])
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


PORT = free_port()


def prerequisites():
    """Return a reason the fixture cannot run here, or None."""
    if shutil.which("tmux") is None:
        return "tmux is not installed"
    if not (os.path.isfile(BIN) and os.access(BIN, os.X_OK)):
        return f"binary not found: {BIN} (build it, or set LSU_BIN)"
    env = dict(os.environ, **({"TERMINFO": TERMINFO} if TERMINFO else {}))
    if subprocess.run(["infocmp", TEST_TERM], env=env, capture_output=True).returncode:
        return f"no {TEST_TERM} terminfo (install it, or set KITTY_TERMINFO)"
    return None


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, ok, detail))
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


class Fx:
    def __init__(self, cols=120, rows=40, env=None, static=False, width=320, height=200):
        self.dir = tempfile.mkdtemp(prefix="osfx")
        self.cols, self.rows = cols, rows
        self.static = static
        self.width, self.height = width, height
        self.sock = self.dir + "/tmux.sock"
        self.out = bytearray()
        self.lock = threading.Lock()
        self.env = env or {}
        self.tok = self.dir + "/tokens"
        os.mkdir(self.tok)
        os.mkdir(self.dir + "/hosts")
        self.cli = self.dir + "/openstack"
        self.srv = None

    # ---- fixture files
    def write_files(self):
        h = self.dir + "/hosts"
        with open(h + "/t1.env", "w") as f:
            f.write(f"SSH_HOSTNAME=127.0.0.1\nSSH_PORT=1\nSSH_KEY={self.dir}/key\nLUKS_PASSPHRASE=fixture-not-a-secret\nSSH_INITRD_KNOWN_HOSTS=127.0.0.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureFixtureFixtureFixtureFixtureFixtureFix\n")
        open(self.dir + "/key", "w").write("fixture\n")
        os.chmod(self.dir + "/key", 0o600)
        import json
        common = {"openstackBin": self.cli, "cloud": "fixture-cloud", "cloudsFile": self.dir + "/clouds.yaml"}
        open(self.dir + "/clouds.yaml", "w").write("clouds: {}\n")
        json.dump({"type": "openstack", "parameters": dict(common, server="srv-t1")}, open(h + "/t1.vnc.json", "w"))
        json.dump({"type": "openstack", "parameters": dict(common, server="srv-t1")}, open(h + "/t1.console.json", "w"))
        script = f"""#!/bin/sh
# Fake `openstack` CLI: prints a console URL with a fresh single-use token.
d={self.dir}
echo "args: $* | clouds_file=${{OS_CLIENT_CONFIG_FILE:-none}}" >> $d/cli.log
[ -e $d/fail ] && {{ echo "Server srv-t1 not found (HTTP 404)" >&2; exit 1; }}
mode=
for a in "$@"; do
  case "$a" in --novnc) mode=vnc;; --serial) mode=serial;; esac
done
tok=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \\n')
echo $mode > $d/tokens/$tok
echo $tok >> $d/issued.txt
case $mode in
  vnc) echo "http://127.0.0.1:{PORT}/vnc_lite.html?path=websockify%3Ftoken%3D$tok" ;;
  serial) echo "ws://127.0.0.1:{PORT}/?token=$tok" ;;
  *) echo "usage" >&2; exit 2 ;;
esac
"""
        open(self.cli, "w").write(script)
        os.chmod(self.cli, 0o755)

    def tmux(self, *a, check=True):
        r = subprocess.run(["tmux", "-S", self.sock, "-f", "/dev/null", *a], capture_output=True, text=True)
        if check and r.returncode:
            raise RuntimeError(f"tmux {a}: {r.stderr}")
        return r.stdout

    def start(self):
        self.write_files()
        self.srv = subprocess.Popen([sys.executable, HERE + "/server.py", str(PORT), self.tok, self.dir + "/server.log", "static" if self.static else "animated", str(self.width), str(self.height)])
        time.sleep(0.6)
        cmd = f"{BIN} tui --no-sudo -k /dev/null --refresh 3600 {self.dir}/hosts 2>{self.dir}/lsu.err"
        session_env = ["-e", "TERM=tmux-256color"]
        if FORCE_KITTY:
            session_env.extend(["-e", "LUKS_SSH_UNLOCK_GRAPHICS=kitty"])
        self.tmux("new-session", "-d", "-x", str(self.cols), "-y", str(self.rows), "-s", "lsu", *session_env, cmd)
        self.tmux("set", "-g", "allow-passthrough", "on")
        self.tmux("set", "-g", "escape-time", "0")
        self.tmux("set", "-ga", "terminal-features", f",{TEST_TERM}:RGB")
        self.tmux("set", "-g", "remain-on-exit", "on")
        pid, fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = TEST_TERM
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

    def stop(self):
        self.tmux("kill-server", check=False)
        if self.srv:
            self.srv.kill()
        try:
            os.close(self.fd)
        except OSError:
            pass

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)


def title(fx):
    for ln in fx.capture().split("\n"):
        m = re.search(r"(vnc|console) · t1", ln)
        if m:
            return m.group(1)
    return None


def show(fx, label):
    for _ in range(4):
        if title(fx) == label:
            return True
        fx.keys("c", literal=True)
        time.sleep(1.0)
    return title(fx) == label


def explore():
    fx = Fx()
    fx.start()
    try:
        time.sleep(3)
        fx.keys("i", literal=True)
        time.sleep(1)
        print(fx.capture())
        print("title:", title(fx))
        print(fx.log("cli.log"), fx.log("server.log"))
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


def replay(token, path):
    """A raw WebSocket handshake with an already used token."""
    s = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    req = f"GET {path}{token} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n"
    s.sendall(req.encode())
    data = s.recv(200)
    s.close()
    return data.split(b"\r\n")[0].decode()


def scenario():
    fx = Fx()
    fx.start()
    try:
        # --- serial (the first view: console.json is listed before vnc.json)
        check("S0 dashboard shows the OpenStack serial console view", fx.wait(lambda: title(fx) == "console"), f"title={title(fx)}")
        check("S1 serial: connected and banner from the fake proxy", fx.wait(lambda: "fake-serial-console ready" in fx.capture()))
        cli = fx.log("cli.log")
        check("S2 serial CLI call: --os-cloud, console url show --serial, server, clouds file", "--os-cloud fixture-cloud console url show --serial -f value -c url srv-t1" in cli and "clouds_file=" + fx.dir + "/clouds.yaml" in cli, cli.strip().splitlines()[0] if cli else "no call")
        check("S3 serial: the proxy accepted the single-use token and the binary subprotocol", "serial connect (token ok, subprotocol binary)" in fx.log("server.log"))
        fx.keys("S-Down")
        time.sleep(0.8)
        fx.keys("hello", literal=True)
        fx.keys("Enter")
        check("S4 serial round trip: typed line comes back from the server", fx.wait(lambda: "server-echo: hello" in fx.capture()), "pane shows server-echo: hello")
        sl = fx.log("server.log")
        check("S5 serial: the server received the keystrokes as binary frames", "serial rx" in sl and "TEXT frame" not in sl)
        fx.keys("C-r")
        check("S6 Ctrl+R restarts the serial console with a fresh token", fx.wait(lambda: count(fx.log("server.log"), "serial connect") == 2 and count(fx.log("cli.log"), "--serial") == 2), f"cli calls={count(fx.log('cli.log'), '--serial')} proxy connects={count(fx.log('server.log'), 'serial connect')}")
        time.sleep(1.0)
        # The console keeps the keyboard across the restart.
        fx.keys("again", literal=True)
        fx.keys("Enter")
        check("S7 serial still round-trips after the restart", fx.wait(lambda: "server-echo: again" in fx.capture()))

        # --- mode switch to the graphical view
        fx.keys("F6")
        check("M1 F6 switches to the graphical OpenStack view", fx.wait(lambda: title(fx) == "vnc"), f"title={title(fx)}")
        check("M2 the graphical view fetched a --novnc URL for the server", fx.wait(lambda: "console url show --novnc -f value -c url srv-t1" in fx.log("cli.log")))
        check("M3 the proxy accepted the noVNC token (subprotocol binary) and the RFB handshake ran", fx.wait(lambda: "vnc serverinit sent" in fx.log("server.log")) and "vnc connect (token ok, subprotocol binary)" in fx.log("server.log"))
        check("M4 picture arrives: Kitty transmits for the provider-fed RFB", fx.wait(lambda: len(last_images(fx)) >= 2, 12), f"{len(last_images(fx))} transmits")
        T = last_images(fx)
        if T:
            k = T[-1]["keys"]
            w, h, raw = rgba(T[-1])
            check("M5 payload is a complete RGBA image (Kitty fits it to the cells)", len(raw) == w * h * 4 and w > 0 and h > 0, f"s={w} v={h}")
            mid = pixels(T[-1], [h // 2])[2]
            check("M6 decoded pixels come from the fake framebuffer (gradient with b~90)", any(80 <= p[2] <= 100 for p in mid), f"sample={mid[len(mid)//2]}")
        time.sleep(2.0)
        T2 = last_images(fx)
        top = lambda e: pixels(e, [2])[2]
        check("M7 the picture keeps updating (moving bar differs between transmits)", len(T2) >= 3 and any(top(a) != top(b) for a, b in zip(T2, T2[1:])), f"{len(T2)} transmits")
        view_model(fx, "M8 the picture is placed (direct placement of a live image)")
        # keyboard to the desktop
        stripe = lambda e: pixels(e, [rgba(e)[1] - 3])[2]
        check("M9a before the key the stripe is the initial colour", any(near(p, (200, 40, 40), 30) for p in stripe(last_images(fx)[-1])))
        fx.keys("a", literal=True)
        check("M9 keys reach the desktop (RFB key event logged)", fx.wait(lambda: "vnc key down=1 a" in fx.log("server.log")))
        check("M10 and the new frame shows the reaction (stripe colour changed)", fx.wait(lambda: any(near(p, (4, 130, 70), 30) for p in stripe(last_images(fx)[-1]))), "stripe now ~(4,130,70)")
        fx.keys("C-t")
        time.sleep(0.8)
        fx.keys("b", literal=True)
        time.sleep(1.0)
        check("M11 Ctrl+T makes the desktop view-only: no key event for 'b'", "vnc key down=1 b" not in fx.log("server.log"))
        fx.keys("C-t")
        time.sleep(0.5)
        # restart the graphical view
        fx.keys("C-r")
        check("M12 Ctrl+R fetches a fresh token and reconnects the desktop", fx.wait(lambda: count(fx.log("cli.log"), "--novnc") == 2 and count(fx.log("server.log"), "vnc connect") == 2, 15), f"cli={count(fx.log('cli.log'), '--novnc')} proxy={count(fx.log('server.log'), 'vnc connect')}")
        m = fx.mark()
        check("M13 the picture is back after the restart", fx.wait(lambda: len(last_images(fx, m)) >= 1, 12))
        view_model(fx, "M14 the picture is placed after the restart")

        # --- back to serial, and the CLI failure path
        fx.keys("F6")
        check("M15 F6 returns to the serial console", fx.wait(lambda: title(fx) == "console"))
        check("M16 the console works again after the mode switch", (fx.wait(lambda: count(fx.log("server.log"), "serial connect") >= 3, 10)))
        open(fx.dir + "/fail", "w").close()
        fx.keys("C-r")
        time.sleep(3.0)
        cap = fx.capture()
        check("F1 a failing CLI is reported in the pane, dashboard still alive", bool(re.search(r"not found|failed|could not|cannot|error", cap, re.I)) and "t1" in cap, [l for l in cap.split("\n") if re.search(r"not found|failed|could not|cannot|error", l, re.I)][:2].__repr__())
        os.remove(fx.dir + "/fail")

        # --- token and secret hygiene
        issued = [t for t in fx.log("issued.txt").split() if t]
        check("T1 several distinct single-use tokens were issued", len(set(issued)) == len(issued) and len(issued) >= 5, f"{len(issued)} tokens")
        vt = open(fx.dir + "/issued.txt").read().split()
        status = replay(vt[0], "/websockify?token=")
        check("T2 replaying a used token is refused by the proxy", "401" in status, status)
        status2 = replay(vt[1], "/?token=")
        check("T3 a token of the other kind or a used one is refused too", "401" in status2, status2)
        haystacks = {
            "pane scrollback": fx.capture(scrollback=True),
            "terminal stream": fx.since(0).decode("latin1"),
            "lsu stderr": fx.log("lsu.err"),
            "cli log": fx.log("cli.log"),
            "proxy log": fx.log("server.log"),
        }
        ps = subprocess.run(["ps", "-eo", "args"], capture_output=True, text=True).stdout
        haystacks["process arguments"] = ps
        leaks = [f"{name}" for name, text in haystacks.items() for t in issued if t in text]
        check("T4 no token appears in the pane, terminal stream, logs or any argv", not leaks, str(leaks))

        # --- cleanup on SIGTERM
        m = fx.mark()
        pids = fx.lsu_pids()
        for p in pids:
            os.kill(p, signal.SIGTERM)
        time.sleep(2.5)
        left = subprocess.run(["pgrep", "-f", f"console openstack.*{re.escape(fx.dir)}"], capture_output=True, text=True).stdout.split()
        dels = [e["keys"] for e in events(fx.since(m)) if e["t"] == "d"]
        check("C1 SIGTERM leaves no `console openstack` child", not left, str(left))
        check("C2 the dashboard exits", all(not os.path.exists(f"/proc/{p}") for p in pids))
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
    elif mode == "static":
        fx = Fx(static=True, width=1920, height=1080)
        fx.start()
        try:
            check("ST0 static OpenStack fixture reaches the VNC viewer", fx.wait(lambda: title(fx) == "console"))
            started = time.perf_counter()
            fx.keys("F6")
            received = fx.wait(lambda: len(last_images(fx)) >= 1, 12)
            first_frame_ms = (time.perf_counter() - started) * 1000
            server_log = fx.log("server.log")
            screen = fx.capture().lower()
            detail = "first frame {:.0f} ms; vnc_connect={}; serverinit={}; server_frame={}; waiting={}; viewer={}".format(
                first_frame_ms,
                "vnc connect (token ok" in server_log,
                "vnc serverinit sent" in server_log,
                "vnc framebuffer update" in server_log,
                "waiting for the first picture" in screen,
                title(fx),
            )
            check("ST1 static 1920x1080 OpenStack VNC image arrives", received, detail)
            images = last_images(fx)
            if images:
                image_bytes = sum(len(e["b64"]) for e in images)
                image = images[-1]["keys"]
                print("PROFILE source=1920x1080 output={}x{} kitty_payload={}B first_frame={:.0f}ms".format(image.get("s"), image.get("v"), image_bytes, first_frame_ms))
            check("ST2 one static framebuffer produces one image upload", fx.wait(lambda: len(last_images(fx)) == 1, 3))
            first = placeholders(fx.capture(esc=True))
            time.sleep(1.5)
            second = placeholders(fx.capture(esc=True))
            check("ST3 static placeholder cells remain stable", bool(first) and first == second)
            check("ST4 no repeated image upload while framebuffer is static", len(last_images(fx)) == 1)
            fx.keys("S-Down")
            key_started = time.perf_counter()
            fx.keys("a", literal=True)
            key_seen = fx.wait(lambda: "vnc key down=1 a" in fx.log("server.log"), 3)
            key_ms = (time.perf_counter() - key_started) * 1000
            check("ST5 keyboard reaches the loopback OpenStack proxy", key_seen, f"{key_ms:.0f} ms")
            view_model(fx, "ST6 static frame is placed")
        finally:
            fx.stop()
            if os.environ.get("KEEP"):
                print("kept", fx.dir)
            else:
                fx.cleanup()
        bad = [r for r in RESULTS if not r[1]]
        print(f"\n{len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed")
        sys.exit(1 if bad else 0)
    else:
        scenario()
        bad = [r for r in RESULTS if not r[1]]
        print(f"\n{len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed")
        sys.exit(1 if bad else 0)
