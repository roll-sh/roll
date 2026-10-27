# Renders the JSON captures in out/ into docs/nosql/reference/*.md.
import json, re, sys, os, collections
OUT = sys.argv[1]
CAP = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'out')
D = ['scylla', 'cassandra']
NAME = {'scylla': 'ScyllaDB 2026.3.3', 'cassandra': 'Cassandra 5.0.9'}
def load(kind, d):
    p = os.path.join(CAP, '%s-%s.json' % (d, kind))
    return json.load(open(p)) if os.path.exists(p) else None
SYSTEM_KS = {'system', 'system_schema', 'system_auth', 'system_distributed', 'system_distributed_everywhere', 'system_traces',
             'system_views', 'system_virtual_schema', 'system_replicated_keys', 'audit'}
UUID = re.compile(r'\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b')
IP = re.compile(r'\b172\.\d+\.\d+\.\d+\b')
HDR = ('<!-- Generated from our own observations of the reference images (see conformance.md for the '
       'capture method). Do not edit by hand. -->\n\n')

def esc(s):
    return str(s).replace('|', '\\|').replace('\n', '\\n')
def short(s, n=220):
    s = str(s)
    return s if len(s) <= n else s[:n] + '… (%d chars)' % len(s)
def fence(text, lang=''):
    return '```%s\n%s\n```\n' % (lang, text)
def outcome(r, n=400):
    if r is None: return '—'
    if not r.get('ok'):
        e = r.get('error', {})
        msg = re.sub(r'\b[0-9a-f]{12}/', '<hostname>/', IP.sub('<ip>', e.get('message', '')))
        s = '`%s` %s' % (e.get('code'), esc(short(msg, n)))
        if e.get('extra'): s += ' (extra: `%s`)' % esc(json.dumps(e['extra']))
        if e.get('extra_hex'): s += ' (extra bytes `%s`)' % esc(short(e['extra_hex'], 80))
        return s
    s = 'OK'
    if r.get('result_kind'): s += ' %s' % r['result_kind']
    if r.get('schema_change'): s += ' `%s`' % esc(json.dumps(r['schema_change']))
    if r.get('warnings'): s += '; warnings: ' + ' / '.join('"%s"' % esc(short(w, n)) for w in r['warnings'])
    return s
def write(name, text):
    with open(os.path.join(OUT, name), 'w') as f:
        f.write(text.rstrip('\n') + '\n')
    print('wrote', name, len(text.splitlines()), 'lines')

# ---------------------------------------------------------------- supported.md
def supported():
    t = [HDR, '# Protocol versions, OPTIONS/SUPPORTED and handshake errors\n\n',
         'Raw-socket probes (no driver): one frame per fresh connection unless noted. "stream" is the stream id of the '
         'response; the request used stream 1. "conn" is the connection state 1.5 s after the response '
         '(`open`, `closed`, `reset`).\n\n']
    P = {d: load('proto', d) for d in D}
    P2 = {d: load('proto2', d) for d in D}
    t.append('## SUPPORTED\n\n')
    for d in D:
        for k in ('OPTIONS v3', 'OPTIONS v4', 'OPTIONS v5', 'OPTIONS v6 USE_BETA'):
            r = P[d].get(k, {})
            if 'supported' in r:
                sup = dict(r['supported'])
                for key in ('SCYLLA_HOST_ID',):
                    if key in sup: sup[key] = ['<host id, volatile>']
                t.append('**%s, %s** (response version byte `%s`):\n\n' % (NAME[d], k, r['version_byte']))
                t.append(fence('\n'.join('%s: %s' % (kk, json.dumps(vv)) for kk, vv in sup.items())))
    t.append('Volatile: `SCYLLA_HOST_ID` is the node\'s host id. `SCYLLA_SHARD` is the shard that accepted the '
             'connection. Everything else is fixed for the image and flags.\n\n')
    t.append('## Accepted protocol versions\n\n| probe | ' + ' | '.join(NAME[d] for d in D) + ' |\n|---|---|---|\n')
    keys = list(P['scylla'].keys())
    for k in P['cassandra']:
        if k not in keys: keys.append(k)
    def cell(r):
        if r is None: return '—'
        if 'exception' in r: return 'no response: `%s`' % esc(r['exception'])
        if 'note' in r and 'opcode' not in r: return esc(r['note'])
        s = '%s, version byte `%s`, stream %s' % (r.get('opcode'), r.get('version_byte'), r.get('stream'))
        if r.get('error'): s += ', `%s` %s' % (r['error']['code'], esc(r['error']['message']))
        if r.get('header_hex') and r.get('opcode') == 'ERROR': s += ', header `%s`' % r['header_hex']
        if r.get('conn_after'): s += ', conn %s' % r['conn_after']
        return s
    for k in keys:
        t.append('| %s | %s | %s |\n' % (esc(k), cell(P['scylla'].get(k)), cell(P['cassandra'].get(k))))
    t.append('\nProtocol v1/v2 probes with alternative framings:\n\n| probe | ' + ' | '.join(NAME[d] for d in D) + ' |\n|---|---|---|\n')
    for k in P2['scylla']:
        t.append('| %s | %s | %s |\n' % (esc(k), cell(P2['scylla'].get(k)), cell(P2['cassandra'].get(k))))
    t.append('\nNotes:\n\n'
             '- ScyllaDB answers every unsupported version with a v4-framed ERROR (`0x84`, 9-byte header) on stream 0 '
             'and closes the connection. Cassandra answers in the client\'s framing (v1/v2 use the 8-byte header, '
             'version byte `0x81`/`0x82`; v6 without USE_BETA and v7+ get `0x85`/`0x86` frames), echoes the stream id '
             'when it could parse one, and keeps the connection open after a version error.\n'
             '- Cassandra 5.0 accepts v5 without the beta flag and v6 only with USE_BETA (`0x10`).\n'
             '- ScyllaDB ignores the CQL_VERSION value in STARTUP (missing, `2.0.0` and `abc` are all accepted); '
             'Cassandra validates it.\n')
    write('supported.md', ''.join(t))

