# Generates crates/nosql-engine/src/tests/captured_describe.rs from the reference catalog
# captures (docs/nosql/reference/capture/catalog.py): the sample schema statements and every
# DESCRIBE statement's rows, on the default images (`catalog`) and with UDFs and materialized
# views enabled (`catalog-suite`). Rows of keyspaces other agents left on the shared references
# and keyspace-less rows (ScyllaDB's roles and grants, covered by tests/describe.rs) are dropped.
#
#   python3 -I gen_describe.py ~/.cache/roll-nosql/capture/out ../src/tests/captured_describe.rs
import json, sys

outdir, dest = sys.argv[1], sys.argv[2]
SYSTEM = {
    'scylla': {'system', 'system_schema', 'system_auth', 'system_distributed', 'system_traces',
               'system_replicated_keys', 'audit'},
    'cassandra': {'system', 'system_schema', 'system_auth', 'system_distributed', 'system_traces',
                  'system_views', 'system_virtual_schema'},
}
KEEP = {'ref', 'ref_simple', 'ref_vnodes'}


def rs(s):
    return 'r###"' + s + '"###'


def opt(s):
    return 'None' if s is None else f'Some({rs(s)})'


def want(d, r):
    if not r['ok']:
        e = r['error']
        return f'W::Err({e["code"]}, {rs(e["message"])})'
    rows = []
    for row in r['rows']:
        if r['columns'][0] == 'keyspace_name':
            ks = row[0]
            if ks is None or (ks not in KEEP and ks not in SYSTEM[d]):
                continue
        rows.append('&[' + ', '.join(opt(v) for v in row) + ']')
    cols = ', '.join(rs(c) for c in r['columns'])
    return f'W::Rows(&[{cols}], &[' + ', '.join(rows) + '])'


out = ['//! Generated from the reference catalog captures (`docs/nosql/reference/capture/catalog.py`',
       '//! on both images, also with UDFs and views enabled) by `scripts/gen_describe.py`; do not',
       '//! edit by hand.', '',
       'use super::captured_describe_harness::{Aggregate, Case, Function, W};', '']
for suite in ('catalog', 'catalog-suite'):
    cat = {d: json.load(open(f'{outdir}/{d}-{suite}.json')) for d in ('scylla', 'cassandra')}
    up = suite.replace('-', '_').upper()
    ddl = [q for _, q, _ in cat['scylla']['schema_ddl']]
    assert ddl == [q for _, q, _ in cat['cassandra']['schema_ddl']]
    out.append(f'pub const {up}_DDL: &[&str] = &[')
    out.extend(f'    {rs(q)},' for q in ddl)
    out.append('];')
    out.append('')
    out.append(f'pub const {up}: &[Case] = &[')
    s = dict((q, r) for q, r in cat['scylla']['describe'])
    c = dict((q, r) for q, r in cat['cassandra']['describe'])
    for q, _ in cat['scylla']['describe']:
        if q not in c:
            continue
        out.append(f'    Case {{ cql: {rs(q)}, scylla: {want("scylla", s[q])}, cassandra: {want("cassandra", c[q])} }},')
    out.append('];')
    out.append('')
# The suite's user functions and aggregates, injected when the UDF subsystem doesn't create them.
suite = {d: json.load(open(f'{outdir}/{d}-catalog-suite.json')) for d in ('scylla', 'cassandra')}
for d in ('scylla', 'cassandra'):
    up = d.upper()
    out.append(f'/// `system_schema.functions` rows of the suite on {d}.')
    out.append(f'pub const SUITE_FUNCTIONS_{up}: &[Function] = &[')
    for _, name, types, names, body, called, lang, ret in suite[d]['ks ref functions']['rows']:
        out.append(f'    Function {{ name: {rs(name)}, argument_types: &[{", ".join(rs(t) for t in types)}], argument_names: &[{", ".join(rs(n) for n in names)}], body: {rs(body)}, called_on_null_input: {str(called).lower()}, language: {rs(lang)}, return_type: {rs(ret)} }},')
    out.append('];')
    out.append(f'pub const SUITE_AGGREGATES_{up}: &[Aggregate] = &[')
    for _, name, types, final, init, ret, sfunc, stype in suite[d]['ks ref aggregates']['rows']:
        out.append(f'    Aggregate {{ name: {rs(name)}, argument_types: &[{", ".join(rs(t) for t in types)}], final_func: {opt(final)}, initcond: {opt(init)}, return_type: {rs(ret)}, state_func: {rs(sfunc)}, state_type: {rs(stype)} }},')
    out.append('];')
    out.append('')
open(dest, 'w').write('\n'.join(out))
print('ok')
