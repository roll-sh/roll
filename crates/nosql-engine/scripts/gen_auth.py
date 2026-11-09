# gen_auth.py <capture dir> <out.rs>: turns the auth-enabled references' answers
# (out/scylla-auth-live.json, out/cassandra-auth-live.json from docs/nosql/reference/capture/
# auth_live.py) into src/tests/captured_auth.rs. Run with `python3 -I`.
import json, os, sys

cap, out = sys.argv[1], sys.argv[2]
sys.path.insert(0, cap)
from auth_cases import BATTERIES  # noqa: E402

s = json.load(open(os.path.join(cap, 'out', 'scylla-auth-live.json')))
c = json.load(open(os.path.join(cap, 'out', 'cassandra-auth-live.json')))


def rs(t):
    return 'r###"' + t + '"###'


def keep(row, cql):
    # The shared references hold other roles and keyspaces: keep rows naming auth_ objects (and
    # the cassandra role of LIST ROLES / USERS). The harness filters roll's rows the same way.
    return any('auth_' in v for v in row) or (row and row[0] == 'cassandra' and cql.startswith(('LIST ROLES', 'LIST USERS')))


def exp(r, cql):
    if r.get('sleep'):
        return 'X::Sleep'
    if 'client' in r:
        return 'X::Skip'
    if 'code' in r:
        code = int(r['code'], 16)
        if code == 0x000A:  # a session whose login failed
            return 'X::Skip'
        return 'X::Err(0x%04X, %s)' % (code, rs(r['message']))
    if r.get('ok'):
        return 'X::Ok { kind: "Ready", table: "", cols: &[], rows: &[], warnings: &[] }'
    kind = r.get('kind', 'Prepared')
    cols = ', '.join('(%s, %s)' % (rs(n), rs(t)) for n, t in r.get('cols', []))
    rows = ', '.join('&[%s]' % ', '.join(rs(v) for v in row) for row in r.get('rows', []) if keep(row, cql))
    warnings = ', '.join(rs(w) for w in r.get('warnings', []))
    return 'X::Ok { kind: %s, table: %s, cols: &[%s], rows: &[%s], warnings: &[%s] }' % (
        rs(kind), rs(r.get('table', '')), cols, rows, warnings)


lines = []
for name, clean, cases in BATTERIES:
    sr, cr = s[name], c[name]
    assert len(sr) == len(cr) == len(cases), name
    lines.append('    (%s, &[%s], &[' % (rs(name), ', '.join(rs(q) for _, _, q in clean)))
    for (cid, who, q), a, b in zip(cases, sr, cr):
        lines.append('        A { id: %s, who: %s, cql: %s, scylla: %s, cassandra: %s },' % (
            rs(cid), rs(who), rs(q), exp(a[3], q), exp(b[3], q)))
    lines.append('    ]),')
with open(out, 'w') as f:
    f.write('//! Auth cases generated from the auth-enabled references (`scylladb/scylla:2026.3.3` and\n'
            '//! `cassandra:5.0.9` with `PasswordAuthenticator` + `CassandraAuthorizer`): statement, session\n'
            '//! and what each reference answered. See `docs/nosql/reference/capture/auth_cases.py`.\n\n')
    f.write('#![allow(clippy::all)]\n\nuse super::captured_auth_harness::{A, X};\n\n')
    f.write('pub const BATTERIES: &[(&str, &[&str], &[A])] = &[\n' + '\n'.join(lines) + '\n];\n')
print(sum(len(x[2]) for x in BATTERIES))
