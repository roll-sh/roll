# Result shapes: column names/types, rows, warnings, paging, LWT results, schema-change results,
# prepared-statement metadata, for every statement family. Writes JSON list [id, family, stmt, result].
import sys, json, os, time
sys.path.insert(0, '/cap')
from common import connect, run as drun, err, typename
from raw import Raw
from cassandra.query import SimpleStatement, BatchStatement, BatchType
host = os.environ.get('CAP_HOST', '127.0.0.1'); out = sys.argv[1]
c, s = connect(host, proto=4)
R = Raw(host)
RES = []
def reconnect():
    global c, s
    try: c.shutdown()
    except Exception: pass
    c, s = connect(host, proto=4)
def q(cid, fam, stmt, fetch_size=None, **kw):
    st = SimpleStatement(stmt, fetch_size=fetch_size)
    r = drun(s, st)
    if not r['ok'] and r['error']['class'] in ('DriverException', 'ConnectionShutdown', 'ConnectionException', 'NoHostAvailable'):
        reconnect()
        if 'Failed decoding' in r['error']['message']:
            r['raw'] = R.query(stmt)
        else:
            r = drun(s, st)
    if r['ok'] and fetch_size:
        rs = s.execute(st)
        r['has_more_pages'] = rs.has_more_pages
        r['paging_state_present'] = rs.paging_state is not None
    RES.append([cid, fam, stmt, r])
def raw(cid, fam, stmt, **kw):
    RES.append([cid, fam, stmt, R.query(stmt, **kw)])
def setup(*stmts):
    for x in stmts:
        r = R.query(x)
        if not r['ok']: RES.append(['setup-failure', 'setup', x, r])

