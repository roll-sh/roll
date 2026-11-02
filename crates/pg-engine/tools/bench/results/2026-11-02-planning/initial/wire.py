"""wire.py: send raw protocol messages to two servers and print what each answers, message by
message, to find wire-level differences between the engine and PostgreSQL.

    python3 -I wire.py <script.py> [engine port] [pg port]

The script defines `run(c)`, where `c` is a Conn: c.query(sql), c.parse(name, sql, oids),
c.bind(portal, stmt, params, formats, result_formats), c.describe('S'|'P', name),
c.execute(portal, max_rows), c.close('S'|'P', name), c.sync(), c.flush(), c.raw(type, body),
c.read(n) (read n messages, or until ReadyForQuery when n is None). Received messages are
printed, decoded; the two servers' transcripts are diffed.
"""
import base64
import difflib
import hashlib
import hmac
import os
import socket
import struct
import sys


class Conn:
    def __init__(self, port, user="postgres", password="pw", db="postgres", params=None):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.buf = b""
        self.log = []
        p = {"user": user, "database": db, **(params or {})}
        body = struct.pack("!i", 196608) + b"".join(
            k.encode() + b"\0" + v.encode() + b"\0" for k, v in p.items()
        ) + b"\0"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self.password = password
        self.user = user
        self._auth()

    def _auth(self):
        while True:
            t, b = self._msg()
            if t == b"R":
                kind = struct.unpack("!i", b[:4])[0]
                if kind == 0:
                    continue
                if kind == 3:
                    self.send(b"p", self.password.encode() + b"\0")
                elif kind == 5:
                    salt = b[4:8]
                    inner = hashlib.md5(self.password.encode() + self.user.encode()).hexdigest()
                    h = "md5" + hashlib.md5(inner.encode() + salt).hexdigest()
                    self.send(b"p", h.encode() + b"\0")
                elif kind == 10:
                    self.nonce = base64.b64encode(os.urandom(18)).decode()
                    self.first_bare = f"n=,r={self.nonce}"
                    msg = ("n,," + self.first_bare).encode()
                    self.send(b"p", b"SCRAM-SHA-256\0" + struct.pack("!i", len(msg)) + msg)
                elif kind == 11:
                    sf = b[4:].decode()
                    attrs = dict(x.split("=", 1) for x in sf.split(","))
                    salt = base64.b64decode(attrs["s"])
                    it = int(attrs["i"])
                    salted = hashlib.pbkdf2_hmac("sha256", self.password.encode(), salt, it)
                    ck = hmac.new(salted, b"Client Key", "sha256").digest()
                    sk = hashlib.sha256(ck).digest()
                    final_bare = f"c=biws,r={attrs['r']}"
                    auth = f"{self.first_bare},{sf},{final_bare}".encode()
                    sig = hmac.new(sk, auth, "sha256").digest()
                    proof = bytes(a ^ b for a, b in zip(ck, sig))
                    self.send(b"p", (final_bare + ",p=" + base64.b64encode(proof).decode()).encode())
                elif kind == 12:
                    continue
            elif t == b"E":
                raise RuntimeError(decode(t, b))
            elif t == b"K":
                self.pid, self.secret = struct.unpack("!ii", b[:8])
            elif t == b"Z":
                return

    def _recv(self, n):
        while len(self.buf) < n:
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.buf += d
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _msg(self):
        h = self._recv(5)
        t, n = h[:1], struct.unpack("!i", h[1:])[0]
        return t, self._recv(n - 4)

    def send(self, t, body):
        self.s.sendall(t + struct.pack("!i", len(body) + 4) + body)

    def raw(self, t, body):
        self.send(t, body)

    def query(self, sql):
        self.send(b"Q", sql.encode() + b"\0")

    def parse(self, name, sql, oids=()):
        self.send(b"P", name.encode() + b"\0" + sql.encode() + b"\0"
                  + struct.pack("!h", len(oids)) + b"".join(struct.pack("!I", o) for o in oids))

    def bind(self, portal, stmt, params=(), formats=(), result_formats=()):
        b = portal.encode() + b"\0" + stmt.encode() + b"\0"
        b += struct.pack("!h", len(formats)) + b"".join(struct.pack("!h", f) for f in formats)
        b += struct.pack("!h", len(params))
        for p in params:
            if p is None:
                b += struct.pack("!i", -1)
            else:
                p = p.encode() if isinstance(p, str) else p
                b += struct.pack("!i", len(p)) + p
        b += struct.pack("!h", len(result_formats)) + b"".join(
            struct.pack("!h", f) for f in result_formats)
        self.send(b"B", b)

    def describe(self, kind, name):
        self.send(b"D", kind.encode() + name.encode() + b"\0")

    def execute(self, portal, max_rows=0):
        self.send(b"E", portal.encode() + b"\0" + struct.pack("!i", max_rows))

    def close(self, kind, name):
        self.send(b"C", kind.encode() + name.encode() + b"\0")

    def sync(self):
        self.send(b"S", b"")

    def flush(self):
        self.send(b"H", b"")

    def read(self, n=None, timeout=5):
        self.s.settimeout(timeout)
        got = 0
        try:
            while n is None or got < n:
                t, b = self._msg()
                got += 1
                self.log.append(decode(t, b))
                if n is None and t == b"Z":
                    break
        except (socket.timeout, EOFError) as e:
            self.log.append(f"<{type(e).__name__}>")


