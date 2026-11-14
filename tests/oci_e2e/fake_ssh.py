"""Fake `ssh` for the OCI fixture: stdlib only, no network.

The dashboard starts `ssh -o ProxyCommand=... -N -L SOCKET:INSTANCE:5900
INSTANCE` for the console tunnel. This script is that process: it listens on
the Unix socket SOCKET (like `ssh -L` does) and speaks RFB to whoever connects
(a small fake desktop: 320x200 gradient, a moving bar, a bottom stripe whose
colour changes on every key press). It never contacts anything.

Any other `ssh` call is handed to the real `ssh` (LSU_FX_REAL_SSH), so the
rest of the dashboard behaves as usual.

Environment (set by the fixture's wrapper script):
  LSU_FX_DIR       the fixture directory (logs, switch files)
  LSU_FX_REAL_SSH  the real ssh, for calls that are not the tunnel

Switch files in LSU_FX_DIR:
  busy             a client is refused the way OCI refuses a second one:
                   "administratively prohibited" on stderr, stream closed
  held             the console slot is taken while one client is connected,
                   a second client is refused as with `busy`
  static           hold the initial framebuffer instead of animating it

The log never contains a password or key.
"""
import asyncio, os, signal, struct, sys, time

DIR = os.environ.get("LSU_FX_DIR", ".")
W, H = 320, 200


def log(*a):
    with open(os.path.join(DIR, "ssh.log"), "a") as f:
        f.write(f"{time.time():.2f} pid={os.getpid()} " + " ".join(str(x) for x in a) + "\n")


class Pix:
    def __init__(self):
        self.bpp, self.be = 32, 0
        self.shift = (16, 8, 0)
        self.max = (255, 255, 255)

    def pack(self, r, g, b):
        v = 0
        for c, s, m in zip((r, g, b), self.shift, self.max):
            v |= (c * m // 255) << s
        n = self.bpp // 8
        return v.to_bytes(n, "big" if self.be else "little")


def frame_pixels(pix, tick, stripe):
    rows = []
    for y in range(H):
        row = bytearray()
        for x in range(W):
            if y < 16:
                # a bar that moves with the tick: the picture changes over time
                on = (x // 16) == (tick % (W // 16))
                px = pix.pack(255, 255, 255) if on else pix.pack(0, 0, 0)
            elif y >= H - 24:
                px = pix.pack(*stripe)
            else:
                px = pix.pack(x * 255 // W, y * 255 // H, 90)
            row += px
        rows.append(bytes(row))
    return b"".join(rows)


KEYS = {0xFF0D: "Return", 0xFF08: "BackSpace", 0xFF1B: "Escape", 0xFF09: "Tab"}


async def rfb(r, w):
    async def read(n):
        return await r.readexactly(n)

    w.write(b"RFB 003.008\n")
    ver = await read(12)
    log("vnc client version", ver.decode().strip())
    w.write(bytes([1, 1]))  # one security type: None
    await read(1)
    w.write(struct.pack(">I", 0))
    await read(1)  # ClientInit
    pix = Pix()
    name = b"fake-oci-instance"
    w.write(struct.pack(">HH", W, H) + bytes([32, 24, 0, 1]) + struct.pack(">HHH", 255, 255, 255) + bytes([16, 8, 0, 0, 0, 0]) + struct.pack(">I", len(name)) + name)
    log("vnc serverinit sent")
    state = {"tick": 0, "stripe": (200, 40, 40), "dirty": True, "pending": False, "stop": False}

    async def ticker():
        while not state["stop"]:
            await asyncio.sleep(0.5)
            if not os.path.exists(os.path.join(DIR, "static")):
                state["tick"] += 1
                state["dirty"] = True
                await push()

    async def push():
        if state["pending"] and state["dirty"]:
            state["pending"] = state["dirty"] = False
            data = frame_pixels(pix, state["tick"], state["stripe"])
            w.write(struct.pack(">BxH", 0, 1) + struct.pack(">HHHHi", 0, 0, W, H, 0) + data)
            await w.drain()

    t = asyncio.ensure_future(ticker())
    try:
        while True:
            m = (await read(1))[0]
            if m == 0:
                d = await read(19)
                bpp, _depth, be, tc = d[3], d[4], d[5], d[6]
                mx = struct.unpack(">HHH", d[7:13])
                pix.bpp, pix.be, pix.max = bpp, be, mx
                pix.shift = (d[13], d[14], d[15])
                log(f"vnc setpixelformat bpp={bpp} be={be} tc={tc}")
            elif m == 2:
                d = await read(3)
                n = struct.unpack(">H", d[1:3])[0]
                await read(4 * n)
                log(f"vnc setencodings n={n}")
            elif m == 3:
                await read(9)
                state["pending"] = True
                await push()
            elif m == 4:
                d = await read(7)
                down, key = d[0], struct.unpack(">I", d[3:7])[0]
                label = KEYS.get(key) or (chr(key) if 32 <= key < 127 else hex(key))
                log(f"vnc key down={down} {label}")
                if down:
                    state["stripe"] = ((state["stripe"][0] + 60) % 256, (state["stripe"][1] + 90) % 256, (state["stripe"][2] + 30) % 256)
                    state["dirty"] = True
                    await push()
            elif m == 5:
                d = await read(5)
                buttons, x, y = struct.unpack(">BHH", d)
                log(f"vnc pointer buttons={buttons} x={x} y={y}")
            elif m == 6:
                d = await read(7)
                await read(struct.unpack(">I", d[3:7])[0])
            else:
                log("vnc unknown client message", m)
                return
    finally:
        state["stop"] = True
        t.cancel()


CLIENTS = 0


async def handle(r, w):
    global CLIENTS
    if os.path.exists(os.path.join(DIR, "busy")) or (os.path.exists(os.path.join(DIR, "held")) and CLIENTS):
        log("client refused (console in use)")
        sys.stderr.write("channel 2: open failed: administratively prohibited: open failed\n")
        sys.stderr.flush()
        w.close()
        return
    CLIENTS += 1
    log("client connected")
    try:
        await rfb(r, w)
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    except Exception as e:  # noqa
        log("error", type(e).__name__, e)
    finally:
        CLIENTS -= 1
        log("client disconnected")
        w.close()


async def tunnel(socket_path):
    srv = await asyncio.start_unix_server(handle, path=socket_path)
    os.chmod(socket_path, 0o600)
    log("listening on a unix socket")
    async with srv:
        await srv.serve_forever()


def main(argv):
    spec = next((a for a in argv if a.startswith("/") and a.count(":") == 2 and a.endswith(":5900")), None)
    if spec is None:
        real = os.environ.get("LSU_FX_REAL_SSH")
        if not real:
            sys.stderr.write("fake ssh: not a tunnel call and no real ssh\n")
            sys.exit(255)
        os.execv(real, [real] + argv)
    log("tunnel call: forward to a unix socket,", "-N" in argv and "no command" or "command")
    signal.signal(signal.SIGTERM, lambda *_: (log("SIGTERM"), os._exit(0)))
    asyncio.run(tunnel(spec.split(":", 1)[0]))


if __name__ == "__main__":
    main(sys.argv[1:])