# ---------------------------------------------------------------- system-local-peers.md
VOLATILE_LOCAL = {'broadcast_address', 'listen_address', 'rpc_address', 'gossip_generation', 'host_id',
                  'schema_version', 'tokens', 'truncated_at', 'peer', 'preferred_ip'}
def columns_of(cat, ks, table):
    r = cat['columns_all']
    cols = [row for row in r['rows'] if row[0] == ks and row[1] == table]
    if not cols and cat['virtual_columns'].get('ok'):
        vc = cat['virtual_columns']
        idx = {c: i for i, c in enumerate(vc['columns'])}
        cols = [[row[idx['keyspace_name']], row[idx['table_name']], row[idx['column_name']], row[idx['kind']],
                 row[idx['position']], row[idx['clustering_order']], row[idx['type']]]
                for row in vc['rows'] if row[idx['keyspace_name']] == ks and row[idx['table_name']] == table]
    order = {'partition_key': 0, 'clustering': 1, 'static': 2, 'regular': 3}
    return sorted(cols, key=lambda c: (order.get(c[3], 9), c[4] if c[4] is not None else 0, c[2]))
def coltable(cols):
    s = '| column | kind | position | clustering_order | type |\n|---|---|---|---|---|\n'
    for c in cols:
        s += '| %s | %s | %s | %s | %s |\n' % (c[2], c[3], c[4], c[5], c[6])
    return s
def mask_value(col, v):
    if col in VOLATILE_LOCAL and v is not None:
        if col == 'tokens':
            n = len(re.findall(r"'-?\d+'", str(v)))
            return '<%d tokens, volatile>' % n
        return '<volatile>'
    return v
def system_local():
    t = [HDR, '# system.local, system.peers, system.peers_v2\n\n',
         'Columns come from `system_schema.columns` (`system_virtual_schema.columns` for virtual tables). Values are '
         'from a fresh single node started with the reference flags; values marked `<volatile>` change per node or per '
         'boot (addresses are the container IP).\n\n']
    for d in D:
        cat = load('catalog', d)
        t.append('## %s\n\n' % NAME[d])
        for tb in ('local', 'peers', 'peers_v2'):
            cols = columns_of(cat, 'system', tb)
            sel = cat.get('select system.' + tb)
            if not cols and not (sel and sel['ok']):
                t.append('### system.%s\n\nNot present: `SELECT * FROM system.%s` → %s\n\n' % (tb, tb, outcome(sel or {'ok': False, 'error': {'code': None, 'message': 'not captured'}})))
                continue
            t.append('### system.%s\n\n' % tb)
            t.append(coltable(cols) + '\n')
            if sel and sel['ok']:
                t.append('`SELECT *` column order and driver types: %s\n\n' % ', '.join('`%s %s`' % (c, ty) for c, ty in zip(sel['columns'], sel['types'])))
                if sel['rows']:
                    t.append('Values:\n\n| column | value |\n|---|---|\n')
                    for c, v in zip(sel['columns'], sel['rows'][0]):
                        t.append('| %s | %s |\n' % (c, esc(short(mask_value(c, v), 300))))
                    t.append('\n')
                else:
                    t.append('No rows.\n\n')
        for extra in ('scylla_local', 'versions', 'protocol_servers', 'runtime_info', 'cluster_status', 'topology', 'load_per_node', 'clients'):
            sel = cat.get('select system.' + extra)
            if sel and sel['ok']:
                t.append('### system.%s\n\n' % extra)
                t.append(coltable(columns_of(cat, 'system', extra)) + '\n')
                t.append('Rows (addresses and uuids masked):\n\n')
                body = '\n'.join(' | '.join(UUID.sub('<uuid>', IP.sub('<ip>', short(str(v), 160))) for v in row) for row in sel['rows'][:30])
                t.append(fence(' | '.join(sel['columns']) + '\n' + body))
    write('system-local-peers.md', ''.join(t))

