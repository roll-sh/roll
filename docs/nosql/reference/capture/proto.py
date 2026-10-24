# Raw native-protocol probe: OPTIONS/STARTUP per version, error bodies, post-STARTUP edge cases.
import socket, struct, sys, json
sys.path.insert(0, '/cap')
from raw import rs, ls, smap, decode_error
import os
host = os.environ.get('CAP_HOST', '127.0.0.1'); port = 9042
OPC = {0:'ERROR',1:'STARTUP',2:'READY',3:'AUTHENTICATE',5:'OPTIONS',6:'SUPPORTED',8:'RESULT',0x0b:'REGISTER',0x0c:'EVENT',0x0e:'AUTH_CHALLENGE',0x0f:'AUTH_RESPONSE',0x10:'AUTH_SUCCESS'}
def recv_exact(s, n):
    b = b''
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c: break
        b += c
    return b
def frame(ver, flags, opcode, body=b'', stream=1):
    if ver & 0x7f <= 2: return struct.pack('>BBbBI', ver, flags, stream, opcode, len(body)) + body
    return struct.pack('>BBhBI', ver, flags, stream, opcode, len(body)) + body
def read_resp(s):
    first = recv_exact(s, 1)
    if not first: return {'note': 'connection closed without response'}
    v = first[0] & 0x7f; hl = 8 if v <= 2 else 9
    hdr = first + recv_exact(s, hl - 1)
    if len(hdr) < hl: return {'raw': hdr.hex(), 'note': 'short header'}
    if hl == 8: ver, flags, stream, op, ln = struct.unpack('>BBbBI', hdr)
    else: ver, flags, stream, op, ln = struct.unpack('>BBhBI', hdr)
    body = recv_exact(s, ln) if ln < 1 << 20 else b''
    out = {'header_hex': hdr.hex(), 'version_byte': '0x%02x' % ver, 'flags': flags, 'stream': stream, 'opcode': OPC.get(op, op), 'length': ln}
    o = 0
    if flags & 0x08:
        n = struct.unpack_from('>H', body, o)[0]; o += 2; w = []
        for _ in range(n): x, o = rs(body, o); w.append(x)
        out['warnings'] = w
    if op == 0: out['error'] = decode_error(body, o); out['body_hex'] = body.hex()
    elif op == 6:
        n = struct.unpack_from('>H', body, o)[0]; o += 2; m = {}
        for _ in range(n):
            k, o = rs(body, o); c = struct.unpack_from('>H', body, o)[0]; o += 2; vals = []
            for _ in range(c): x, o = rs(body, o); vals.append(x)
            m[k] = vals
        out['supported'] = m
    elif op == 3: out['authenticator'] = rs(body, o)[0]
    else: out['body_hex'] = body[:300].hex()
    return out
def probe(raw_bytes):
    s = socket.create_connection((host, port), timeout=5)
    try:
        s.sendall(raw_bytes); r = read_resp(s)
        try:
            s.settimeout(1.5); extra = s.recv(1); r['conn_after'] = 'closed' if extra == b'' else 'more-data'
        except socket.timeout: r['conn_after'] = 'open'
        except Exception as e: r['conn_after'] = 'reset:' + type(e).__name__
        return r
    except Exception as e: return {'exception': repr(e)}
    finally: s.close()
S = lambda d: smap(d)
res = {}
for v in [1, 2, 3, 4, 5, 6, 7, 0x7f]:
    res['OPTIONS v%d' % v] = probe(frame(v, 0, 5))
res['OPTIONS v5 USE_BETA'] = probe(frame(5, 0x10, 5))
res['OPTIONS v6 USE_BETA'] = probe(frame(6, 0x10, 5))
res['OPTIONS response-direction byte 0x84'] = probe(frame(0x84, 0, 5))
for v in [2, 3, 4, 5, 6]:
    res['STARTUP v%d CQL_VERSION=3.0.0' % v] = probe(frame(v, 0, 1, S({'CQL_VERSION': '3.0.0'})))
