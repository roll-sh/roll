# Auth behaviour of the auth-enabled references (PasswordAuthenticator + CassandraAuthorizer,
# superuser cassandra/cassandra): `python3 -I auth_live.py <port> <out.json>` against
# nosql-ref-scylla-auth (19043) or nosql-ref-cassandra-auth (29043). A raw protocol v4 client:
# rows are rendered as text (text as is, booleans True/False, ints, sets/maps python-like, other
# types as 0x-hex). Cases are in auth_cases.py; crates/nosql-engine/scripts/gen_auth.py turns
# the two outputs into crates/nosql-engine/src/tests/captured_auth.rs.
import socket, struct
def ls(x): b = x.encode(); return struct.pack('>i', len(b)) + b
def rs(b, o):
    n = struct.unpack_from('>H', b, o)[0]; return b[o+2:o+2+n].decode(), o+2+n
TYPES = {1:'ascii',2:'bigint',3:'blob',4:'boolean',5:'counter',6:'decimal',7:'double',8:'float',9:'int',11:'timestamp',12:'uuid',13:'text',14:'varint',15:'timeuuid',16:'inet',17:'date',18:'time',19:'smallint',20:'tinyint',21:'duration'}
def rtype(b, o):
    t = struct.unpack_from('>H', b, o)[0]; o += 2
    if t == 0: s, o = rs(b, o); return 'custom(%s)' % s, o
    if t in (32, 34): e, o = rtype(b, o); return '%s<%s>' % ('list' if t == 32 else 'set', e), o
    if t == 33: k, o = rtype(b, o); v, o = rtype(b, o); return 'map<%s, %s>' % (k, v), o
    return TYPES.get(t, str(t)), o
def fmt(ty, v):
    if v is None: return 'null'
    if ty in ('text', 'ascii'): return v.decode('utf8', 'replace')
    if ty == 'boolean': return 'True' if v[0] else 'False'
    if ty == 'int': return str(struct.unpack('>i', v)[0])
    if ty.startswith('set<') or ty.startswith('list<'):
        inner = ty[ty.index('<')+1:-1]; n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            l = struct.unpack_from('>i', v, o)[0]; o += 4; items.append(fmt1(inner, v[o:o+l])); o += l
        return ('{%s}' if ty.startswith('set') else '[%s]') % ', '.join(items)
    if ty.startswith('map<'):
        k, val = ty[4:-1].split(', ', 1); n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            l = struct.unpack_from('>i', v, o)[0]; o += 4; a = fmt1(k, v[o:o+l]); o += l
            l = struct.unpack_from('>i', v, o)[0]; o += 4; b = fmt1(val, v[o:o+l]); o += l
            items.append('%s: %s' % (a, b))
        return '{%s}' % ', '.join(items)
    return '0x' + v.hex()
def fmt1(ty, v):
    s = fmt(ty, v)
    return "'%s'" % s if ty in ('text', 'ascii') else s
def decode_error(b):
    code = struct.unpack_from('>i', b, 0)[0]; m, _ = rs(b, 4)
    return {'code': '0x%04X' % code, 'message': m}