# ---------------------------------------------------------------- system-schema.md
def render_rows(r, mask_ids=True, limit=200):
    if r is None: return 'not captured\n'
    if not r.get('ok'): return 'error: %s\n' % outcome(r)
    if not r['rows']: return 'columns: %s — no rows\n' % ', '.join(r['columns'])
    lines = [' | '.join(r['columns'])]
    for row in r['rows'][:limit]:
        vals = []
        for c, v in zip(r['columns'], row):
            s = str(v)
            if mask_ids and c in ('id', 'version', 'table_id', 'schema_version') and v and '5a1c395e' not in s:
                s = '<uuid>'
            if c in ('salted_hash',) and v:
                s = '<salted hash>'
            vals.append(s)
        lines.append(' | '.join(vals))
    if len(r['rows']) > limit: lines.append('… %d more rows' % (len(r['rows']) - limit))
    return fence('\n'.join(lines))
def system_schema():
    t = [HDR, '# system_schema\n\n',
         'Part 1 lists every `system_schema` table with its columns. Part 2 shows the rows each dialect writes for '
         'the sample schema below (statements that a dialect rejects are listed with their error). Table ids and '
         'schema versions are masked as `<uuid>`. Rows are as the Python driver decodes them.\n\n']
    for d in D:
        cat = load('catalog', d)
        t.append('## Part 1 — tables and columns, %s\n\n' % NAME[d])
        tables = sorted(r[1] for r in cat['tables']['rows'] if r[0] == 'system_schema')
        for tb in tables:
            t.append('### system_schema.%s\n\n' % tb + coltable(columns_of(cat, 'system_schema', tb)) + '\n')
    t.append('## Part 2 — the sample schema\n\n')
    s_cat, c_cat = load('catalog', 'scylla'), load('catalog', 'cassandra')
    t.append('| step | statement | %s | %s |\n|---|---|---|---|\n' % (NAME['scylla'], NAME['cassandra']))
    for (n, q, rs), (_, _, rc) in zip(s_cat['schema_ddl'], c_cat['schema_ddl']):
        t.append('| %s | `%s` | %s | %s |\n' % (n, esc(short(' '.join(q.split()), 300)), outcome(rs), outcome(rc)))
    t.append('\nThe full `CREATE TABLE ref.t_all` statement:\n\n' + fence([q for n, q, _ in s_cat['schema_ddl'] if n == 't_all'][0], 'cql'))
    for d in D:
        cat = load('catalog', d)
        t.append('### Rows, %s\n\n' % NAME[d])
        for k in sorted(k for k in cat if k.startswith('ks ')):
            r = cat[k]
            if not r.get('ok') and 'unconfigured table' in r.get('error', {}).get('message', '') or (not r.get('ok') and 'does not exist' in r.get('error', {}).get('message', '')):
                t.append('#### `%s`\n\n%s\n\n' % (k[3:], outcome(r)))
                continue
            t.append('#### `%s`\n\n' % k[3:] + render_rows(r) + '\n')
        t.append('Note: `dropped_columns raw` re-reads `dropped_time` as `blobAsBigint(timestampAsBlob(dropped_time))`.\n\n')
    S2 = {d: load('catalog-suite', d) for d in D}
    if all(S2.values()):
        t.append('## Part 3 — the sample schema with features enabled\n\n'
                 'The same statements on a node started with the suites\' settings (see conformance.md): '
                 'PasswordAuthenticator, UDFs enabled (ScyllaDB `--enable-user-defined-functions 1 '
                 '--experimental-features=udf`; Cassandra `user_defined_functions_enabled`), materialized views and SASI '
                 'enabled on Cassandra. Only the rows that differ in kind from Part 2 are shown.\n\n')
        t.append('| step | %s | %s |\n|---|---|---|\n' % (NAME['scylla'], NAME['cassandra']))
        for (n, q, rs), (_, _, rc) in zip(S2['scylla']['schema_ddl'], S2['cassandra']['schema_ddl']):
            t.append('| %s | %s | %s |\n' % (n, outcome(rs), outcome(rc)))
        t.append('\n')
        for d in D:
            t.append('### Rows with features enabled, %s\n\n' % NAME[d])
            for k in ('ks ref functions', 'ks ref aggregates', 'ks ref views', 'ks ref indexes', 'ks ref view_virtual_columns', 'ks ref column_masks', 'ks ref_vnodes views'):
                r = S2[d].get(k)
                if r is None or (not r.get('ok') and 'unconfigured table' in r.get('error', {}).get('message', '')):
                    continue
                t.append('#### `%s`\n\n' % k[3:] + render_rows(r) + '\n')
    write('system-schema.md', ''.join(t))

