# Generates crates/nosql-engine/src/system/catalog_data.rs from the reference catalog captures
# (docs/nosql/reference/capture/catalog.py on both images): every table of the built-in keyspaces
# with its exact columns and `system_schema.tables` options, Cassandra's virtual tables
# (`system_virtual_schema`), ScyllaDB's system user types and the indexes on system tables.
#
#   python3 -I gen_system_catalog.py ~/.cache/roll-nosql/capture/out ../src/system/catalog_data.rs
import ast, json, re, struct, sys

outdir, dest = sys.argv[1], sys.argv[2]
cat = {d: json.load(open(f'{outdir}/{d}-catalog.json')) for d in ('scylla', 'cassandra')}
SYSTEM = {
    'scylla': ['system', 'system_schema', 'system_auth', 'system_distributed', 'system_traces',
               'system_replicated_keys', 'audit'],
    'cassandra': ['system', 'system_schema', 'system_auth', 'system_distributed', 'system_traces'],
}
KIND = {'partition_key': 'p', 'clustering': 'c', 'static': 's', 'regular': 'r'}


def rs(s):
    return 'r###"' + s + '"###'


def scylla_map(blob):
    """A map<text, text> as ScyllaDB serialises schema extensions (little-endian lengths)."""
    (n,), i, out = struct.unpack_from('<i', blob, 0), 4, []
    for _ in range(n):
        (kl,) = struct.unpack_from('<i', blob, i); i += 4
        k = blob[i:i + kl].decode(); i += kl
        (vl,) = struct.unpack_from('<i', blob, i); i += 4
        v = blob[i:i + vl].decode(); i += vl
        out.append((k, v))
    return out


def opt_values(name, v):
    """`(option, Rust value)` pairs of one `system_schema.tables` column."""
    if v is None:
        return []
    if name == 'flags':
        return [(name, 'O::Flags(&[' + ', '.join(rs(f) for f in re.findall(r"'([a-z_]+)'", v)) + '])')]
    if name == 'extensions':
        out = []
        for k, b in sorted(ast.literal_eval(v).items()):
            if k != 'tombstone_gc':
                raise SystemExit(f'unknown extension {k}')
            out.append((k, 'O::Map(&[' + ', '.join(f'({rs(a)}, {rs(c)})' for a, c in scylla_map(b)) + '])'))
        return out
    if isinstance(v, bool):
        return [(name, f'O::Bool({str(v).lower()})')]
    if isinstance(v, int):
        return [(name, f'O::Int({v})')]
    if isinstance(v, float):
        return [(name, f'O::Float({v!r})')]
    if isinstance(v, str) and v.startswith('{'):
        m = ast.literal_eval(v)
        return [(name, 'O::Map(&[' + ', '.join(f'({rs(a)}, {rs(b)})' for a, b in sorted(m.items())) + '])')]
    return [(name, f'O::Text({rs(v)})')]


def tables(d):
    R = cat[d]
    cols = {}
    for ks, t, name, kind, pos, order, ty in R['columns_all']['rows']:
        if ks in SYSTEM[d]:
            cols.setdefault((ks, t), []).append((name, ty, KIND[kind], pos, order.lower() == 'desc'))
    opts = {}
    r = R['system_schema_tables_rows']
    for row in r['rows']:
        row = dict(zip(r['columns'], row))
        key = (row['keyspace_name'], row['table_name'])
        o = []
        for c, v in row.items():
            if c not in ('keyspace_name', 'table_name', 'id'):
                o.extend(f'({rs(n)}, {x})' for n, x in opt_values(c, v))
        opts[key] = o
    out = []
    for key in sorted(cols, key=lambda k: (SYSTEM[d].index(k[0]), k[1])):
        out.append((key, cols[key], opts.get(key, []), False))
    if R['virtual_tables']['ok']:
        vcols = {}
        vc = R['virtual_columns']
        for row in vc['rows']:
            row = dict(zip(vc['columns'], row))
            vcols.setdefault((row['keyspace_name'], row['table_name']), []).append(
                (row['column_name'], row['type'], KIND[row['kind']], row['position'], row['clustering_order'] == 'desc'))
        for ks, t, comment in R['virtual_tables']['rows']:
            out.append(((ks, t), vcols[(ks, t)], [f'({rs("comment")}, O::Text({rs(comment)}))'], True))
    return out


def describe_rows(d):
    rows = dict((q, r) for q, r in cat[d]['describe'])['DESCRIBE FULL SCHEMA']['rows']
    return [r for r in rows if r[0] in SYSTEM[d]]


def types(d):
    out = []
    for ks, kind, name, stmt in describe_rows(d):
        if kind != 'type':
            continue
        body = stmt.split('(', 1)[1].rsplit(')', 1)[0]
        fields = [f.strip().rstrip(',').split(' ', 1) for f in body.strip().split('\n')]
        out.append((ks, name, fields))
    return out


def indexes(d):
    out = []
    for ks, kind, name, stmt in describe_rows(d):
        if kind == 'index':
            table = re.search(r' ON \w+\.("[^"]+"|\w+)', stmt).group(1).strip('"')
            out.append((ks, table, name.strip('"'), stmt))
    return out


lines = ['//! Generated from the reference catalog captures (`docs/nosql/reference/capture/catalog.py`',
         '//! on both images) by `scripts/gen_system_catalog.py`; do not edit by hand.', '',
         'use super::catalog::{Cat, O, TypeEntry};', '']
for d in ('scylla', 'cassandra'):
    up = d.upper()
    lines.append(f'pub const {up}: &[Cat] = &[')
    for (ks, t), cs, o, virtual in tables(d):
        cs = sorted(cs, key=lambda c: ('pcsr'.index(c[2]), c[3], c[0]))
        col_s = ', '.join(f'({rs(n)}, {rs(ty)}, {k!r}, {p}, {str(desc).lower()})' for n, ty, k, p, desc in cs)
        lines.append(f'    Cat {{ keyspace: {rs(ks)}, name: {rs(t)}, is_virtual: {str(virtual).lower()}, columns: &[{col_s}], options: &[{", ".join(o)}] }},')
    lines.append('];')
    lines.append('')
    lines.append(f'/// User types of the built-in keyspaces, in the order `DESCRIBE` lists them.')
    lines.append(f'pub const {up}_TYPES: &[TypeEntry] = &[')
    for ks, name, fields in types(d):
        lines.append(f'    ({rs(ks)}, {rs(name)}, &[' + ', '.join(f'({rs(a)}, {rs(b)})' for a, b in fields) + ']),')
    lines.append('];')
    lines.append('')
    lines.append(f'/// Indexes on built-in tables: keyspace, table, index, `DESCRIBE` statement.')
    lines.append(f'pub const {up}_INDEXES: &[(&str, &str, &str, &str)] = &[')
    for ks, t, name, stmt in indexes(d):
        lines.append(f'    ({rs(ks)}, {rs(t)}, {rs(name)}, {rs(stmt)}),')
    lines.append('];')
    lines.append('')
open(dest, 'w').write('\n'.join(lines))
print({d: len(tables(d)) for d in cat}, {d: len(types(d)) for d in cat}, {d: len(indexes(d)) for d in cat})
