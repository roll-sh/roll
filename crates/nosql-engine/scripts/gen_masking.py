# gen_masking.py <capture dir> <out.rs>: turns the feature-enabled references' masking answers
# (out/scylla-masking-live.json, out/cassandra-masking-live.json from
# docs/nosql/reference/capture/masking_live.py, in ~/.cache/roll-nosql/capture) into
# src/tests/captured_masking.rs. Run with `python3 -I`.
import json, os, sys

cap, out = sys.argv[1], sys.argv[2]
sys.path.insert(0, cap)
from masking_cases import BATTERIES  # noqa: E402

outdir = os.path.expanduser('~/.cache/roll-nosql/capture/out')
s = json.load(open(os.path.join(outdir, 'scylla-masking-live.json')))
c = json.load(open(os.path.join(outdir, 'cassandra-masking-live.json')))


def rs(t):
    return 'r###"' + t + '"###'


def exp(r):
    if r.get('sleep'):
        return 'R::Sleep'
    if 'client' in r:
        return 'R::Skip'
    if 'code' in r:
        return 'R::Err(0x%04X, %s)' % (int(r['code'], 16), rs(r['message']))
    cols = ', '.join('(%s, %s)' % (rs(n), rs(t)) for n, t in r.get('cols', []))
    rows = ', '.join('&[%s]' % ', '.join(rs(v) for v in row) for row in r.get('rows', []))
    pages = ', '.join(str(p) for p in r.get('pages', []))
    warnings = ', '.join(rs(w) for w in r.get('warnings', []))
    return 'R::Ok { kind: %s, table: %s, cols: &[%s], rows: &[%s], pages: &[%s], warnings: &[%s] }' % (
        rs(r['kind']), rs(r.get('table', '')), cols, rows, pages, warnings)


lines = []
for name, clean, cases in BATTERIES:
    sr, cr = s[name], c[name]
    assert len(sr) == len(cr) == len(cases), name
    lines.append('    (%s, &[%s], &[' % (rs(name), ', '.join(rs(q) for q in clean)))
    for (cid, who, q), a, b in zip(cases, sr, cr):
        lines.append('        Case { id: %s, who: %s, cql: %s, scylla: %s, cassandra: %s },' % (
            rs(cid), rs(who), rs(q), exp(a[3]), exp(b[3])))
    lines.append('    ]),')
with open(out, 'w') as f:
    f.write('//! Dynamic data masking cases generated from the feature-enabled references\n'
            '//! (`cassandra:5.0.9` with masking, UDFs and views enabled, `scylladb/scylla:2026.3.3`, both with\n'
            '//! `PasswordAuthenticator` + `CassandraAuthorizer`): statement, session and what each reference\n'
            '//! answered. See `docs/nosql/reference/capture/masking_cases.py`.\n\n')
    f.write('#![allow(clippy::all)]\n\nuse super::captured_masking_harness::{Case, R};\n\n')
    f.write('pub const BATTERIES: &[(&str, &[&str], &[Case])] = &[\n' + '\n'.join(lines) + '\n];\n')
print(sum(len(x[2]) for x in BATTERIES))
