# Dumps system tables, system_schema, DESCRIBE and settings for one dialect into JSON.
import sys, json, os, time
sys.path.insert(0, '/cap')
from common import connect, run as _run
from sample import SCHEMA
host = os.environ.get('CAP_HOST', '127.0.0.1'); out = sys.argv[1]
user = sys.argv[2] if len(sys.argv) > 2 else None
pw = sys.argv[3] if len(sys.argv) > 3 else None
c, s = connect(host, user, pw)
def run(_s, q, keep_rows=True):
    global c, s
    for attempt in range(4):
        r = _run(s, q, keep_rows=keep_rows)
        if r['ok'] or r['error']['class'] not in ('ConnectionException', 'NoHostAvailable', 'OperationTimedOut', 'ConnectionShutdown') or 'Error from server' in r['error']['message']:
            return r
        time.sleep(3)
        try: c.shutdown()
        except Exception: pass
        c, s = connect(host, user, pw)
    return r
R = {'protocol_version': c.protocol_version}
R['keyspaces'] = run(s, "SELECT * FROM system_schema.keyspaces")
R['tables'] = run(s, "SELECT keyspace_name, table_name FROM system_schema.tables")
R['virtual_keyspaces'] = run(s, "SELECT * FROM system_virtual_schema.keyspaces")
R['virtual_tables'] = run(s, "SELECT * FROM system_virtual_schema.tables")
R['virtual_columns'] = run(s, "SELECT * FROM system_virtual_schema.columns")
R['columns_all'] = run(s, "SELECT keyspace_name, table_name, column_name, kind, position, clustering_order, type FROM system_schema.columns")
R['system_schema_tables_rows'] = run(s, "SELECT * FROM system_schema.tables WHERE keyspace_name IN ('system_schema', 'system', 'system_auth', 'system_distributed', 'system_traces', 'system_distributed_everywhere', 'system_replicated_keys', 'audit')")
names = set()
for src in ('tables', 'virtual_tables'):
    if R[src]['ok']:
        for row in R[src]['rows']:
            names.add('%s.%s' % (row[0], row[1]))
SKIP_ROWS = set()
for t in sorted(names):
    if t.split('.')[0] in ('ref', 'ref_simple', 'ref_vnodes', 'e', 'ea'): continue
    R['select ' + t] = run(s, 'SELECT * FROM %s LIMIT 1000' % '.'.join('"%s"' % x for x in t.split('.')))
for ks in ['ref', 'ref_simple', 'ref_vnodes']: run(s, 'DROP KEYSPACE IF EXISTS ' + ks)
R['schema_ddl'] = []
for name, q in SCHEMA:
    R['schema_ddl'].append([name, q, run(s, q)])
time.sleep(2)
for ks in ['ref', 'ref_simple', 'ref_vnodes']:
    for t in ['keyspaces', 'tables', 'columns', 'dropped_columns', 'triggers', 'views', 'types', 'functions',
              'aggregates', 'indexes', 'scylla_tables', 'view_virtual_columns', 'computed_columns', 'scylla_keyspaces', 'column_masks']:
        R['ks %s %s' % (ks, t)] = run(s, "SELECT * FROM system_schema.%s WHERE keyspace_name = '%s'" % (t, ks))
    R['ks %s dropped_columns raw' % ks] = run(s, "SELECT keyspace_name, table_name, column_name, blobAsBigint(timestampAsBlob(dropped_time)) AS dropped_time_raw, type FROM system_schema.dropped_columns WHERE keyspace_name = '%s'" % ks)
D = ["DESCRIBE KEYSPACE ref", "DESCRIBE KEYSPACE ref_simple", "DESCRIBE KEYSPACE ref_vnodes",
     "DESCRIBE TABLE ref.t_all", "DESCRIBE TABLE ref.t_simple", "DESCRIBE TABLE ref.t_counter", "DESCRIBE TABLE ref.t_compact",
     "DESCRIBE TABLE ref.t_compact2", "DESCRIBE TABLE ref.t_opts", "DESCRIBE TABLE ref.t_vec", "DESCRIBE TABLE ref.t_cdc",
     "DESCRIBE TABLE ref.\"QuotedTable\"", "DESCRIBE TABLE ref.t_mask", "DESCRIBE TABLE ref.t_twcs", "DESCRIBE TABLE ref.t_comp1", "DESCRIBE TABLE ref.t_comp2", "DESCRIBE TABLE ref.t_comp3",
     "DESCRIBE SCHEMA", "DESCRIBE FULL SCHEMA", "DESCRIBE CLUSTER", "DESCRIBE KEYSPACES", "DESC TABLES",
     "DESC TYPES", "DESC TYPE ref.address", "DESC TYPE ref.person", "DESC FUNCTIONS", "DESC AGGREGATES", "DESC INDEX ref.t_all_txt_idx", "DESC INDEX ref.t_all_local_idx",
     "DESC MATERIALIZED VIEW ref.t_simple_by_v", "DESC MATERIALIZED VIEW ref.t_all_by_txt", "DESC MATERIALIZED VIEW ref_vnodes.base_by_v",
     "DESCRIBE TABLE ref.t_all WITH INTERNALS", "DESCRIBE KEYSPACE ref WITH INTERNALS", "DESCRIBE SCHEMA WITH INTERNALS",
     "DESC ONLY KEYSPACE ref", "DESC TABLE ref.nope", "DESC KEYSPACE nope", "DESC TYPE ref.nope", "DESC INDEX ref.nope", "DESC MATERIALIZED VIEW ref.nope",
     "DESC ref.t_simple", "DESC ref", "DESC nope", "DESC TABLE t_simple",
     "DESCRIBE TABLE system.local", "DESCRIBE TABLE system.peers", "DESC KEYSPACE system_auth", "DESCRIBE KEYSPACE system", "DESCRIBE TABLE system_views.settings",
     "DESC FUNCTION ref.acc", "DESC AGGREGATE ref.sum1", "DESC FUNCTION ref.acc_lua", "DESC AGGREGATE ref.sum1_lua", "DESC FUNCTION ref.plus1_lua", "DESC FUNCTION ref.plus1", "DESC FUNCTION system.now", "DESC FUNCTION ref.nope",
     "DESCRIBE TABLE ref.t_all WITH INTERNALS AND PASSWORDS", "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS", "DESCRIBE SCHEMA WITH PASSWORDS"]
R['describe'] = [[q, run(s, q)] for q in D]
# final cleanup
for ks in ['ref', 'ref_simple', 'ref_vnodes']: run(s, 'DROP KEYSPACE IF EXISTS ' + ks)
json.dump(R, open(out, 'w'), indent=1, default=str)
print('ok', c.protocol_version)
