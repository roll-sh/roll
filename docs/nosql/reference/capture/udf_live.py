# UDF behaviour of the feature-enabled references: `python3 -I udf_live.py <port> <scylla|cassandra>
# <out.json>` against nosql-ref-scylla-auth (19043: Lua UDFs enabled) or nosql-ref-cassandra-auth
# (29043: Java UDFs enabled), superuser cassandra/cassandra. A raw protocol v4 client (auth_live.C):
# text as is, booleans True/False, ints, sets/lists/maps python-like, other types (tuples, UDTs,
# vectors included) as 0x-hex. Cases are in udf_cases.py (only the batteries of the dialect run);
# crates/nosql-engine/scripts/gen_udf.py turns both outputs into
# crates/nosql-engine/src/tests/captured_udf.rs.
import os, struct, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import auth_live  # noqa: E402
from auth_live import rs  # noqa: E402

_rtype = auth_live.rtype


def rtype(b, o):
    t = struct.unpack_from('>H', b, o)[0]
    if t == 0x30:  # UDT: keyspace, name, fields
        ks, o2 = rs(b, o + 2)
        name, o2 = rs(b, o2)
        n = struct.unpack_from('>H', b, o2)[0]; o2 += 2
        for _ in range(n):
            _, o2 = rs(b, o2)
            _, o2 = rtype(b, o2)
        return 'udt(%s.%s)' % (ks, name), o2
    if t == 0x31:  # tuple
        n = struct.unpack_from('>H', b, o + 2)[0]; o2 = o + 4; items = []
        for _ in range(n):
            x, o2 = rtype(b, o2); items.append(x)
        return 'tuple<%s>' % ', '.join(items), o2
    if t in (32, 34):
        e, o2 = rtype(b, o + 2)
        return '%s<%s>' % ('list' if t == 32 else 'set', e), o2
    if t == 33:
        k, o2 = rtype(b, o + 2); v, o2 = rtype(b, o2)
        return 'map<%s, %s>' % (k, v), o2
    return _rtype(b, o)


auth_live.rtype = rtype
_fmt = auth_live.fmt


def fmt(ty, v):
    if v is not None and (ty.startswith('tuple<') or ty.startswith('udt(')):
        return '0x' + v.hex()
    return _fmt(ty, v)


auth_live.fmt = fmt


def run_battery(port, setup, cases):
    # A case is (id, cql) run as the superuser, or (id, cql, role) run as that role (password =
    # its name; sessions are opened on first use and kept).
    c = auth_live.C(port, 'cassandra', 'cassandra')
    sessions = {}
    for q in setup:
        c.q(q)
    res = []
    for case in cases:
        cid, q = case[0], case[1]
        who = case[2] if len(case) > 2 else None
        if q.startswith('!sleep'):  # waits out Cassandra's auth caches; not replayed
            import time
            time.sleep(float(q[6:]))
            res.append([cid, q, {'sleep': True}])
            continue
        try:
            if who is None:
                r = c.q(q)
            else:
                if who not in sessions:
                    sessions[who] = auth_live.C(port, who, who)
                r = sessions[who].q(q)
        except Exception as e:  # the reference closed the connection (it shouldn't: see udf_cases.py)
            r = {'client': repr(e)}
            c = auth_live.C(port, 'cassandra', 'cassandra')
        res.append([cid, q, r] + ([who] if who else []))
    for q in setup:
        if q.startswith('CREATE KEYSPACE'):
            c.q('DROP KEYSPACE IF EXISTS ' + q.split()[2])
        if q.startswith('CREATE ROLE'):
            c.q('DROP ROLE IF EXISTS ' + q.split()[2])
    return res


if __name__ == '__main__':
    import json
    from udf_cases import BATTERIES
    port, dialect, out = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    result = {}
    for name, dialects, setup, cases in BATTERIES:
        if dialect in dialects:
            result[name] = run_battery(port, setup, cases)
            print(name, len(cases), file=sys.stderr)
    json.dump(result, open(out, 'w'), indent=1)
    print('done')