class C:
    def __init__(s, port, user=None, pw=None, token=None):
        s.s = socket.create_connection(('127.0.0.1', port), timeout=60); s.st = 0
        body = struct.pack('>H', 1) + struct.pack('>H', 11) + b'CQL_VERSION' + struct.pack('>H', 5) + b'3.0.0'
        op, fl, b = s.req(1, body)
        s.login = {'ok': True, 'opcode': op}
        if op == 3:
            s.authenticator, _ = rs(b, 0)
            if token is None: token = b'\0' + (user or '').encode() + b'\0' + (pw or '').encode()
            op, fl, b = s.req(0x0f, struct.pack('>i', len(token)) + token)
            s.login = decode_error(b) if op == 0 else {'ok': True, 'opcode': op}
    def recv(s, n):
        d = b''
        while len(d) < n:
            x = s.s.recv(n - len(d))
            if not x: raise EOFError('closed')
            d += x
        return d
    def req(s, op, body):
        s.st += 1; s.s.sendall(struct.pack('>BBhBi', 4, 0, s.st, op, len(body)) + body)
        h = s.recv(9); v, fl, st, op, ln = struct.unpack('>BBhBi', h); b = s.recv(ln)
        return op, fl, b
    def prep(s, text):
        op, fl, b = s.req(9, ls(text))
        if op == 0: return decode_error(b[4 + 2 + 0:] if False else b)
        return {'kind': 'Prepared'}
    def q(s, text):
        op, fl, b = s.req(7, ls(text) + struct.pack('>HB', 1, 0))
        o = 0; out = {}
        if fl & 8:
            n = struct.unpack_from('>H', b, o)[0]; o += 2; w = []
            for _ in range(n): x, o = rs(b, o); w.append(x)
            out['warnings'] = w
        b = b[o:]
        if op == 0: out.update(decode_error(b)); return out
        kind = struct.unpack_from('>i', b, 0)[0]
        out['kind'] = {1: 'Void', 2: 'Rows', 3: 'SetKeyspace', 4: 'Prepared', 5: 'SchemaChange'}[kind]
        if kind != 2: return out
        o = 4; mf, n = struct.unpack_from('>ii', b, o); o += 8
        if mf & 2: l = struct.unpack_from('>i', b, o)[0]; o += 4 + l
        if mf & 1: ks, o = rs(b, o); tb, o = rs(b, o); out['table'] = ks + '.' + tb
        cols = []
        for _ in range(n):
            if not mf & 1: k, o = rs(b, o); t, o = rs(b, o); out['table'] = k + '.' + t
            c, o = rs(b, o); ty, o = rtype(b, o); cols.append([c, ty])
        out['cols'] = cols
        rows = []
        nr = struct.unpack_from('>i', b, o)[0]; o += 4
        for _ in range(nr):
            r = []
            for ci in range(n):
                l = struct.unpack_from('>i', b, o)[0]; o += 4
                r.append(fmt(cols[ci][1], None if l < 0 else b[o:o+l])); o += max(l, 0)
            rows.append(r)
        out['rows'] = rows
        return out
def login(port, token):
    try:
        c = C(port, token=token); r = dict(c.login)
        if 'code' not in r:
            r = {'ok': True}
        c.s.close(); return r
    except Exception as e:
        return {'client': repr(e)}

# --- runner -------------------------------------------------------------------------------------
CREDS = {'su': ('cassandra', 'cassandra'), 'alice': ('auth_alice', 'alice'), 'bob': ('auth_bob', 'bob'),
         'dc': ('auth_dc', 'dc'), 'carol': ('auth_carol', 'carol'), 'viewer': ('auth_viewer', 'v')}

def run_battery(port, clean, cases):
    import time
    conns = {}
    def get(who):
        if who not in conns:
            u, p = CREDS[who]
            conns[who] = C(port, u, p)
        return conns[who]
    res = []
    for _, _, q in clean:
        get('su').q(q)
    for cid, who, q in cases:
        if q.startswith('!sleep'):
            res.append([cid, who, q, {'sleep': True}])
            time.sleep(float(q[6:]))
            continue
        try:
            if q.startswith('!login') or q.startswith('!token'):
                if q.startswith('!login'):
                    _, u, p = q.split(' ')
                    token = b'\0' + u.encode() + b'\0' + p.encode()
                else:
                    token = bytes.fromhex(q[7:])
                r = login(port, token)
            elif q.startswith('!prepare'):
                r = get(who).prep(q[9:])
            else:
                r = get(who).q(q)
        except Exception as e:
            r = {'client': repr(e)}
        res.append([cid, who, q, r])
    for _, _, q in clean:
        get('su').q(q)
    return res

if __name__ == '__main__':
    import json, sys
    sys.path.insert(0, __import__('os').path.dirname(__import__('os').path.abspath(__file__)))
    from auth_cases import BATTERIES
    port, out = int(sys.argv[1]), sys.argv[2]
    json.dump({name: run_battery(port, clean, cases) for name, clean, cases in BATTERIES}, open(out, 'w'), indent=1)
    print('done')
