# gen_udf.py <capture dir> <json dir> <out.rs>: turns the UDF-enabled references' answers
# (<json dir>/scylla-udf-live.json, cassandra-udf-live.json from docs/nosql/reference/capture/
# udf_live.py, usually ~/.cache/roll-nosql/capture/out) into src/tests/captured_udf.rs.
# Run with `python3 -I`.
import json, os, sys

cap, jdir, out = sys.argv[1], sys.argv[2], sys.argv[3]
sys.path.insert(0, cap)
from udf_cases import BATTERIES  # noqa: E402

runs = {d: json.load(open(os.path.join(jdir, '%s-udf-live.json' % d))) for d in ('scylla', 'cassandra')}


def rs(t):
    return 'r###"' + t + '"###'


def exp(r):
    if 'client' in r:
        raise SystemExit('client error in the capture: %r' % r)
    if 'code' in r:
        return 'X::Err(0x%04X, %s)' % (int(r['code'], 16), rs(r['message']))
    cols = ', '.join('(%s, %s)' % (rs(n), rs(t)) for n, t in r.get('cols', []))
    rows = ', '.join('&[%s]' % ', '.join(rs(v) for v in row) for row in r.get('rows', []))
    warnings = ', '.join(rs(w) for w in r.get('warnings', []))
    return 'X::Ok { kind: %s, table: %s, cols: &[%s], rows: &[%s], warnings: &[%s] }' % (
        rs(r['kind']), rs(r.get('table', '')), cols, rows, warnings)


parts = []
for d in ('scylla', 'cassandra'):
    lines = []
    for name, dialects, setup, cases in BATTERIES:
        if d not in dialects:
            continue
        got = runs[d][name]
        assert len(got) == len(cases), (d, name)
        lines.append('    (%s, &[%s], &[' % (rs(name), ', '.join(rs(q) for q in setup)))
        for case, g in zip(cases, got):
            cid, q = case[0], case[1]
            who = case[2] if len(case) > 2 else ''
            assert g[0] == cid and g[1] == q, (d, name, cid)
            if q.startswith('!sleep'):
                continue
            lines.append('        U { id: %s, who: %s, cql: %s, want: %s },' % (rs(cid), rs(who), rs(q), exp(g[2])))
        lines.append('    ]),')
    parts.append('pub const %s: &[(&str, &[&str], &[U])] = &[\n%s\n];\n' % (d.upper(), '\n'.join(lines)))

with open(out, 'w') as f:
    f.write('//! UDF / UDA cases generated from the UDF-enabled references (`scylladb/scylla:2026.3.3` with\n'
            '//! Lua UDFs, `cassandra:5.0.9` with Java UDFs; both with auth): statement and what the reference\n'
            '//! answered. See `docs/nosql/reference/capture/udf_cases.py`; regenerate with\n'
            '//! `scripts/gen_udf.py`.\n\n')
    f.write('#![allow(clippy::all)]\n\nuse super::captured_udf_harness::{U, X};\n\n')
    f.write('\n'.join(parts))
print(sum(len(b[3]) * len(b[1]) for b in BATTERIES))
