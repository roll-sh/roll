# Dynamic data masking on the feature-enabled references: `python3 -I masking_live.py <port>
# <out.json>` against nosql-ref-cassandra-auth (29043: PasswordAuthenticator + CassandraAuthorizer,
# dynamic_data_masking_enabled, user_defined_functions_enabled, materialized_views_enabled) or
# nosql-ref-scylla-auth (19043, which has no masking). Output goes to
# ~/.cache/roll-nosql/capture/out/{cassandra,scylla}-masking-live.json.
#
# A raw protocol v4 client like auth_live.py's, with more types (bigint/smallint/tinyint as
# numbers, UDTs as `ks.name`, tuples), paging and prepared statements. Values render as text
# (text as is, booleans True/False, integers, collections python-like, anything else 0x-hex).
# Cases are in masking_cases.py; crates/nosql-engine/scripts/gen_masking.py turns the two
# outputs into crates/nosql-engine/src/tests/captured_masking.rs.
import socket, struct

def ls(x): b = x.encode(); return struct.pack('>i', len(b)) + b
def rs(b, o):
    n = struct.unpack_from('>H', b, o)[0]; return b[o+2:o+2+n].decode(), o+2+n
def rbytes(b, o):
    n = struct.unpack_from('>i', b, o)[0]; o += 4
    return (None, o) if n < 0 else (b[o:o+n], o+n)
TYPES = {1:'ascii',2:'bigint',3:'blob',4:'boolean',5:'counter',6:'decimal',7:'double',8:'float',9:'int',11:'timestamp',12:'uuid',13:'text',14:'varint',15:'timeuuid',16:'inet',17:'date',18:'time',19:'smallint',20:'tinyint',21:'duration'}
def rtype(b, o):
    t = struct.unpack_from('>H', b, o)[0]; o += 2
    if t == 0: s, o = rs(b, o); return 'custom(%s)' % s, o
    if t in (32, 34): e, o = rtype(b, o); return '%s<%s>' % ('list' if t == 32 else 'set', e), o
    if t == 33: k, o = rtype(b, o); v, o = rtype(b, o); return 'map<%s, %s>' % (k, v), o
    if t == 48:
        ks, o = rs(b, o); name, o = rs(b, o); n = struct.unpack_from('>H', b, o)[0]; o += 2
        for _ in range(n): _, o = rs(b, o); _, o = rtype(b, o)
        return '%s.%s' % (ks, name), o
    if t == 49:
        n = struct.unpack_from('>H', b, o)[0]; o += 2; ts = []
        for _ in range(n): x, o = rtype(b, o); ts.append(x)
        return 'tuple<%s>' % ', '.join(ts), o
    return TYPES.get(t, str(t)), o
INTS = {'int': '>i', 'bigint': '>q', 'counter': '>q', 'smallint': '>h', 'tinyint': '>b'}
def fmt(ty, v):
    if v is None: return 'null'
    if ty in ('text', 'ascii'): return v.decode('utf8', 'replace')
    if ty == 'boolean': return 'True' if v and v[0] else 'False'
    if ty in INTS and len(v) == struct.calcsize(INTS[ty]): return str(struct.unpack(INTS[ty], v)[0])
    if ty.startswith('set<') or ty.startswith('list<'):
        inner = ty[ty.index('<')+1:-1]; n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            x, o = rbytes(v, o); items.append(fmt1(inner, x))
        return ('{%s}' if ty.startswith('set') else '[%s]') % ', '.join(items)
    if ty.startswith('map<'):
        k, val = split2(ty[4:-1]); n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            a, o = rbytes(v, o); b, o = rbytes(v, o)
            items.append('%s: %s' % (fmt1(k, a), fmt1(val, b)))
        return '{%s}' % ', '.join(items)
    return '0x' + v.hex()
def split2(s):
    depth = 0
    for i, ch in enumerate(s):
        if ch == '<': depth += 1
        elif ch == '>': depth -= 1
        elif ch == ',' and depth == 0: return s[:i], s[i+2:]
    return s, ''
