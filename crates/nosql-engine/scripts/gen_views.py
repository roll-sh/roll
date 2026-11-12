# gen_views.py <cassandra-views-live.json> <out.rs>: turns the live Cassandra materialized view and
# SASI capture (docs/nosql/reference/capture/views_live.py, run against nosql-ref-cassandra-auth)
# into src/tests/captured_views.rs. Run with `python3 -I`.
import json, sys

src, dest = sys.argv[1], sys.argv[2]
steps = json.load(open(src))


def rs(t):
    return 'r###"' + t + '"###'


def one(r):
    if 'code' in r:
        return 'R::Err(0x%04X, %s)' % (int(r['code'], 16), rs(r['message']))
    cols = ', '.join('(%s, %s)' % (rs(n), rs(t)) for n, t in r.get('cols', []))
    rows = ', '.join('&[%s]' % ', '.join(rs(v) for v in row) for row in r.get('rows', []))
    warnings = ', '.join(rs(w) for w in r.get('warnings', []))
    return 'R::Ok { kind: %s, change: %s, cols: &[%s], rows: &[%s], warnings: &[%s] }' % (
        rs(r['kind']), rs(r.get('change', '')), cols, rows, warnings)


out = ['//! Generated from the live Cassandra materialized view and SASI capture',
       '//! (`docs/nosql/reference/capture/views_live.py` against `cassandra:5.0.9` with',
       '//! `materialized_views_enabled` and `sasi_indexes_enabled`) by `scripts/gen_views.py`; do not',
       '//! edit by hand. Replayed by `super::captured_views_harness`.', '',
       'use super::captured_views_harness::{R, V};', '',
       'pub const STEPS: &[V] = &[']
n = 0
for s in steps:
    cid, cql = s['id'], s['cql']
    if cid == 'setup':
        out.append('    V::Setup(%s),' % rs(cql))
    elif cid == 'wait':
        out.append('    V::Wait,')
    elif cid == 'sleep':
        out.append('    V::Sleep(%s),' % float(cql))
    else:
        r = s['result']
        if 'client' in r:
            want = '&[]'
        else:
            want = '&[%s]' % ', '.join(one(p) for p in r.get('pages', [r]))
        page = 'Some(%d)' % s['page'] if s.get('page') else 'None'
        out.append('    V::Case { id: %s, cql: %s, page: %s, sort: %s, want: %s },' % (
            rs(cid), rs(cql), page, 'true' if s.get('sort') else 'false', want))
        n += 1
out.append('];')
open(dest, 'w').write('\n'.join(out) + '\n')
print(n)