# ---------------------------------------------------------------- describe.md
def describe():
    t = [HDR, '# Server-side DESCRIBE\n\n',
         'Raw result rows of server-side `DESCRIBE` statements sent through the Python driver (protocol v4 on '
         'ScyllaDB, v5 on Cassandra) for the sample schema in [system-schema.md](system-schema.md). Each row is shown '
         'as `keyspace_name | type | name` followed by `create_statement` verbatim in a fenced block (whitespace is '
         'exact). Statements over system keyspaces are in [describe-system.md](describe-system.md).\n\n']
    sysq = {'DESCRIBE FULL SCHEMA', 'DESCRIBE KEYSPACE system', 'DESC KEYSPACE system_auth', 'DESCRIBE TABLE system.local',
            'DESCRIBE TABLE system.peers', 'DESCRIBE TABLE system_views.settings', 'DESC TABLES'}
    ts = [HDR, '# Server-side DESCRIBE over system keyspaces\n\n', 'Same format as [describe.md](describe.md).\n\n']
    cats = {d: dict(load('catalog', d)['describe']) for d in D}
    seen = {}
    for q in [x[0] for x in load('catalog', 'scylla')['describe']]:
        target = ts if q in sysq else t
        target.append('## `%s`\n\n' % q)
        for d in D:
            r = cats[d].get(q)
            target.append('### %s\n\n' % NAME[d])
            if r is None: target.append('not captured\n\n'); continue
            if not r['ok']: target.append(outcome(r) + '\n\n'); continue
            target.append('Result columns: %s\n\n' % ', '.join('`%s %s`' % (c, ty) for c, ty in zip(r['columns'], r['types'])))
            if 'create_statement' not in r['columns']:
                target.append(fence('\n'.join(' | '.join(str(v) for v in row) for row in r['rows'])))
                continue
            for row in r['rows']:
                key = (d, row[3])
                if key in seen:
                    target.append('`%s | %s | %s` — create_statement identical to the one under `%s`\n\n' % (tuple(row[:3]) + (seen[key],)))
                else:
                    seen[key] = q
                    target.append('`%s | %s | %s`\n\n' % tuple(row[:3]) + fence(row[3], 'cql'))
    S2 = {d: load('catalog-suite', d) for d in D}
    if all(S2.values()):
        t.append('# Part 2 — with UDFs, MVs and auth enabled\n\n'
                 'The same sample schema on a node started with the suites\' settings (PasswordAuthenticator, UDFs, '
                 'materialized views). Only the statements whose output depends on those features are repeated.\n\n')
        c2 = {d: dict(S2[d]['describe']) for d in D}
        for q in [x[0] for x in S2['scylla']['describe']]:
            if not any(w in q for w in ('FUNCTION', 'AGGREGATE', 'MATERIALIZED', 'KEYSPACE ref', 'PASSWORDS', 'DESCRIBE SCHEMA')):
                continue
            t.append('## `%s` (features enabled)\n\n' % q)
            for d in D:
                r = c2[d].get(q)
                t.append('### %s\n\n' % NAME[d])
                if r is None: t.append('not captured\n\n'); continue
                if not r['ok']: t.append(outcome(r) + '\n\n'); continue
                if 'create_statement' not in r['columns']:
                    t.append(fence('\n'.join(' | '.join(str(v) for v in row) for row in r['rows']))); continue
                for row in r['rows']:
                    stmt = re.sub(r"(SALTED HASH|salted_hash|HASHED PASSWORD) = '[^']*'", r"\1 = '<salted hash>'", row[3])
                    key = (d, row[3])
                    if key in seen:
                        t.append('`%s | %s | %s` — create_statement identical to the one under `%s`\n\n' % (tuple(row[:3]) + (seen[key],)))
                    else:
                        seen[key] = q + ' (features enabled)'
                        t.append('`%s | %s | %s`\n\n' % tuple(row[:3]) + fence(stmt, 'cql'))
    write('describe.md', ''.join(t))
    write('describe-system.md', ''.join(ts))

# ---------------------------------------------------------------- default-options.md
CONFIG_RE = re.compile(r'threshold|_enabled|^enable_|authenticator|authorizer|role_manager|num_tokens|cluster_name|partitioner|'
                       r'tablet|batch_size|tombstone|timeout|guardrail|warn|fail|strict|allow_|cdc|materialized|user_defined|udf|sasi|'
                       r'secondary_index|compact_storage|replication|snitch|native_transport|commitlog_sync|data_masking|transient|'
                       r'storage_compatibility|cql|alternator|consistency|page|ttl|default_|max_|minimum_|in_select|collection|'
                       r'experimental|dc$|rack|listen_address|rpc_address|broadcast|seeds?$|superuser|permissions|roles_|credentials|'
                       r'uuid_sstable|sstable_format|auto_snapshot|reject|limit|vector|sai|traditional|_scale_factor|schema_|query', re.I)