res['STARTUP v5 USE_BETA'] = probe(frame(5, 0x10, 1, S({'CQL_VERSION': '3.0.0'})))
res['STARTUP v6 USE_BETA'] = probe(frame(6, 0x10, 1, S({'CQL_VERSION': '3.0.0'})))
res['STARTUP v4 empty map'] = probe(frame(4, 0, 1, S({})))
res['STARTUP v4 CQL_VERSION=2.0.0'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '2.0.0'})))
res['STARTUP v4 CQL_VERSION=3.4.7'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.4.7'})))
res['STARTUP v4 CQL_VERSION=abc'] = probe(frame(4, 0, 1, S({'CQL_VERSION': 'abc'})))
res['STARTUP v4 COMPRESSION=zstd'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.0.0', 'COMPRESSION': 'zstd'})))
res['STARTUP v4 COMPRESSION=lz4'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.0.0', 'COMPRESSION': 'lz4'})))
res['STARTUP v4 COMPRESSION=snappy'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.0.0', 'COMPRESSION': 'snappy'})))
res['STARTUP v4 unknown option'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.0.0', 'FOO': 'bar'})))
res['STARTUP v4 DRIVER_NAME etc'] = probe(frame(4, 0, 1, S({'CQL_VERSION': '3.0.0', 'DRIVER_NAME': 'x', 'DRIVER_VERSION': '1', 'APPLICATION_NAME': 'a', 'THROW_ON_OVERLOAD': '1', 'NO_COMPACT': 'true'})))
res['STARTUP v4 compressed flag set'] = probe(frame(4, 0x01, 1, S({'CQL_VERSION': '3.0.0'})))
res['STARTUP v4 truncated body'] = probe(frame(4, 0, 1, b'\x00\x05'))
res['QUERY v4 before STARTUP'] = probe(frame(4, 0, 7, ls('SELECT key FROM system.local') + struct.pack('>HB', 1, 0)))
res['unknown opcode 0x2a v4'] = probe(frame(4, 0, 0x2a))
res['REGISTER v4 before STARTUP'] = probe(frame(4, 0, 0x0b, struct.pack('>H', 1) + struct.pack('>H', 13) + b'SCHEMA_CHANGE'))
res['AUTH_RESPONSE v4 before STARTUP'] = probe(frame(4, 0, 0x0f, struct.pack('>i', 2) + b'\x00\x00'))
res['response opcode READY sent by client'] = probe(frame(4, 0, 2))
res['frame length 300MB'] = probe(struct.pack('>BBhBI', 4, 0, 1, 7, 300 * 1024 * 1024))
res['negative stream id OPTIONS'] = probe(frame(4, 0, 5, stream=-5))
# post-STARTUP probes on one connection
def session(frames, v=4, opts=None):
    s = socket.create_connection((host, port), timeout=5); outs = []
    s.sendall(frame(v, 0, 1, S(opts or {'CQL_VERSION': '3.0.0'}))); first = read_resp(s)
    for name, op, body, *fl in frames:
        try:
            s.sendall(frame(v, fl[0] if fl else 0, op, body)); outs.append([name, read_resp(s)])
        except Exception as e:
            outs.append([name, {'exception': repr(e)}]); break
    s.close(); return first, outs
q = lambda text, cl=1, flags=0, tail=b'': ls(text) + struct.pack('>HB', cl, flags) + tail
frames = [
  ('QUERY ok system.local', 0x07, q('SELECT key FROM system.local')),
  ('EXECUTE unknown id', 0x0a, struct.pack('>H', 4) + b'\xde\xad\xbe\xef' + struct.pack('>HB', 1, 0)),
  ('QUERY syntax error', 0x07, q('SELEC 1')),
  ('QUERY unknown keyspace', 0x07, q('SELECT * FROM nope.t')),
  ('QUERY consistency 0x0042', 0x07, q('SELECT key FROM system.local', 0x42)),
  ('QUERY consistency SERIAL on read', 0x07, q('SELECT key FROM system.local', 8)),
  ('QUERY consistency ANY on read', 0x07, q('SELECT key FROM system.local', 0)),
  ('QUERY page_size 0', 0x07, q('SELECT key FROM system.local', 1, 0x04, struct.pack('>i', 0))),
  ('QUERY page_size -1', 0x07, q('SELECT key FROM system.local', 1, 0x04, struct.pack('>i', -1))),
  ('QUERY bogus paging state', 0x07, q('SELECT key FROM system.local', 1, 0x08, struct.pack('>i', 4) + b'\x01\x02\x03\x04')),
  ('QUERY with tracing', 0x07, q('SELECT key FROM system.local'), 0x02),
  ('QUERY values but no markers', 0x07, q('SELECT key FROM system.local', 1, 0x01, struct.pack('>H', 1) + struct.pack('>i', 4) + b'\x00\x00\x00\x01')),
  ('QUERY unknown flag bits 0x80', 0x07, q('SELECT key FROM system.local', 1, 0x80)),
  ('PREPARE syntax error', 0x09, ls('SELEC 1')),
  ('PREPARE ok', 0x09, ls('SELECT key FROM system.local WHERE key = ?')),
  ('BATCH empty logged', 0x0d, struct.pack('>BH', 0, 0) + struct.pack('>HB', 1, 0)),
  ('BATCH type 7', 0x0d, struct.pack('>BH', 7, 0) + struct.pack('>HB', 1, 0)),
  ('BATCH with SELECT', 0x0d, struct.pack('>BH', 0, 1) + b'\x00' + ls('SELECT key FROM system.local') + struct.pack('>H', 0) + struct.pack('>HB', 1, 0)),
  ('STARTUP again', 0x01, S({'CQL_VERSION': '3.0.0'})),
  ('OPTIONS after STARTUP', 0x05, b''),
  ('REGISTER unknown event', 0x0b, struct.pack('>H', 1) + struct.pack('>H', 4) + b'NOPE'),
  ('REGISTER all events', 0x0b, struct.pack('>H', 3) + b''.join(struct.pack('>H', len(x)) + x for x in (b'TOPOLOGY_CHANGE', b'STATUS_CHANGE', b'SCHEMA_CHANGE'))),
  ('AUTH_RESPONSE after READY', 0x0f, struct.pack('>i', 2) + b'\x00\x00'),
  ('truncated QUERY body', 0x07, b'\x00\x00'),
]
first, outs = session(frames)
res['post-STARTUP v4: STARTUP'] = first
for n, r in outs: res['post-STARTUP v4: ' + n] = r
first, outs = session(frames[:4], v=3)
res['post-STARTUP v3: STARTUP'] = first
for n, r in outs: res['post-STARTUP v3: ' + n] = r
print(json.dumps(res, indent=1))
