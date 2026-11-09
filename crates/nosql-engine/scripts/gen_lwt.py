# Generates crates/nosql-engine/src/tests/captured_lwt.rs from the LWT capture
# (docs/nosql/reference/capture/lwt.py, output ~/.cache/roll-nosql/capture/out/lwt.json).
# Usage: python3 -I gen_lwt.py <lwt.json> <dest.rs>
import json, sys

src, dest = sys.argv[1], sys.argv[2]
steps = json.load(open(src))

def rs(s):
    return 'r###"' + s + '"###'

def opt(s):
    return 'None' if s is None else 'Some(%s)' % rs(s)

def vals(vs):
    out = []
    for v in vs:
        if v is None: out.append('V::Null')
        elif v == 'unset': out.append('V::Unset')
        else: out.append('V::Hex(%s)' % rs(v))
    return '&[' + ', '.join(out) + ']'

out = ['//! Generated from the reference LWT capture (`docs/nosql/reference/capture/lwt.py` run on both',
       '//! images) by `scripts/gen_lwt.py`; do not edit by hand. Expected results are in the canonical',
       '//! text form of `super::captured_lwt_harness`.', '',
       'use super::captured_lwt_harness::{L, V};', '',
       'pub const STEPS: &[L] = &[']
for s in steps:
    if s['id'] == 'setup':
        out.append('    L::Setup(%s),' % rs(s['cql']))
        continue
    if s.get('pbatch'):
        kind, stmts = s['pbatch']
        members = ', '.join('(%s, %s)' % (rs(q), vals(v)) for q, v in stmts)
        out.append('    L::Batch { id: %s, kind: %s, statements: &[%s], scylla: %s, cassandra: %s },'
                   % (rs(s['id']), rs(kind), members, rs(s['scylla']), rs(s['cassandra'])))
        continue
    v = 'Some(%s)' % vals(s['values']) if s.get('values') is not None else 'None'
    out.append('    L::Case { id: %s, cql: %s, cl: %s, serial: %s, prep: %s, values: %s, scylla: %s, cassandra: %s },'
               % (rs(s['id']), rs(s['cql']), rs(s['cl']), opt(s['serial']), 'true' if s['prep'] else 'false',
                  v, rs(s['scylla']), rs(s['cassandra'])))
out.append('];')
open(dest, 'w').write('\n'.join(out) + '\n')
print(len(steps))