def default_options():
    t = [HDR, '# Default options and settings\n\n']
    t.append('## Table defaults as `DESCRIBE` prints them\n\n`CREATE TABLE ref.t_simple (id int PRIMARY KEY, v text)` '
             'then `ALTER TABLE ref.t_simple WITH comment = \'altered\' AND gc_grace_seconds = 7200` '
             '(plus an index, a materialized view and dropped columns where accepted):\n\n')
    for d in D:
        r = dict(load('catalog', d)['describe']).get('DESCRIBE TABLE ref.t_counter')
        t.append('### %s, `DESCRIBE TABLE ref.t_counter` (no options given)\n\n' % NAME[d])
        if r and r['ok']: t.append(fence(r['rows'][0][3], 'cql'))
    t.append('## Table defaults as `system_schema.tables` stores them\n\n')
    for d in D:
        cat = load('catalog', d)
        r = cat['ks ref tables']
        t.append('### %s, `ref.t_counter`\n\n' % NAME[d])
        if r['ok']:
            idx = r['columns'].index('table_name')
            row = [x for x in r['rows'] if x[idx] == 't_counter']
            if row: t.append('| column | value |\n|---|---|\n' + ''.join('| %s | %s |\n' % (c, esc('<uuid>' if c == 'id' else v)) for c, v in zip(r['columns'], row[0])) + '\n')
        r = cat['ks ref scylla_tables']
        if r.get('ok') and r['rows']:
            idx = r['columns'].index('table_name')
            row = [x for x in r['rows'] if x[idx] == 't_counter']
            if row: t.append('`system_schema.scylla_tables` for `ref.t_counter`:\n\n| column | value |\n|---|---|\n' + ''.join('| %s | %s |\n' % (c, esc('<uuid>' if c == 'version' else v)) for c, v in zip(r['columns'], row[0])) + '\n')
    t.append('## Keyspace defaults\n\n')
    for d in D:
        cat = load('catalog', d)
        t.append('### %s\n\n' % NAME[d])
        for k in ('ks ref keyspaces', 'ks ref scylla_keyspaces', 'ks ref_simple keyspaces', 'ks ref_vnodes keyspaces'):
            r = cat.get(k)
            if r and r.get('ok') and r['rows']:
                t.append('`%s`:\n\n' % k[3:] + render_rows(r) + '\n')
        r = dict(cat['describe']).get('DESCRIBE KEYSPACES')
        ks = cat['keyspaces']
        t.append('System keyspaces on a fresh node (`system_schema.keyspaces`):\n\n' + render_rows({'ok': True, 'columns': ks['columns'], 'rows': [x for x in ks['rows'] if x[0] in SYSTEM_KS]}) + '\n')
    t.append('## Server settings relevant to clients\n\n'
             'Filtered from `system.config` (ScyllaDB) and `system_views.settings` (Cassandra) by name (thresholds, '
             'feature switches, auth, tokens, names, partitioner, tablets, timeouts, guardrails). Values are as the '
             'reference images report them with the flags in [conformance.md](conformance.md) (default image config).\n\n')
    for d, key in (('scylla', 'select system.config'), ('cassandra', 'select system_views.settings')):
        r = load('catalog', d)[key]
        t.append('### %s (`%s`, %d rows in total)\n\n' % (NAME[d], key[7:], len(r['rows'])))
        cols = r['columns']
        lines = [' | '.join(cols)]
        for row in sorted(r['rows'], key=lambda x: x[0]):
            if CONFIG_RE.search(row[0]):
                lines.append(' | '.join(IP.sub('<ip>', short(str(v), 200)) for v in row))
        t.append(fence('\n'.join(lines)))
    r = load('catalog', 'cassandra').get('select system_views.system_properties')
    if r and r['ok']:
        t.append('### Cassandra `system_views.system_properties`\n\n' + render_rows(r, mask_ids=False) + '\n')
    write('default-options.md', ''.join(t))

