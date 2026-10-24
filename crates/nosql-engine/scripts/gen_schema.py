# Generates crates/nosql-engine/src/tests/captured_schema.rs from the catalog captures: the
# sample schema DDL outcomes and the system_schema rows it produced, filtered to what the engine
# implements (no functions, triggers, masks yet).
import ast
import json, re, sys
outdir, dest = sys.argv[1], sys.argv[2]
cat = {d: json.load(open(f'{outdir}/{d}-catalog.json')) for d in ('scylla', 'cassandra')}

OUT_OF_SCOPE = re.compile(r'^(trigger|masked)')
SNAP_TABLES = ['keyspaces', 'tables', 'columns', 'dropped_columns', 'triggers', 'types', 'functions',
               'aggregates', 'scylla_tables', 'scylla_keyspaces', 'column_masks', 'views', 'indexes',
               'view_virtual_columns', 'computed_columns']
MASK = {'tables': ['id'], 'scylla_tables': ['version'], 'dropped_columns': ['dropped_time'],
        'dropped_columns raw': ['dropped_time_raw'], 'views': ['id', 'base_table_id']}
# ScyllaDB's vector index options carry a creation timeuuid.
def index_options(v):
    """ScyllaDB's vector index options: `index_version` is a creation id and the map comes in hash
    order, so the version is masked and the entries sorted (`canon_index_options` in the harness)."""
    if not (isinstance(v, str) and v.startswith('{') and "'index_version': '" in v):
        return v
    m = ast.literal_eval(v)
    m['index_version'] = '*'
    return '{' + ', '.join('%r: %r' % kv for kv in sorted(m.items())) + '}'
KS = ['ref', 'ref_simple', 'ref_vnodes']

def canon_ddl(r):
    if not r['ok']:
        e = r['error']
        return f"ERR {e['code']} {e['message']}"
    w = r.get('warnings') or []
    return 'OK' + (f' WARN {w!r}' if w else '')

ddl = []
tables = {d: [] for d in cat}
for i, (name, q, _) in enumerate(cat['scylla']['schema_ddl']):
    skip = bool(OUT_OF_SCOPE.match(name))
    exp = {}
    for d in cat:
        n2, q2, r = cat[d]['schema_ddl'][i]
        assert n2 == name
        exp[d] = canon_ddl(r)
        m = re.match(r'CREATE TABLE (\w+)\.("[^"]+"|\w+)', q)
        if m and r['ok'] and not skip:
            tables[d].append(m.group(1) + '.' + m.group(2).strip('"'))
        # Views (and ScyllaDB's index views) are tables of the snapshots too.
        mv = re.match(r'CREATE MATERIALIZED VIEW (\w+)\.(\w+)', q)
        if mv and r['ok'] and not skip:
            tables[d].append(mv.group(1) + '.' + mv.group(2))
        ix = re.match(r'CREATE (?:CUSTOM )?INDEX (?:IF NOT EXISTS )?("[^"]+"|\w+)? ?ON (\w+)\.', q)
        if ix and r['ok'] and not skip and ix.group(1) and d == 'scylla':
            tables[d].append(ix.group(2) + '.' + ix.group(1).strip('"') + '_index')
    ddl.append((name, q, skip, exp))

# Every view of the snapshots (materialized views, ScyllaDB index views, CDC logs) belongs to an
# in-scope statement.
for d in cat:
    for ks in KS:
        for t in ('views', 'tables'):
            r = cat[d].get(f'ks {ks} {t}')
            if not r or not r['ok']:
                continue
            col = r['columns'].index('view_name' if t == 'views' else 'table_name')
            for row in r['rows']:
                name = f"{ks}.{row[col]}"
                if (t == 'views' or name.endswith('_scylla_cdc_log')) and name not in tables[d]:
                    tables[d].append(name)

def canon_rows(d, key, r, ks):
    if not r['ok']:
        e = r['error']
        if e['class'] == 'DriverException':
            return None
        return f"ERR {e['code']} {e['message']}"
    cols = r['columns']
    tname = key.split(' ', 2)[2]
    mask = MASK.get(tname, [])
    rows = []
    for row in r['rows']:
        if 'table_name' in cols and f"{ks}.{row[cols.index('table_name')]}" not in tables[d]:
            continue
        rows.append([('*' if c in mask else index_options(v)) for c, v in zip(cols, row)])
    return f"ROWS cols={cols!r} types={r['types']!r} rows={rows!r}"

snaps = []
for ks in KS:
    for t in SNAP_TABLES + ['dropped_columns raw']:
        key = f'ks {ks} {t}'
        exp = {}
        for d in cat:
            if key in cat[d]:
                exp[d] = canon_rows(d, key, cat[d][key], ks)
        if t == 'dropped_columns raw':
            q = f"SELECT keyspace_name, table_name, column_name, blobAsBigint(timestampAsBlob(dropped_time)) AS dropped_time_raw, type FROM system_schema.dropped_columns WHERE keyspace_name = '{ks}'"
        else:
            q = f"SELECT * FROM system_schema.{t} WHERE keyspace_name = '{ks}'"
        snaps.append((ks, t, q, exp.get('scylla'), exp.get('cassandra')))

def rs(s):
    return 'r###"' + s + '"###'
def opt(s):
    return 'None' if s is None else f'Some({rs(s)})'

out = ['//! Generated from the reference catalog captures (`docs/nosql/reference/capture/catalog.py`',
       '//! on both images) by `gen_schema.py`; do not edit by hand.', '',
       'use super::captured_schema_harness::{Ddl, Snap};', '',
       'pub const DDL: &[Ddl] = &[']
for name, q, skip, exp in ddl:
    out.append(f'    Ddl {{ id: {rs(name)}, cql: {rs(q)}, skip: {str(skip).lower()}, scylla: {rs(exp["scylla"])}, cassandra: {rs(exp["cassandra"])} }},')
out.append('];')
out.append('')
out.append('/// Tables created by the in-scope statements, per dialect.')
out.append('pub const TABLES_SCYLLA: &[&str] = &[' + ', '.join(rs(t) for t in tables['scylla']) + '];')
out.append('pub const TABLES_CASSANDRA: &[&str] = &[' + ', '.join(rs(t) for t in tables['cassandra']) + '];')
out.append('')
out.append('/// Columns whose values differ between runs of the reference (ids, times).')
out.append('pub const MASK: &[(&str, &[&str])] = &[' + ', '.join(f'({rs(k)}, &[{", ".join(rs(c) for c in v)}])' for k, v in MASK.items()) + '];')
out.append('')
out.append('pub const SNAPSHOTS: &[Snap] = &[')
for ks, t, q, s, c in snaps:
    out.append(f'    Snap {{ keyspace: {rs(ks)}, table: {rs(t)}, query: {rs(q)}, scylla: {opt(s)}, cassandra: {opt(c)} }},')
out.append('];')
open(dest, 'w').write('\n'.join(out) + '\n')
print(len(ddl), len(snaps), tables)