def decode(t, b):
    t = t.decode()
    if t in "EN":
        fields = {}
        for f in b.split(b"\0"):
            if f:
                fields[chr(f[0])] = f[1:].decode(errors="replace")
        keep = "SCMDHPW"
        return t + " " + " ".join(f"{k}={fields[k]!r}" for k in keep if k in fields)
    if t == "T":
        n = struct.unpack("!h", b[:2])[0]
        pos, cols = 2, []
        for _ in range(n):
            e = b.index(b"\0", pos)
            name = b[pos:e].decode()
            tab, att, oid, ln, mod, fmt = struct.unpack("!IhIhih", b[e + 1:e + 19])
            pos = e + 19
            cols.append(f"{name}:{oid}/{ln}/{mod}/f{fmt}" + (f"@{att}" if att else ""))
        return "T " + " ".join(cols)
    if t == "D":
        n = struct.unpack("!h", b[:2])[0]
        pos, vals = 2, []
        for _ in range(n):
            ln = struct.unpack("!i", b[pos:pos + 4])[0]
            pos += 4
            if ln < 0:
                vals.append("NULL")
            else:
                v = b[pos:pos + ln]
                pos += ln
                try:
                    s = v.decode()
                    vals.append(repr(s) if s.isprintable() else v.hex())
                except UnicodeDecodeError:
                    vals.append(v.hex())
        return "D " + " ".join(vals)
    if t == "t":
        n = struct.unpack("!h", b[:2])[0]
        return "t " + " ".join(str(x) for x in struct.unpack(f"!{n}I", b[2:2 + 4 * n]))
    if t in "CS":
        return t + " " + " ".join(x.decode() for x in b.split(b"\0") if x)
    if t == "Z":
        return "Z " + b.decode()
    if t in "dGHW":
        return t + " " + b.hex()
    if t == "A":
        pid = struct.unpack("!i", b[:4])[0]
        return "A " + " ".join(x.decode() for x in b[4:].split(b"\0") if x)
    return t + (" " + b.hex() if b else "")


def main():
    script = sys.argv[1]
    ports = [int(sys.argv[2]) if len(sys.argv) > 2 else 55839,
             int(sys.argv[3]) if len(sys.argv) > 3 else 55838]
    ns = {}
    exec(open(script).read(), ns)
    outs = []
    for port in ports:
        c = Conn(port, **ns.get("CONN", {}))
        try:
            ns["run"](c)
        except Exception as e:  # noqa: BLE001
            c.log.append(f"<python {type(e).__name__}: {e}>")
        outs.append(c.log)
    if outs[0] == outs[1]:
        print("same:")
        print("\n".join(outs[0]))
    else:
        print("\n".join(difflib.unified_diff(outs[1], outs[0], "pg", "engine", lineterm="", n=50)))


if __name__ == "__main__":
    main()