# ---------------------------------------------------------------- system-tables.md
COMMON = ['system.size_estimates', 'system.table_estimates', 'system.clients', 'system.config', 'system.tablets',
          'system.scylla_local', 'system.versions', 'system.roles', 'system.role_members', 'system.role_permissions',
          'system.role_attributes', 'system.service_levels_v2', 'system.topology', 'system.token_ring', 'system.runtime_info',
          'system.protocol_servers', 'system.cluster_status', 'system.load_per_node', 'system.snapshots', 'system.large_partitions',
          'system.large_rows', 'system.large_cells', 'system.built_views', 'system.view_build_status_v2', 'system.cdc_local',
          'system.prepared_statements', 'system.top_partitions', 'system.paxos', 'system.compaction_history',
          'system_auth.roles', 'system_auth.role_members', 'system_auth.role_permissions', 'system_auth.network_permissions',
          'system_auth.cidr_groups', 'system_auth.cidr_permissions', 'system_auth.identity_to_role', 'system_auth.resource_role_permissons_index',
          'system_traces.sessions', 'system_traces.events', 'system_traces.node_slow_log', 'system_traces.sessions_time_idx',
          'system_distributed.view_build_status', 'system_distributed.cdc_streams_descriptions_v2',
          'system_distributed.cdc_generation_timestamps', 'system_distributed.repair_history', 'system_distributed.parent_repair_history',
          'system_distributed.partition_denylist', 'system_distributed.service_levels']
def system_tables():
    t = [HDR, '# Keyspaces and tables on a fresh node\n\n',
         'Every system keyspace and table (including virtual ones) present on a fresh node. Other agents\' user '
         'keyspaces are filtered out. Columns for the tables drivers and tools commonly read follow; rows are '
         'summarised (count, first rows with addresses/uuids masked) because they are volatile.\n\n']
    for d in D:
        cat = load('catalog', d)
        t.append('## %s\n\n' % NAME[d])
        by = collections.defaultdict(list)
        for ks, tb in cat['tables']['rows']:
            if ks in SYSTEM_KS: by[ks].append(tb)
        virt = {}
        if cat['virtual_tables'].get('ok'):
            for row in cat['virtual_tables']['rows']:
                by[row[0]].append(row[1]); virt[(row[0], row[1])] = row[2]
        for ks in sorted(by):
            t.append('- `%s`%s: %s\n' % (ks, ' (virtual)' if any(k[0] == ks for k in virt) else '', ', '.join('`%s`' % x for x in sorted(by[ks]))))
        t.append('\n')
        if d == 'scylla':
            r = cat['system_schema_tables_rows']
            if r.get('ok'):
                ci, fi = r['columns'].index('table_name'), (r['columns'].index('flags') if 'flags' in r['columns'] else None)
        for name in COMMON:
            ks, tb = name.split('.')
            if ks not in by or tb not in by[ks]: continue
            cols = columns_of(cat, ks, tb)
            sel = cat.get('select ' + name)
            t.append('### %s\n\n' % name)
            if virt.get((ks, tb)) is not None: t.append('Virtual table. Comment: "%s"\n\n' % virt[(ks, tb)])
            t.append(coltable(cols) + '\n')
            if sel and sel.get('ok'):
                t.append('%d rows on a fresh node%s\n\n' % (len(sel['rows']), ' (first 5, masked):' if sel['rows'] else '.'))
                if sel['rows']:
                    body = '\n'.join(' | '.join('<salted hash>' if c == 'salted_hash' and v else UUID.sub('<uuid>', IP.sub('<ip>', short(str(v), 120))) for c, v in zip(sel['columns'], row)) for row in sel['rows'][:5])
                    t.append(fence(' | '.join(sel['columns']) + '\n' + body))
            elif sel:
                t.append('`SELECT *` → %s\n\n' % outcome(sel))
        if cat['virtual_tables'].get('ok'):
            t.append('### All virtual tables (columns)\n\n')
            for (ks, tb), comment in sorted(virt.items()):
                cols = columns_of(cat, ks, tb)
                t.append('- `%s.%s` — %s: %s\n' % (ks, tb, comment, ', '.join('%s %s%s' % (c[2], c[6], ' (%s)' % c[3] if c[3] != 'regular' else '') for c in cols)))
            t.append('\n')
        else:
            t.append('### Virtual tables\n\n`system_virtual_schema` does not exist (`%s`). ScyllaDB lists its virtual tables in '
                     '`system_schema.tables` like any other table.\n\n' % outcome(cat['virtual_tables']))
        t.append('### All other system tables (columns)\n\n')
        for ks in sorted(by):
            for tb in sorted(by[ks]):
                if '%s.%s' % (ks, tb) in COMMON or (ks, tb) in virt or ks == 'system_schema': continue
                cols = columns_of(cat, ks, tb)
                t.append('- `%s.%s`: %s\n' % (ks, tb, ', '.join('%s %s%s' % (c[2], c[6], ' (%s)' % c[3] if c[3] != 'regular' else '') for c in cols)))
        t.append('\n')
    write('system-tables.md', ''.join(t))