def fmt1(ty, v):
    s = fmt(ty, v)
    return "'%s'" % s if ty in ('text', 'ascii') and v is not None else s
def decode_error(b):
    code = struct.unpack_from('>i', b, 0)[0]; m, _ = rs(b, 4)
    return {'code': '0x%04X' % code, 'message': m}

class C:
    def __init__(s, port, user, pw):
        s.s = socket.create_connection(('127.0.0.1', port), timeout=60); s.st = 0
        body = struct.pack('>H', 1) + struct.pack('>H', 11) + b'CQL_VERSION' + struct.pack('>H', 5) + b'3.0.0'
        op, fl, b = s.req(1, body)
        s.ok = True
        if op == 3:
            token = b'\0' + user.encode() + b'\0' + pw.encode()
            op, fl, b = s.req(0x0f, struct.pack('>i', len(token)) + token)
            s.ok = op != 0
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
    def params(s, page, state):
        flags = (4 if page else 0) | (8 if state else 0)
        out = struct.pack('>HB', 1, flags)
        if page: out += struct.pack('>i', page)
        if state: out += struct.pack('>i', len(state)) + state
        return out
    def q(s, text, page=None, state=None):
        return s.result(*s.req(7, ls(text) + s.params(page, state)))
    def prepare(s, text):
        op, fl, b = s.req(9, ls(text))
        o = 0
        if fl & 8:
            n = struct.unpack_from('>H', b, o)[0]; o += 2
            for _ in range(n): _, o = rs(b, o)
        b = b[o:]
        if op == 0: return None, decode_error(b)
        n = struct.unpack_from('>H', b, 4)[0]
        return b[6:6+n], None
    def execute(s, pid, page=None, state=None):
        return s.result(*s.req(10, struct.pack('>H', len(pid)) + pid + s.params(page, state)))
    def result(s, op, fl, b):
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
        if mf & 2:
            st, o = rbytes(b, o); out['paging'] = st
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
                x, o = rbytes(b, o); r.append(fmt(cols[ci][1], x))
            rows.append(r)
        out['rows'] = rows
        return out

def paged(run, page):
    """Every page of a paged read: the first page's result with `pages` (rows per page)."""
    pages, state, first = [], None, None
    while True:
        r = run(page, state)
        if 'rows' not in r: return r
        first = first or r
        pages.append(r['rows'])
        state = r.pop('paging', None)
        if not state or len(pages) > 50: break
    first.pop('paging', None)
    first['rows'] = [row for p in pages for row in p]
    first['pages'] = [len(p) for p in pages]
    return first

# --- runner -------------------------------------------------------------------------------------
import os, sys  # noqa: E402
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from masking_cases import CREDS, BATTERIES  # noqa: E402

def run_battery(port, clean, cases):
    import time
    conns = {}
    def get(who):
        if who not in conns:
            u, p = CREDS[who]
            c = C(port, u, p)
            conns[who] = c if c.ok else None
        return conns[who]
    res = []
    for q in clean:
        get('su').q(q)
    for cid, who, q in cases:
        if q.startswith('!sleep'):
            res.append([cid, who, q, {'sleep': True}])
            time.sleep(float(q[6:]))
            continue
        try:
            c = get(who)
            if c is None:
                r = {'client': 'login failed'}
            elif q.startswith('!page '):
                n, text = q[6:].split(' ', 1)
                r = paged(lambda p, s: c.q(text, p, s), int(n))
            elif q.startswith('!prepared '):
                pid, e = c.prepare(q[10:])
                r = e or c.execute(pid)
            else:
                r = c.q(q)
                r.pop('paging', None)
        except Exception as e:
            r = {'client': repr(e)}
        res.append([cid, who, q, r])
    for q in clean:
        get('su').q(q)
    return res

if __name__ == '__main__':
    import json, sys
    port, out = int(sys.argv[1]), sys.argv[2]
    json.dump({name: run_battery(port, clean, cases) for name, clean, cases in BATTERIES}, open(out, 'w'), indent=1)
    print('done')
