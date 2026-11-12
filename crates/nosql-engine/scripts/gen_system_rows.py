# Generates crates/nosql-engine/src/system/rows_data.rs from the reference catalog captures: the
# fixed rows of the configuration tables (Cassandra's system_views.settings,
# system_views.system_properties and thread pools; ScyllaDB's system.config and
# system.runtime_info items). Values the database derives from its own configuration are
# overridden at read time (see src/system/views.rs).
#
#   python3 -I gen_system_rows.py ~/.cache/roll-nosql/capture/out ../src/system/rows_data.rs
import json, sys

outdir, dest = sys.argv[1], sys.argv[2]
s = json.load(open(f'{outdir}/scylla-catalog.json'))
c = json.load(open(f'{outdir}/cassandra-catalog.json'))


def rs(v):
    return 'r###"' + v + '"###'


def opt(v):
    return 'None' if v is None else f'Some({rs(v)})'


out = ['//! Generated from the reference catalog captures by `scripts/gen_system_rows.py`; do not',
       '//! edit by hand.', '']
rows = sorted(c['select system_views.settings']['rows'])
out.append('/// Cassandra 5.0.9 `system_views.settings` (name, value) on the reference image.')
out.append('pub const CASSANDRA_SETTINGS: &[(&str, Option<&str>)] = &[')
out.extend(f'    ({rs(n)}, {opt(v)}),' for n, v in rows)
out.append('];')
out.append('')
rows = sorted(c['select system_views.system_properties']['rows'])
out.append('/// Cassandra `system_views.system_properties`.')
out.append('pub const CASSANDRA_PROPERTIES: &[(&str, &str)] = &[')
out.extend(f'    ({rs(n)}, {rs(v)}),' for n, v in rows if n != 'java.class.path')
out.append('];')
out.append('')
rows = sorted(c['select system_views.thread_pools']['rows'])
out.append('/// Cassandra `system_views.thread_pools`: name and `active_tasks_limit`.')
out.append('pub const CASSANDRA_THREAD_POOLS: &[(&str, i32)] = &[')
out.extend(f'    ({rs(r[0])}, {r[2]}),' for r in rows)
out.append('];')
out.append('')
rows = sorted(s['select system.config']['rows'])
out.append('/// ScyllaDB 2026.3.3 `system.config` (name, source, type, value) on the reference image.')
out.append('pub const SCYLLA_CONFIG: &[(&str, &str, &str, &str)] = &[')
out.extend(f'    ({rs(n)}, {rs(src)}, {rs(t)}, {rs(v)}),' for n, src, t, v in rows)
out.append('];')
out.append('')
open(dest, 'w').write('\n'.join(out))
print('ok')