# ---------------------------------------------------------------- errors.md
CAT_TITLES = collections.OrderedDict([
    ('select', 'SELECT restrictions and selectors'), ('write', 'INSERT / UPDATE / DELETE and literals'),
    ('counter', 'Counters'), ('lwt', 'Lightweight transactions'), ('batch', 'Batches'), ('ddl', 'DDL'),
    ('views-indexes', 'Materialized views, secondary indexes, SAI and vector indexes'), ('syntax', 'Syntax errors'),
    ('warning', 'Warnings and guardrails'), ('consistency', 'Consistency levels and replication'), ('function', 'Functions'),
    ('rate-limit-timeout', 'Rate limits and USING TIMEOUT'), ('prepared', 'Prepared statements, bound nulls and UNSET'),
    ('auth-off', 'Auth statements with the default AllowAllAuthenticator'), ('auth', 'Auth with PasswordAuthenticator'),
    ('destructive', 'Writes to system keyspaces (observed once; not re-probed)')])
def errors():
    E = {d: load('errors', d) for d in D}
    A = {d: load('errors-auth', d) for d in D}
    X = {d: load('destructive', d) for d in D}
    t = [HDR, '# Errors and warnings\n\n',
         'Every statement was sent as a QUERY over a raw protocol v4 connection (consistency ONE, page size 5000 '
         'unless noted) to the default-configured reference, so codes, messages, extra error fields and warnings are '
         'exactly what the server sent. The prepared-statement section uses the Python driver. Results appear in '
         'execution order: earlier statements in a section can change later ones (for example ALTERs). '
         'Long statements are abbreviated as `… (N chars)`.\n\n'
         'Error codes: `0x0000` server, `0x000A` protocol, `0x0100` bad credentials, `0x1000` unavailable, '
         '`0x1100` write timeout, `0x1200` read timeout, `0x1300` read failure, `0x1400` function failure, '
         '`0x2000` syntax, `0x2100` unauthorized, `0x2200` invalid, `0x2300` config, `0x2400` already exists, '
         '`0x2500` unprepared.\n\n'
         'Setup (both dialects): keyspace `e` (NetworkTopologyStrategy datacenter1: 1; tablets on in ScyllaDB), '
         '`e.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<int>, m map<int, int>, fl frozen<list<int>>, '
         'fm frozen<map<int, int>>, tu tuple<int, text>, PRIMARY KEY (pk, ck))`, `e.t2 (a, b, c, d, v int, PRIMARY KEY ((a, b), c, d))`, '
         '`e.c (pk int PRIMARY KEY, cnt counter)`, `e.u` (one column of every native type), `e.ix` with index `ix_v` on `v`, '
         '`e.uses (pk int PRIMARY KEY, t frozen<ty>, nt ty)` with `TYPE e.ty (x int, y text)`, `e.vec (pk int PRIMARY KEY, emb vector<float, 3>)`, '
         '`e.blobk (pk blob PRIMARY KEY)`, `e.textk (pk text, ck text, …)`, `e.big`/`e.tomb (pk int, ck int, v, PRIMARY KEY (pk, ck))`; '
         'one row `(1, 1, v=1, txt=\'a\', l=[1, 2])` in `e.t`.\n\n']
    order = collections.OrderedDict()
    for d in D:
        for cid, cat, q, r in (E[d] or []):
            if cid in ('setup', 'setup-failure'): continue
            order.setdefault((cat, cid), {'q': q})[d] = r
        for cid, cat, q, r in (A[d] or []):
            if cid in ('setup', 'setup-failure'): continue
            order.setdefault(('auth', cid), {'q': q})[d] = r
        for cid, cat, q, r in (X[d] or []):
            order.setdefault(('destructive', cid), {'q': q})[d] = r
    n = 0
    for cat, title in CAT_TITLES.items():
        items = [(cid, v) for (c, cid), v in order.items() if c == cat]
        if not items: continue
        t.append('## %s\n\n' % title)
        if cat == 'destructive':
            t.append('These statements were accepted by both references in an early capture run and damaged the node '
                     '(drivers could no longer connect; ScyllaDB\'s Raft group 0 stopped). They are recorded here as observed '
                     'and are no longer probed. Do not run them against shared references.\n\n')
        if cat == 'auth':
            t.append('Captured on references started from the default image with only `PasswordAuthenticator` + '
                     '`CassandraAuthorizer` added (superuser `cassandra`/`cassandra`). `refs_alice` is a non-superuser '
                     'login role, `refs_admin` a second superuser, `refs_nologin` has `LOGIN = false`; `refs_ea.t` is a '
                     'table. Login cases use raw SASL PLAIN; statements are sent on raw connections logged in as the '
                     'role named.\n\n')
        t.append('| id | statement | %s | %s |\n|---|---|---|---|\n' % (NAME['scylla'], NAME['cassandra']))
        for cid, v in items:
            a, b = v.get('scylla'), v.get('cassandra')
            sa, sb = outcome(a), outcome(b)
            if sa == sb and a is not None: sb = '(same)'
            t.append('| %s | `%s` | %s | %s |\n' % (cid, esc(short(v['q'], 200)), sa, sb))
            n += 1
        t.append('\n')
    t.insert(2, '%d cases.\n\n' % n)
    write('errors.md', ''.join(t))

