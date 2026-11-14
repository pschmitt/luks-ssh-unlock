"""Fake OpenStack console proxy for the LSU fixture: stdlib only, loopback only.

One WebSocket server speaks both console flavours that `openstack console url
show` points at:

  /websockify?token=T   a noVNC proxy; the RFB server is built in
  /?token=T             a serial console proxy (echoes lines with a prefix)

Tokens come from the fake `openstack` CLI (one file per token in TOKDIR) and
are single use, like Nova's: a second connection with the same token is
refused. The log never contains a token.
"""
import asyncio, base64, hashlib, os, struct, sys, time, urllib.parse

PORT = int(sys.argv[1])
TOKDIR = sys.argv[2]
LOG = open(sys.argv[3], "a", buffering=1)
STATIC = len(sys.argv) > 4 and sys.argv[4] == "static"
W = int(sys.argv[5]) if len(sys.argv) > 5 else 320
H = int(sys.argv[6]) if len(sys.argv) > 6 else 200
GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def log(*a):
    LOG.write(f"{time.time():.2f} " + " ".join(str(x) for x in a) + "\n")


def take_token(tok, kind):
    path = os.path.join(TOKDIR, os.path.basename(tok))
    try:
        got = open(path).read().strip()
        os.remove(path)
    except OSError:
        return False
    return got == kind


