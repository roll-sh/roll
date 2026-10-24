# Minimal raw CQL client (v3/v4 framing): exact error codes, messages, extra fields, warnings.
import socket, struct
def rs(b, o):
    n = struct.unpack_from('>H', b, o)[0]; return b[o+2:o+2+n].decode('utf8', 'replace'), o+2+n
def ls(x):
    x = x.encode() if isinstance(x, str) else x; return struct.pack('>I', len(x)) + x
def smap(d):
    b = struct.pack('>H', len(d))
    for k, v in d.items():
        for x in (k, v): x = x.encode(); b += struct.pack('>H', len(x)) + x
    return b
CL = {'ANY': 0, 'ONE': 1, 'TWO': 2, 'THREE': 3, 'QUORUM': 4, 'ALL': 5, 'LOCAL_QUORUM': 6, 'EACH_QUORUM': 7, 'SERIAL': 8, 'LOCAL_SERIAL': 9, 'LOCAL_ONE': 10}
def decode_error(body, o=0):
    code = struct.unpack_from('>i', body, o)[0]; msg, o2 = rs(body, o + 4)
    e = {'code': '0x%04X' % code, 'message': msg}
    rest = body[o2:]
    try:
        if code == 0x2400:
            ks, o3 = rs(body, o2); t, _ = rs(body, o3); e['extra'] = {'keyspace': ks, 'table': t}
        elif code == 0x2500:
            n = struct.unpack_from('>H', body, o2)[0]; e['extra'] = {'id': body[o2+2:o2+2+n].hex()}
        elif code == 0x1000:
            cl, n1, n2 = struct.unpack_from('>Hii', body, o2); e['extra'] = {'consistency': cl, 'required': n1, 'alive': n2}
        elif code in (0x1100, 0x1200):
            cl, rec, bf = struct.unpack_from('>Hii', body, o2); e['extra'] = {'consistency': cl, 'received': rec, 'blockfor': bf, 'rest_hex': body[o2+10:].hex()}
        elif rest:
            e['extra_hex'] = rest.hex()
    except Exception:
        e['extra_hex'] = rest.hex()
    return e
class Raw:
    def __init__(self, host='127.0.0.1', port=9042, user=None, pw=None, version=4, options=None):
        self.v = version; self.sock = socket.create_connection((host, port), timeout=120); self.stream = 0
        op, flags, body = self.req(1, smap(options or {'CQL_VERSION': '3.0.0'}))
        self.startup = (op, body)
        if op == 3:
            tok = b'\x00' + (user or '').encode() + b'\x00' + (pw or '').encode()
            op, flags, body = self.req(0x0f, struct.pack('>i', len(tok)) + tok)
            self.auth = (op, decode_error(body) if op == 0 else None)
    def recv(self, n):
        b = b''
        while len(b) < n:
            c = self.sock.recv(n - len(b))
            if not c: raise ConnectionError('closed')
            b += c
        return b
    def req(self, op, body, flags=0):
        self.stream = (self.stream + 1) % 30000
        self.sock.sendall(struct.pack('>BBhBI', self.v, flags, self.stream, op, len(body)) + body)
        while True:
            hdr = self.recv(9); ver, flags, stream, rop, ln = struct.unpack('>BBhBI', hdr)
            body = self.recv(ln)
            if stream == self.stream or rop == 0 and stream in (0, -1): return rop, flags, body
    def parse(self, op, flags, body):
        o = 0; out = {}
        if flags & 0x02: out['tracing_id'] = body[o:o+16].hex(); o += 16
        if flags & 0x08:
            n = struct.unpack_from('>H', body, o)[0]; o += 2; w = []
            for _ in range(n): x, o = rs(body, o); w.append(x)
            out['warnings'] = w
        if flags & 0x04:
            n = struct.unpack_from('>H', body, o)[0]; o += 2; cp = {}
            for _ in range(n):
                k, o = rs(body, o); l = struct.unpack_from('>i', body, o)[0]; cp[k] = body[o+4:o+4+max(l, 0)].hex(); o += 4 + max(l, 0)
            out['custom_payload'] = cp
        if op == 0:
            out.update(ok=False, error=decode_error(body, o))
        elif op == 8:
            kind = struct.unpack_from('>i', body, o)[0]
            out.update(ok=True, result_kind={1: 'Void', 2: 'Rows', 3: 'SetKeyspace', 4: 'Prepared', 5: 'SchemaChange'}.get(kind, kind))
            if kind == 5:
                o += 4; ct, o = rs(body, o); tg, o = rs(body, o); ks, o = rs(body, o); sc = [ct, tg, ks]
                if tg != 'KEYSPACE':
                    nm, o = rs(body, o); sc.append(nm)
                    if tg in ('FUNCTION', 'AGGREGATE'):
                        n = struct.unpack_from('>H', body, o)[0]; o += 2; args = []
                        for _ in range(n): a, o = rs(body, o); args.append(a)
                        sc.append(args)
                out['schema_change'] = sc
            elif kind == 3:
                out['keyspace'] = rs(body, o + 4)[0]
            elif kind == 2:
                mflags, ncols = struct.unpack_from('>ii', body, o + 4); out['metadata_flags'] = mflags; out['column_count'] = ncols
                out['has_more_pages'] = bool(mflags & 0x02)
        else:
            out.update(ok=True, opcode=op)
        return out
    def query(self, q, page_size=5000, consistency='ONE', serial=None, timestamp=None, tracing=False):
        fl = 0
        tail = b''
        if page_size: fl |= 0x04; tail += struct.pack('>i', page_size)
        if serial: fl |= 0x10; tail += struct.pack('>H', CL[serial])
        if timestamp is not None: fl |= 0x20; tail += struct.pack('>q', timestamp)
        b = ls(q) + struct.pack('>HB', CL.get(consistency, consistency) if isinstance(consistency, str) else consistency, fl) + tail
        return self.parse(*self.req(7, b, 0x02 if tracing else 0))
    def prepare(self, q):
        op, flags, body = self.req(9, ls(q))
        return self.parse(op, flags, body), body