setup("DROP KEYSPACE IF EXISTS s",
      "CREATE KEYSPACE s WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
# schema-change result bodies
DDL = [
 ("ddl_create_type", "CREATE TYPE s.addr (street text, zip int)"),
 ("ddl_create_table", "CREATE TABLE s.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, tu tuple<int, text>, ad addr, PRIMARY KEY (pk, ck))"),
 ("ddl_create_table_if_not_exists_existing", "CREATE TABLE IF NOT EXISTS s.t (pk int PRIMARY KEY)"),
 ("ddl_create_counter", "CREATE TABLE s.c (pk int, ck int, n counter, PRIMARY KEY (pk, ck))"),
 ("ddl_create_types", "CREATE TABLE s.ty (pk int PRIMARY KEY, a ascii, bi bigint, b blob, bo boolean, d date, de decimal, db double, du duration, f float, i inet, n int, sm smallint, tx text, tm time, ts timestamp, tid timeuuid, ti tinyint, u uuid, vc varchar, vi varint, vec vector<float, 3>)"),
 ("ddl_create_t2", "CREATE TABLE s.t2 (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d)) WITH CLUSTERING ORDER BY (c DESC, d ASC)"),
 ("ddl_create_index", "CREATE INDEX t_v_idx ON s.t (v)"),
 ("ddl_alter_table_add", "ALTER TABLE s.t ADD extra text"),
 ("ddl_alter_table_with", "ALTER TABLE s.t WITH comment = 'x'"),
 ("ddl_alter_type_add", "ALTER TYPE s.addr ADD city text"),
 ("ddl_alter_type_rename", "ALTER TYPE s.addr RENAME city TO town"),
 ("ddl_alter_keyspace", "ALTER KEYSPACE s WITH durable_writes = true"),
 ("ddl_create_drop_table", "CREATE TABLE s.tmp (pk int PRIMARY KEY)"),
 ("ddl_drop_table", "DROP TABLE s.tmp"),
 ("ddl_drop_table_if_exists_missing", "DROP TABLE IF EXISTS s.tmp"),
 ("ddl_drop_index_if_exists_missing", "DROP INDEX IF EXISTS s.nope"),
 ("ddl_create_type2", "CREATE TYPE s.tmp_t (x int)"),
 ("ddl_drop_type", "DROP TYPE s.tmp_t"),
 ("ddl_create_ks2", "CREATE KEYSPACE s2 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"),
 ("ddl_drop_ks2", "DROP KEYSPACE s2"),
 ("ddl_drop_ks_if_exists_missing", "DROP KEYSPACE IF EXISTS s2"),
 ("use", "USE s"),
 ("use_quoted", 'USE "s"'),
 ("truncate", "TRUNCATE s.t2"),
 ("truncate_table_keyword", "TRUNCATE TABLE s.t2"),
]
for cid, x in DDL: raw(cid, 'schema-change', x)
# data, with explicit timestamps
setup(*[x for x in [
 "INSERT INTO s.t (pk, ck, v, s, txt, l, st, m, fl, tu, ad) VALUES (1, 1, 10, 100, 'one', [1, 2, 3], {'a', 'b'}, {'a': 1, 'b': 2}, [7, 8], (1, 'x'), {street: 'Main', zip: 1}) USING TIMESTAMP 1000",
 "INSERT INTO s.t (pk, ck, v, txt) VALUES (1, 2, 20, 'two') USING TIMESTAMP 1000",
 "INSERT INTO s.t (pk, ck, v, txt) VALUES (1, 3, 30, 'three') USING TIMESTAMP 1000",
 "INSERT INTO s.t (pk, ck, v) VALUES (2, 1, 5) USING TIMESTAMP 2000",
 "INSERT INTO s.t (pk, s) VALUES (3, 33) USING TIMESTAMP 3000",
 "INSERT INTO s.t2 (a, b, c, d, v) VALUES (1, 1, 1, 1, 1)", "INSERT INTO s.t2 (a, b, c, d, v) VALUES (1, 1, 1, 2, 2)",
 "INSERT INTO s.t2 (a, b, c, d, v) VALUES (1, 1, 2, 1, 3)", "INSERT INTO s.t2 (a, b, c, d, v) VALUES (1, 2, 1, 1, 4)",
 "INSERT INTO s.ty (pk, a, bi, b, bo, d, de, db, du, f, i, n, sm, tx, tm, ts, tid, ti, u, vc, vi, vec) VALUES (1, 'ascii', 9223372036854775807, 0xcafe, true, '2024-02-29', 123.4500, 1.5e300, 1y2mo3w4d5h6m7s8ms9us10ns, 3.25, '::1', -2147483648, -32768, 'tëxt', '13:14:15.123456789', '2024-02-29 12:34:56.789+0000', 50554d6e-29bb-11e5-b345-feff819cdc9f, -128, 6ba7b810-9dad-41d1-80b4-00c04fd430c8, 'varchar', -123456789012345678901234567890, [1.0, -2.5, 3.0])",
 "INSERT INTO s.ty (pk, de, db, f, i, ts) VALUES (2, -0.000001, NaN, -Infinity, '192.168.0.1', 0)",
 "INSERT INTO s.ty (pk) VALUES (3)",
 "INSERT INTO s.ty (pk, tx, b, a) VALUES (4, '', 0x, '')",
]])
S = 'select'
q('select_star', S, "SELECT * FROM s.t WHERE pk = 1")
q('select_star_all', S, "SELECT * FROM s.t")
q('select_columns_order_desc', S, "SELECT pk, ck, v FROM s.t WHERE pk = 1 ORDER BY ck DESC")
q('select_static_only_partition', S, "SELECT * FROM s.t WHERE pk = 3")
q('select_distinct', S, "SELECT DISTINCT pk, s FROM s.t")
q('select_distinct_token', S, "SELECT DISTINCT token(pk) FROM s.t")
q('select_token', S, "SELECT token(pk), pk FROM s.t WHERE pk = 1 LIMIT 1")
q('select_token_range', S, "SELECT pk, ck FROM s.t WHERE token(pk) > -9223372036854775808 AND token(pk) <= 0")
q('select_aggregates', S, "SELECT count(*), count(v), min(v), max(v), sum(v), avg(v) FROM s.t WHERE pk = 1")
q('select_count_1', S, "SELECT count(1) FROM s.t WHERE pk = 1")
q('select_count_empty', S, "SELECT count(*), max(v) FROM s.t WHERE pk = 99")
q('select_aggregate_and_column', S, "SELECT pk, count(*) FROM s.t WHERE pk = 1")
q('select_aliases', S, 'SELECT v AS "Value", count(*) AS n FROM s.t WHERE pk = 1')
q('select_alias_lower', S, "SELECT v AS Value FROM s.t WHERE pk = 1 AND ck = 1")
q('select_writetime_ttl', S, "SELECT writetime(v), ttl(v), writetime(s), writetime(txt) FROM s.t WHERE pk = 1")
q('select_writetime_collection', S, "SELECT writetime(l), writetime(st) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_writetime_frozen_udt', S, "SELECT writetime(fl), writetime(tu), writetime(ad) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_maxwritetime', S, "SELECT maxwritetime(v) FROM s.t WHERE pk = 1")
q('select_cast', S, "SELECT CAST(v AS text), CAST(v AS double), CAST(v AS varint), CAST(v AS decimal), CAST(v AS bigint) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_cast_lowercase', S, "SELECT cast(v as text) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_cast_all_text', S, "SELECT CAST(a AS text), CAST(bi AS text), CAST(b AS text), CAST(bo AS text), CAST(d AS text), CAST(de AS text), CAST(db AS text), CAST(du AS text), CAST(f AS text), CAST(i AS text), CAST(n AS text), CAST(sm AS text), CAST(tm AS text), CAST(ts AS text), CAST(tid AS text), CAST(ti AS text), CAST(u AS text), CAST(vi AS text) FROM s.ty WHERE pk = 1")
q('select_cast_all_text_2', S, "SELECT CAST(de AS text), CAST(db AS text), CAST(f AS text), CAST(i AS text), CAST(ts AS text) FROM s.ty WHERE pk = 2")
q('select_json_types', S, "SELECT JSON * FROM s.ty")
q('select_json_t', S, "SELECT JSON * FROM s.t WHERE pk = 1")
q('select_json_columns_alias', S, 'SELECT JSON pk, v AS "Val", writetime(v) FROM s.t WHERE pk = 1')
q('select_tojson', S, "SELECT toJson(l), toJson(m), toJson(st), toJson(tu), toJson(ad), toJson(fl) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_tojson_types', S, "SELECT toJson(b), toJson(du), toJson(ts), toJson(tm), toJson(d), toJson(vi), toJson(vec) FROM s.ty WHERE pk = 1")
q('select_star_types', S, "SELECT * FROM s.ty WHERE pk = 1")
q('select_empty_values', S, "SELECT * FROM s.ty WHERE pk = 4")
q('select_blob_functions', S, "SELECT intAsBlob(v), blobAsInt(intAsBlob(v)), textAsBlob(txt), varcharAsBlob(txt) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_time_functions', S, "SELECT toDate(ts), toTimestamp(d), toUnixTimestamp(ts), dateOf(tid), unixTimestampOf(tid), toTimestamp(tid), minTimeuuid('2024-01-01 00:00:00+0000'), maxTimeuuid('2024-01-01 00:00:00+0000') FROM s.ty WHERE pk = 1")
q('select_time_functions_c5', S, "SELECT to_date(ts), to_unix_timestamp(ts), min_timeuuid('2024-01-01 00:00:00+0000') FROM s.ty WHERE pk = 1")
q('select_now_columns', S, "SELECT now(), currentTimestamp(), currentDate(), currentTime(), currentTimeUUID(), uuid() FROM system.local")
q('select_arith', S, "SELECT v * 2, v - 1, v / 3, v % 7, -v FROM s.t WHERE pk = 1 AND ck = 1")
q('select_arith_plus', S, "SELECT v + 1 FROM s.t WHERE pk = 1 AND ck = 1")
q('select_literal', S, "SELECT 1, 'a', (int) 5 FROM s.t WHERE pk = 1 AND ck = 1")
q('select_element', S, "SELECT l[0], m['a'], st['a'] FROM s.t WHERE pk = 1 AND ck = 1")
q('select_slice', S, "SELECT m['a'..'b'], st['a'..'z'] FROM s.t WHERE pk = 1 AND ck = 1")
q('select_udt_field', S, "SELECT ad.street, ad.zip, ad FROM s.t WHERE pk = 1 AND ck = 1")
q('select_c5_functions', S, "SELECT map_keys(m), map_values(m), collection_count(l), collection_min(l), collection_max(l), collection_sum(l), collection_avg(l), abs(v), exp(v), log(v), log10(v), round(v) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_mask_functions', S, "SELECT mask_null(v), mask_default(v), mask_default(txt), mask_replace(txt, '*'), mask_inner(txt, 1, 1), mask_outer(txt, 1, 1), mask_hash(txt) FROM s.t WHERE pk = 1 AND ck = 1")
q('select_vector_similarity', S, "SELECT similarity_cosine(vec, [1.0, 1.0, 1.0]), similarity_euclidean(vec, [1.0, 1.0, 1.0]), similarity_dot_product(vec, [1.0, 1.0, 1.0]) FROM s.ty WHERE pk = 1")
q('select_group_by_pk', S, "SELECT pk, count(*), max(v) FROM s.t GROUP BY pk")
q('select_group_by_ck', S, "SELECT a, b, c, count(*) FROM s.t2 WHERE a = 1 AND b = 1 GROUP BY a, b, c")
q('select_group_by_no_agg', S, "SELECT a, b, c, d, v FROM s.t2 WHERE a = 1 AND b = 1 GROUP BY c")
q('select_per_partition_limit', S, "SELECT pk, ck FROM s.t PER PARTITION LIMIT 1")
q('select_in_order_by_unpaged', S, "SELECT pk, ck FROM s.t WHERE pk IN (2, 1) ORDER BY ck DESC")
q('select_in_order', S, "SELECT pk, ck FROM s.t WHERE pk IN (2, 1)")
q('select_in_clustering', S, "SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND c IN (2, 1)")
q('select_multi_column', S, "SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND (c, d) > (1, 1)")
q('select_multi_column_in', S, "SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND (c, d) IN ((1, 2), (2, 1))")
q('select_clustering_desc_natural', S, "SELECT c, d FROM s.t2 WHERE a = 1 AND b = 1")
q('select_clustering_reversed', S, "SELECT c, d FROM s.t2 WHERE a = 1 AND b = 1 ORDER BY c ASC, d DESC")
q('select_filtering', S, "SELECT pk, ck FROM s.t WHERE v > 10 ALLOW FILTERING")
q('select_filtering_contains', S, "SELECT pk, ck FROM s.t WHERE l CONTAINS 2 ALLOW FILTERING")
q('select_filtering_contains_key', S, "SELECT pk, ck FROM s.t WHERE m CONTAINS KEY 'a' ALLOW FILTERING")
q('select_filtering_map_entry', S, "SELECT pk, ck FROM s.t WHERE m['a'] = 1 ALLOW FILTERING")
q('select_index', S, "SELECT pk, ck, v FROM s.t WHERE v = 20")
q('select_index_with_pk', S, "SELECT pk, ck, v FROM s.t WHERE pk = 1 AND v = 20")
q('select_limit_paged', S, "SELECT pk, ck FROM s.t LIMIT 3", fetch_size=2)
q('select_page_size_2', S, "SELECT pk, ck FROM s.t WHERE pk = 1", fetch_size=2)
q('select_page_size_exact', S, "SELECT pk, ck FROM s.t WHERE pk = 1", fetch_size=3)
q('select_empty_result', S, "SELECT * FROM s.t WHERE pk = 99")
q('select_system_local_some', S, "SELECT key, cluster_name, data_center, rack, partitioner, release_version, cql_version, native_protocol_version FROM system.local")
q('select_tracing_flag', S, "SELECT * FROM s.t WHERE pk = 1")
W = 'write'
def wr(cid, *stmts):
    for x in stmts[:-1]: setup(x)
    q(cid, W, stmts[-1])
wr('collection_ops', "UPDATE s.t SET l = l + [4], l = [0] + l WHERE pk = 1 AND ck = 2",
   "UPDATE s.t SET st = st + {'c'}, m['z'] = 26, m = m + {'y': 25} WHERE pk = 1 AND ck = 2",
   "UPDATE s.t SET l = l - [4] WHERE pk = 1 AND ck = 2", "SELECT l, st, m FROM s.t WHERE pk = 1 AND ck = 2")
wr('list_set_by_index', "UPDATE s.t SET l[1] = 99 WHERE pk = 1 AND ck = 1", "SELECT l FROM s.t WHERE pk = 1 AND ck = 1")
wr('delete_element', "DELETE l[0], m['a'], st['a'] FROM s.t WHERE pk = 1 AND ck = 1", "SELECT l, m, st FROM s.t WHERE pk = 1 AND ck = 1")
wr('set_to_empty_collection', "UPDATE s.t SET l = [], st = {}, m = {} WHERE pk = 1 AND ck = 3", "SELECT l, st, m FROM s.t WHERE pk = 1 AND ck = 3")
wr('udt_field_update', "UPDATE s.t SET ad.zip = 2 WHERE pk = 1 AND ck = 1", "SELECT ad FROM s.t WHERE pk = 1 AND ck = 1")
wr('update_creates_no_row_marker', "UPDATE s.t SET v = 1 WHERE pk = 10 AND ck = 1", "UPDATE s.t SET v = null WHERE pk = 10 AND ck = 1", "SELECT * FROM s.t WHERE pk = 10")
wr('insert_keeps_row_marker', "INSERT INTO s.t (pk, ck, v) VALUES (11, 1, 1)", "UPDATE s.t SET v = null WHERE pk = 11 AND ck = 1", "SELECT * FROM s.t WHERE pk = 11")
wr('range_delete', "INSERT INTO s.t2 (a, b, c, d, v) VALUES (5, 5, 1, 1, 1)", "INSERT INTO s.t2 (a, b, c, d, v) VALUES (5, 5, 2, 1, 1)", "INSERT INTO s.t2 (a, b, c, d, v) VALUES (5, 5, 3, 1, 1)", "DELETE FROM s.t2 WHERE a = 5 AND b = 5 AND c >= 2", "SELECT c, d FROM s.t2 WHERE a = 5 AND b = 5")
wr('tie_same_timestamp_values', "INSERT INTO s.t (pk, ck, v, txt) VALUES (20, 1, 5, 'b') USING TIMESTAMP 5000", "INSERT INTO s.t (pk, ck, v, txt) VALUES (20, 1, 3, 'z') USING TIMESTAMP 5000", "INSERT INTO s.t (pk, ck, v, txt) VALUES (20, 1, 4, 'a') USING TIMESTAMP 5000", "SELECT v, txt FROM s.t WHERE pk = 20")
wr('tie_delete_vs_write', "INSERT INTO s.t (pk, ck, v) VALUES (21, 1, 5) USING TIMESTAMP 5000", "DELETE v FROM s.t USING TIMESTAMP 5000 WHERE pk = 21 AND ck = 1", "SELECT * FROM s.t WHERE pk = 21")
wr('tie_ttl_vs_plain', "INSERT INTO s.t (pk, ck, v) VALUES (22, 1, 5) USING TIMESTAMP 5000 AND TTL 100000", "INSERT INTO s.t (pk, ck, v) VALUES (22, 1, 5) USING TIMESTAMP 5000", "SELECT v, ttl(v) FROM s.t WHERE pk = 22")
wr('older_write_ignored', "INSERT INTO s.t (pk, ck, v) VALUES (23, 1, 5) USING TIMESTAMP 5000", "INSERT INTO s.t (pk, ck, v) VALUES (23, 1, 6) USING TIMESTAMP 4000", "SELECT v, writetime(v) FROM s.t WHERE pk = 23")
wr('insert_json_default_null', "INSERT INTO s.t JSON '{\"pk\": 30, \"ck\": 1, \"v\": 1, \"txt\": \"a\"}'", "INSERT INTO s.t JSON '{\"pk\": 30, \"ck\": 1, \"v\": 2}'", "SELECT v, txt FROM s.t WHERE pk = 30")
wr('insert_json_default_unset', "INSERT INTO s.t JSON '{\"pk\": 31, \"ck\": 1, \"v\": 1, \"txt\": \"a\"}'", "INSERT INTO s.t JSON '{\"pk\": 31, \"ck\": 1, \"v\": 2}' DEFAULT UNSET", "SELECT v, txt FROM s.t WHERE pk = 31")
wr('insert_json_types', "INSERT INTO s.ty JSON '{\"pk\": 10, \"b\": \"0xcafe\", \"du\": \"1h\", \"ts\": \"2024-01-01T00:00:00Z\", \"d\": \"2024-01-01\", \"i\": \"10.0.0.1\", \"vi\": 12345678901234567890, \"de\": \"1.50\", \"vec\": [1, 2, 3]}'", "SELECT JSON pk, b, du, ts, d, i, vi, de, vec FROM s.ty WHERE pk = 10")
wr('fromjson', "INSERT INTO s.t (pk, ck, l, m) VALUES (32, 1, fromJson('[1, 2]'), fromJson('{\"k\": 3}'))", "SELECT l, m FROM s.t WHERE pk = 32")
wr('counter_ops', "UPDATE s.c SET n = n + 5 WHERE pk = 1 AND ck = 1", "UPDATE s.c SET n = n - 2 WHERE pk = 1 AND ck = 1", "SELECT * FROM s.c")
wr('counter_never_set', "SELECT n FROM s.c WHERE pk = 2 AND ck = 2")
wr('static_insert_without_ck', "INSERT INTO s.t (pk, s) VALUES (40, 1)", "SELECT * FROM s.t WHERE pk = 40")
wr('static_then_row', "INSERT INTO s.t (pk, ck, v) VALUES (40, 1, 1)", "SELECT * FROM s.t WHERE pk = 40")
wr('delete_partition', "DELETE FROM s.t WHERE pk = 40", "SELECT * FROM s.t WHERE pk = 40")
wr('delete_static_only', "INSERT INTO s.t (pk, ck, v, s) VALUES (41, 1, 1, 1)", "DELETE s FROM s.t WHERE pk = 41", "SELECT * FROM s.t WHERE pk = 41")
wr('ttl_value', "INSERT INTO s.t (pk, ck, v) VALUES (42, 1, 1) USING TTL 86400", "SELECT ttl(v) FROM s.t WHERE pk = 42")
wr('ttl_zero', "INSERT INTO s.t (pk, ck, v) VALUES (43, 1, 1) USING TTL 0", "SELECT ttl(v) FROM s.t WHERE pk = 43")
wr('batch_results', "BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (44, 1, 1); UPDATE s.t SET v = 2 WHERE pk = 44 AND ck = 2; DELETE FROM s.t WHERE pk = 44 AND ck = 3; APPLY BATCH")
L = 'lwt'
for cid, x in [
 ('lwt_insert_applied', "INSERT INTO s.t (pk, ck, v) VALUES (50, 1, 1) IF NOT EXISTS"),
 ('lwt_insert_not_applied', "INSERT INTO s.t (pk, ck, v) VALUES (50, 1, 2) IF NOT EXISTS"),
 ('lwt_update_applied', "UPDATE s.t SET v = 3 WHERE pk = 50 AND ck = 1 IF v = 1"),
 ('lwt_update_not_applied', "UPDATE s.t SET v = 4 WHERE pk = 50 AND ck = 1 IF v = 1"),
 ('lwt_update_not_applied_two_cols', "UPDATE s.t SET v = 4 WHERE pk = 50 AND ck = 1 IF v = 1 AND txt = 'x'"),
 ('lwt_update_if_exists_missing', "UPDATE s.t SET v = 4 WHERE pk = 51 AND ck = 1 IF EXISTS"),
 ('lwt_update_if_exists', "UPDATE s.t SET v = 5 WHERE pk = 50 AND ck = 1 IF EXISTS"),
 ('lwt_delete_if_exists', "DELETE FROM s.t WHERE pk = 50 AND ck = 1 IF EXISTS"),
 ('lwt_delete_if_exists_missing', "DELETE FROM s.t WHERE pk = 50 AND ck = 1 IF EXISTS"),
 ('lwt_delete_condition_not_applied', "DELETE FROM s.t WHERE pk = 1 AND ck = 2 IF v = 999"),
 ('lwt_static_condition', "UPDATE s.t SET s = 7 WHERE pk = 1 IF s = 100"),
 ('lwt_static_condition_not_applied', "UPDATE s.t SET s = 8 WHERE pk = 1 IF s = 100"),
 ('lwt_in_condition', "UPDATE s.t SET v = 21 WHERE pk = 1 AND ck = 2 IF v IN (20, 21)"),
 ('lwt_null_condition', "UPDATE s.t SET v = 21 WHERE pk = 1 AND ck = 2 IF txt = null"),
 ('lwt_ne_condition', "UPDATE s.t SET v = 22 WHERE pk = 1 AND ck = 2 IF v != 21"),
 ('lwt_collection_element_condition', "UPDATE s.t SET v = 23 WHERE pk = 1 AND ck = 1 IF m['b'] = 2"),
 ('lwt_udt_field_condition', "UPDATE s.t SET v = 23 WHERE pk = 1 AND ck = 1 IF ad.zip = 9"),
 ('lwt_batch_applied', "BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (52, 1, 1) IF NOT EXISTS; INSERT INTO s.t (pk, ck, v) VALUES (52, 2, 1) IF NOT EXISTS; APPLY BATCH"),
 ('lwt_batch_not_applied', "BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (52, 1, 1) IF NOT EXISTS; UPDATE s.t SET v = 9 WHERE pk = 52 AND ck = 3 IF v = 1; APPLY BATCH"),
 ('lwt_counter_free_insert_json', "INSERT INTO s.t JSON '{\"pk\": 53, \"ck\": 1}' IF NOT EXISTS"),
]:
    q(cid, L, x)
P = 'prepared'
def prep(cid, x):
    try:
        p = s.prepare(x)
        md = lambda ms: [[m[0], m[1], m[2], typename(m[3])] for m in (ms or [])]
        r = {'ok': True, 'bind': md(p.column_metadata), 'result': md(p.result_metadata),
             'routing_key_indexes': p.routing_key_indexes, 'is_lwt': p.is_lwt() if hasattr(p, 'is_lwt') else None}
    except Exception as e:
        r = {'ok': False, 'error': err(e)}
    RES.append([cid, P, x, r])
for cid, x in [
 ('prep_select_pk', "SELECT * FROM s.t WHERE pk = ?"),
 ('prep_select_named', "SELECT v FROM s.t WHERE pk = :p AND ck = :c"),
 ('prep_select_in', "SELECT * FROM s.t WHERE pk IN ?"),
 ('prep_select_in_markers', "SELECT * FROM s.t WHERE pk IN (?, ?)"),
 ('prep_select_limit', "SELECT * FROM s.t WHERE pk = ? LIMIT ?"),
 ('prep_select_ppl', "SELECT * FROM s.t PER PARTITION LIMIT ?"),
 ('prep_select_token', "SELECT * FROM s.t WHERE token(pk) > ? AND token(pk) <= ?"),
 ('prep_select_slice', "SELECT * FROM s.t WHERE pk = ? AND ck > ? AND ck <= ?"),
 ('prep_select_multi', "SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) > (?, ?)"),
 ('prep_select_multi_tuple', "SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) > ?"),
 ('prep_select_multi_in', "SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) IN ?"),
 ('prep_select_contains', "SELECT * FROM s.t WHERE l CONTAINS ? ALLOW FILTERING"),
 ('prep_select_map_entry', "SELECT * FROM s.t WHERE m[?] = ? ALLOW FILTERING"),
 ('prep_select_composite', "SELECT * FROM s.t2 WHERE a = ? AND b = ?"),
 ('prep_select_composite_reversed', "SELECT * FROM s.t2 WHERE b = ? AND a = ?"),
 ('prep_select_function_arg', "SELECT * FROM s.t WHERE pk = blobAsInt(?)"),
 ('prep_select_cast_arg', "SELECT CAST(? AS text) FROM s.t"),
 ('prep_select_json', "SELECT JSON * FROM s.t WHERE pk = ?"),
 ('prep_select_ann', "SELECT * FROM s.ty ORDER BY vec ANN OF ? LIMIT 1"),
 ('prep_insert', "INSERT INTO s.t (pk, ck, v) VALUES (?, ?, ?)"),
 ('prep_insert_ttl_ts', "INSERT INTO s.t (pk, ck, v) VALUES (?, ?, ?) USING TTL ? AND TIMESTAMP ?"),
 ('prep_insert_json', "INSERT INTO s.t JSON ?"),
 ('prep_insert_lwt', "INSERT INTO s.t (pk, ck) VALUES (?, ?) IF NOT EXISTS"),
 ('prep_update_collection', "UPDATE s.t SET l = l + ?, m[?] = ?, st = st - ? WHERE pk = ? AND ck = ?"),
 ('prep_update_list_index', "UPDATE s.t SET l[?] = ? WHERE pk = ? AND ck = ?"),
 ('prep_update_udt_field', "UPDATE s.t SET ad.zip = ? WHERE pk = ? AND ck = ?"),
 ('prep_update_lwt', "UPDATE s.t SET v = ? WHERE pk = ? AND ck = ? IF v = ?"),
 ('prep_update_lwt_element', "UPDATE s.t SET v = ? WHERE pk = ? AND ck = ? IF m[?] = ?"),
 ('prep_counter', "UPDATE s.c SET n = n + ? WHERE pk = ? AND ck = ?"),
 ('prep_delete', "DELETE FROM s.t WHERE pk = ? AND ck = ?"),
 ('prep_delete_element', "DELETE m[?] FROM s.t WHERE pk = ? AND ck = ?"),
 ('prep_delete_timestamp', "DELETE FROM s.t USING TIMESTAMP ? WHERE pk = ?"),
 ('prep_timeout', "SELECT * FROM s.t WHERE pk = ? USING TIMEOUT ?"),
 ('prep_system_local', "SELECT * FROM system.local"),
 ('prep_star_with_writetime', "SELECT writetime(v), ttl(v), token(pk), count(*) FROM s.t WHERE pk = ?"),
 ('prep_batch_member', "BEGIN BATCH INSERT INTO s.t (pk, ck) VALUES (?, ?); APPLY BATCH"),
]:
    prep(cid, x)
# tracing and keyspace-less statements via raw
RES.append(['tracing_session_id', 'raw', 'SELECT * FROM s.t WHERE pk = 1 (tracing flag)', R.query("SELECT * FROM s.t WHERE pk = 1", tracing=True)])
R.query("DROP KEYSPACE IF EXISTS s")
json.dump(RES, open(out, 'w'), indent=1, default=repr)
print('done', len(RES))