class WS:
    def __init__(self, r, w):
        self.r, self.w, self.buf = r, w, b""

    def send(self, data, op=2):
        n = len(data)
        head = bytes([0x80 | op])
        if n < 126:
            head += bytes([n])
        elif n < 65536:
            head += bytes([126]) + struct.pack(">H", n)
        else:
            head += bytes([127]) + struct.pack(">Q", n)
        self.w.write(head + data)

    def send_stream(self, data, chunk_size=64 * 1024):
        for start in range(0, len(data), chunk_size):
            self.send(data[start:start + chunk_size])

    async def frame(self):
        h = await self.r.readexactly(2)
        op, n = h[0] & 0x0F, h[1] & 0x7F
        masked = h[1] & 0x80
        if n == 126:
            n = struct.unpack(">H", await self.r.readexactly(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", await self.r.readexactly(8))[0]
        mask = await self.r.readexactly(4) if masked else b"\0\0\0\0"
        data = bytearray(await self.r.readexactly(n))
        for i in range(n):
            data[i] ^= mask[i % 4]
        return op, bytes(data)

    async def read(self, n):
        while len(self.buf) < n:
            op, data = await self.frame()
            if op == 8:
                raise asyncio.IncompleteReadError(b"", n)
            if op == 9:
                self.send(data, 10)
            elif op in (1, 2):
                if op == 1:
                    log("TEXT frame from the client (the proxy only takes binary)")
                self.buf += data
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    async def any(self):
        """The next data chunk, whatever its size."""
        while True:
            op, data = await self.frame()
            if op == 8:
                raise asyncio.IncompleteReadError(b"", 1)
            if op == 9:
                self.send(data, 10)
            elif op in (1, 2):
                return data


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


async def rfb(ws):
    ws.send(b"RFB 003.008\n")
    ver = await ws.read(12)
    log("vnc client version", ver.decode().strip())
    ws.send(bytes([1, 1]))  # one security type: None
    await ws.read(1)
    ws.send(struct.pack(">I", 0))
    await ws.read(1)  # ClientInit
    pix = Pix()
    name = b"fake-openstack-server"
    ws.send(struct.pack(">HH", W, H) + bytes([32, 24, 0, 1]) + struct.pack(">HHH", 255, 255, 255) + bytes([16, 8, 0, 0, 0, 0]) + struct.pack(">I", len(name)) + name)
    log("vnc serverinit sent")
    state = {"tick": 0, "stripe": (200, 40, 40), "dirty": True, "pending": False, "stop": False}

    async def ticker():
        while not state["stop"]:
            await asyncio.sleep(0.5)
            if not STATIC:
                state["tick"] += 1
                state["dirty"] = True
                await push()

    async def push():
        if state["pending"] and state["dirty"]:
            state["pending"] = state["dirty"] = False
            data = frame_pixels(pix, state["tick"], state["stripe"])
            log("vnc framebuffer update", W, H, len(data))
            ws.send_stream(struct.pack(">BxH", 0, 1) + struct.pack(">HHHHi", 0, 0, W, H, 0) + data)
            await ws.w.drain()

    t = asyncio.ensure_future(ticker())
    try:
        while True:
            m = (await ws.read(1))[0]
            if m == 0:
                d = await ws.read(19)
                bpp, _depth, be, tc = d[3], d[4], d[5], d[6]
                mx = struct.unpack(">HHH", d[7:13])
                pix.bpp, pix.be, pix.max = bpp, be, mx
                pix.shift = (d[13], d[14], d[15])
                log(f"vnc setpixelformat bpp={bpp} be={be} tc={tc}")
            elif m == 2:
                d = await ws.read(3)
                n = struct.unpack(">H", d[1:3])[0]
                await ws.read(4 * n)
                log(f"vnc setencodings n={n}")
            elif m == 3:
                await ws.read(9)
                state["pending"] = True
                await push()
            elif m == 4:
                d = await ws.read(7)
                down, key = d[0], struct.unpack(">I", d[3:7])[0]
                label = KEYS.get(key) or (chr(key) if 32 <= key < 127 else hex(key))
                log(f"vnc key down={down} {label}")
                if down and not STATIC:
                    state["stripe"] = ((state["stripe"][0] + 60) % 256, (state["stripe"][1] + 90) % 256, (state["stripe"][2] + 30) % 256)
                    state["dirty"] = True
                    await push()
            elif m == 5:
                await ws.read(5)
            elif m == 6:
                d = await ws.read(7)
                await ws.read(struct.unpack(">I", d[3:7])[0])
            else:
                log("vnc unknown client message", m)
                return
    finally:
        state["stop"] = True
        t.cancel()


async def serial(ws):
    ws.send(b"fake-serial-console ready\r\n")
    line = b""
    while True:
        data = await ws.any()
        log("serial rx", repr(data))
        for b in data:
            if b in (13, 10):
                ws.send(b"\r\nserver-echo: " + line + b"\r\n")
                line = b""
            else:
                line += bytes([b])
        await ws.w.drain()


async def handle(r, w):
    try:
        head = await r.readuntil(b"\r\n\r\n")
        lines = head.decode("latin1").split("\r\n")
        target = lines[0].split(" ")[1]
        hdr = {}
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                hdr[k.strip().lower()] = v.strip()
        u = urllib.parse.urlparse(target)
        tok = urllib.parse.parse_qs(u.query).get("token", [""])[0]
        kind = {"/websockify": "vnc", "/": "serial"}.get(u.path)
        protos = [p.strip() for p in hdr.get("sec-websocket-protocol", "").split(",")]
        if kind is None or "sec-websocket-key" not in hdr or not take_token(tok, kind):
            log("reject", u.path)
            w.write(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            await w.drain()
            return
        if "binary" not in protos:
            log("reject: no binary subprotocol", protos)
            w.write(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            await w.drain()
            return
        acc = base64.b64encode(hashlib.sha1(hdr["sec-websocket-key"].encode() + GUID).digest()).decode()
        w.write(
            ("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
             f"Sec-WebSocket-Accept: {acc}\r\nSec-WebSocket-Protocol: binary\r\n\r\n").encode()
        )
        await w.drain()
        log(f"{kind} connect (token ok, subprotocol binary)")
        ws = WS(r, w)
        await (rfb(ws) if kind == "vnc" else serial(ws))
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    except Exception as e:  # noqa
        log("error", type(e).__name__, e)
    finally:
        log("disconnect")
        w.close()


async def main():
    srv = await asyncio.start_server(handle, "127.0.0.1", PORT)
    log("listening")
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