# ---------------------------------------------------------------- statements.md
def fmt_result(r):
    if r is None: return 'not captured'
    if not r.get('ok'):
        s = 'error ' + outcome(r)
        if r.get('raw'): s += '\nraw re-run: ' + outcome(r['raw'])
        return s
    lines = []
    if r.get('result_kind'):
        lines.append(outcome(r))
    if 'bind' in r:
        lines.append('bind: ' + ', '.join('%s %s' % (b[2], b[3]) for b in r['bind']))
        lines.append('result: ' + ', '.join('%s %s' % (b[2], b[3]) for b in r['result']))
        lines.append('routing_key_indexes: %s' % r.get('routing_key_indexes'))
        return '\n'.join(lines)
    if 'columns' in r:
        lines.append('columns: ' + ', '.join('%s %s' % (c, ty) for c, ty in zip(r['columns'], r['types'])))
        for row in r.get('rows', [])[:8]:
            lines.append('  ' + ' | '.join(short(str(v), 200) for v in row))
        if len(r.get('rows', [])) > 8: lines.append('  … %d rows' % len(r['rows']))
    if 'paging_state_present' in r: lines.append('has_more_pages=%s paging_state=%s' % (r['has_more_pages'], r['paging_state_present']))
    if r.get('warnings'): lines.append('warnings: %s' % r['warnings'])
    return '\n'.join(lines)
VOL = re.compile(r'(currenttimeuuid|now|uuid|currenttimestamp|currentdate|currenttime)\(\)')
def statements():
    S = {d: load('statements', d) for d in D}
    t = [HDR, '# Statement results\n\n',
         'Result metadata (column names and types as the driver reports them), rows, paging, LWT results, schema '
         'change results and prepared-statement metadata for each statement family. Results use the Python driver '
         'over protocol v4 on both dialects (schema-change and USE results come from the raw client). Statements run in '
         'order against keyspace `s`; writes use explicit timestamps where the value matters. When both dialects '
         'return the same thing it is shown once. Column types are as the Python driver renders the result metadata, '
         'which does not show frozen-ness reliably (a frozen list prints as `list<int>`, a non-frozen UDT as '
         '`frozen<addr>`); system-schema.md has the authoritative column types. Rows are Python reprs.\n\n'
         'Data: `s.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<text>, m map<text, int>, '
         'fl frozen<list<int>>, tu tuple<int, text>, ad addr, PRIMARY KEY (pk, ck))` with rows (1,1,10,s=100,\'one\',…), '
         '(1,2,20,\'two\'), (1,3,30,\'three\') at timestamp 1000, (2,1,5) at 2000 and a static-only partition 3; '
         '`s.t2 (a, b, c, d, v, PRIMARY KEY ((a, b), c, d)) WITH CLUSTERING ORDER BY (c DESC, d ASC)`; `s.ty` with one '
         'column per native type; `s.c` counters.\n\n']
    fam_titles = collections.OrderedDict([('schema-change', 'Schema-change, USE and TRUNCATE results'), ('select', 'SELECT'),
                                          ('write', 'Writes and read-back'), ('lwt', 'Lightweight transactions'),
                                          ('prepared', 'Prepared statement metadata'), ('raw', 'Tracing'), ('setup', 'Setup failures')])
    rows = collections.OrderedDict()
    for d in D:
        for cid, fam, q, r in S[d]:
            key = (fam, cid if cid != 'setup-failure' else cid + ':' + q)
            rows.setdefault(key, {'q': q})[d] = r
    for fam, title in fam_titles.items():
        items = [(cid, v) for (f, cid), v in rows.items() if f == fam]
        if not items: continue
        t.append('## %s\n\n' % title)
        for cid, v in items:
            t.append('### %s\n\n' % cid.split(':')[0] + fence(short(v['q'], 600), 'cql'))
            a, b = fmt_result(v.get('scylla')), fmt_result(v.get('cassandra'))
            if a == b:
                t.append('Both:\n\n' + fence(a))
            else:
                t.append('%s:\n\n' % NAME['scylla'] + fence(a) + '%s:\n\n' % NAME['cassandra'] + fence(b))
    t.append('Volatile values: results of `now()`, `uuid()`, `currentTimestamp()` and friends, `ttl()` of live '
             'cells (counts down), and tracing session ids.\n')
    write('statements.md', ''.join(t))

for f in (supported, system_local, system_schema, describe, default_options, system_tables, errors, statements):
    f()
