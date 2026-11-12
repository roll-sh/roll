# Generates crates/nosql-engine/src/tests/captured_derived.rs from the derived-data capture
# (docs/nosql/reference/capture/derived.py, output ~/.cache/roll-nosql/capture/out/derived.json).
# Usage: python3 -I gen_derived.py <derived.json> <dest.rs>
import json, sys

src, dest = sys.argv[1], sys.argv[2]
steps = json.load(open(src))

def rs(s):
    return 'r###"' + s + '"###'

out = ['//! Generated from the reference derived-data capture (`docs/nosql/reference/capture/derived.py`',
       '//! run on both images) by `scripts/gen_derived.py`; do not edit by hand. Expected results are in',
       '//! the canonical text form of `super::captured_derived_harness`.', '',
       'use super::captured_derived_harness::D;', '',
       'pub const STEPS: &[D] = &[']
for s in steps:
    if s['id'] == 'setup':
        out.append('    D::Setup(%s),' % rs(s['cql']))
        continue
    page = 'Some(%d)' % s['page'] if s.get('page') else 'None'
    mask = '&[' + ', '.join(rs(m) for m in (s.get('mask') or [])) + ']'
    out.append('    D::Case { id: %s, cql: %s, page: %s, mask: %s, sort: %s, cl: %s, scylla: %s, cassandra: %s },'
               % (rs(s['id']), rs(s['cql']), page, mask, 'true' if s.get('sort') else 'false',
                  rs(s.get('cl') or 'ONE'), rs(s['scylla']), rs(s['cassandra'])))
out.append('];')
open(dest, 'w').write('\n'.join(out) + '\n')
print(len(steps))
