# Error / warning capture. Writes JSON: list of [id, category, statement, result].
# String statements go through the raw client (exact code, message, extra fields, warnings).
import sys, json, time, os
sys.path.insert(0, '/cap')
from common import connect, run as drun, err
from raw import Raw
from cassandra.query import SimpleStatement, BatchStatement, BatchType
from cassandra import ConsistencyLevel
host = os.environ.get('CAP_HOST', '127.0.0.1'); out = sys.argv[1]
c, s = connect(host)
RAW = [None]
def raw():
    if RAW[0] is None: RAW[0] = Raw(host)
    return RAW[0]
def run(q, page_size=5000, consistency='ONE', serial=None, timestamp=None):
    last = None
    for _ in range(3):
        try:
            return raw().query(q, page_size, consistency, serial, timestamp)
        except Exception as e:
            RAW[0] = None; time.sleep(2); last = {'ok': False, 'error': {'code': None, 'message': 'client: ' + repr(e)}}
    return last
RES = []
def case(cid, cat, q, **kw):
    RES.append([cid, cat, q, run(q, **kw)])
def dcase(cid, cat, label, fn):
    try: r = fn()
    except Exception as e: r = {'ok': False, 'error': err(e)}
    RES.append([cid, cat, label, r])
def bulk(qs):
    for q in qs:
        r = run(q)
        if not r['ok']: RES.append(['setup-failure', 'setup', q, r])

SETUP = [
 "DROP KEYSPACE IF EXISTS e", "DROP KEYSPACE IF EXISTS e3", "DROP KEYSPACE IF EXISTS e3t", "DROP KEYSPACE IF EXISTS w1", "DROP KEYSPACE IF EXISTS w2",
 "CREATE KEYSPACE e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}",
 "CREATE TYPE e.ty (x int, y text)",
 "CREATE TABLE e.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<int>, m map<int, int>, fl frozen<list<int>>, fm frozen<map<int, int>>, tu tuple<int, text>, PRIMARY KEY (pk, ck))",
 "CREATE TABLE e.t2 (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d))",
 "CREATE TABLE e.c (pk int PRIMARY KEY, cnt counter)",
 "CREATE TABLE e.u (id int PRIMARY KEY, ts timestamp, u uuid, tu timeuuid, b blob, dt date, tx text, du duration, asc_ ascii, bi bigint, sm smallint, ti tinyint, vi varint, de decimal, fl float, db double, ine inet, tm time, bo boolean)",
 "CREATE TABLE e.ix (pk int, ck int, v int, PRIMARY KEY (pk, ck))",
 "CREATE INDEX ix_v ON e.ix (v)",
 "CREATE TABLE e.uses (pk int PRIMARY KEY, t frozen<ty>, nt ty)",
 "CREATE TABLE e.big (pk int, ck int, v text, PRIMARY KEY (pk, ck))",
 "CREATE TABLE e.t2r (a int, b int, PRIMARY KEY (a, b))",
 "CREATE TABLE e.tomb (pk int, ck int, v int, PRIMARY KEY (pk, ck))",
 "CREATE TABLE e.vec (pk int PRIMARY KEY, emb vector<float, 3>)",
 "CREATE TABLE e.blobk (pk blob PRIMARY KEY, v int)",
 "CREATE TABLE e.textk (pk text, ck text, v int, PRIMARY KEY (pk, ck))",
 "INSERT INTO e.t (pk, ck, v, txt, l) VALUES (1, 1, 1, 'a', [1, 2])",
]
RES.append(['setup', 'setup', '', [[q, run(q)] for q in SETUP]])

I = 'select'
case('unknown_keyspace', I, "SELECT * FROM nope.t")
case('unknown_table', I, "SELECT * FROM e.nope")
case('unknown_column_select', I, "SELECT nope FROM e.t")
case('unknown_column_where', I, "SELECT * FROM e.t WHERE nope = 1")
case('no_keyspace', I, "SELECT * FROM t")
case('filtering_regular', I, "SELECT * FROM e.t WHERE v = 1")
case('filtering_clustering_only', I, "SELECT * FROM e.t WHERE ck = 1")
case('filtering_clustering_slice_only', I, "SELECT * FROM e.t WHERE ck > 1")
case('filtering_with_pk_regular', I, "SELECT * FROM e.t WHERE pk = 1 AND v = 1")
case('filtering_static', I, "SELECT * FROM e.t WHERE s = 1")
case('filtering_ok', I, "SELECT * FROM e.t WHERE v = 1 ALLOW FILTERING")
case('partial_partition_key', I, "SELECT * FROM e.t2 WHERE a = 1")
case('partial_partition_key_filtering', I, "SELECT * FROM e.t2 WHERE a = 1 ALLOW FILTERING")
case('clustering_skip', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND d = 1")
case('clustering_after_slice', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND c > 1 AND d = 1")
case('clustering_in_then_slice', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND c IN (1, 2) AND d > 1")
case('partition_key_slice', I, "SELECT * FROM e.t WHERE pk > 1")
case('partition_key_slice_filtering', I, "SELECT * FROM e.t WHERE pk > 1 ALLOW FILTERING")
case('partition_key_neq', I, "SELECT * FROM e.t WHERE pk != 1")
case('neq_regular', I, "SELECT * FROM e.t WHERE pk = 1 AND v != 1 ALLOW FILTERING")
case('token_and_eq', I, "SELECT * FROM e.t WHERE token(pk) > 0 AND pk = 1")
case('token_on_clustering', I, "SELECT * FROM e.t WHERE token(ck) > 0")
case('token_partial', I, "SELECT * FROM e.t2 WHERE token(a) > 0")
case('token_wrong_order', I, "SELECT * FROM e.t2 WHERE token(b, a) > 0")
case('token_eq_literal_type', I, "SELECT * FROM e.t WHERE token(pk) = 'a'")
case('multiple_eq_same_column', I, "SELECT * FROM e.t WHERE pk = 1 AND ck = 1 AND ck = 2")
case('eq_and_in_same_column', I, "SELECT * FROM e.t WHERE pk = 1 AND pk IN (1, 2)")
case('two_lower_bounds', I, "SELECT * FROM e.t WHERE pk = 1 AND ck > 1 AND ck >= 2")
case('multi_column_non_clustering', I, "SELECT * FROM e.t WHERE pk = 1 AND (ck, v) > (1, 1)")
case('multi_column_arity', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (c, d) = (1)")
case('multi_column_out_of_order', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (d, c) = (1, 1)")
case('multi_and_single_mix', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (c, d) > (1, 1) AND c < 5")
case('contains_no_index', I, "SELECT * FROM e.t WHERE pk = 1 AND l CONTAINS 1")
case('contains_non_collection', I, "SELECT * FROM e.t WHERE pk = 1 AND v CONTAINS 1 ALLOW FILTERING")
case('contains_key_on_list', I, "SELECT * FROM e.t WHERE pk = 1 AND l CONTAINS KEY 1 ALLOW FILTERING")
case('contains_frozen_filtering', I, "SELECT * FROM e.t WHERE pk = 1 AND fl CONTAINS 1 ALLOW FILTERING")
case('map_entry_no_filtering', I, "SELECT * FROM e.t WHERE pk = 1 AND m[1] = 2")
case('map_entry_filtering', I, "SELECT * FROM e.t WHERE pk = 1 AND m[1] = 2 ALLOW FILTERING")
case('like_no_index', I, "SELECT * FROM e.t WHERE pk = 1 AND txt LIKE 'a%'")
case('like_filtering', I, "SELECT * FROM e.t WHERE pk = 1 AND txt LIKE 'a%' ALLOW FILTERING")
case('is_not_null_select', I, "SELECT * FROM e.t WHERE v IS NOT NULL ALLOW FILTERING")
case('is_null_select', I, "SELECT * FROM e.t WHERE v IS NULL ALLOW FILTERING")
case('in_on_regular', I, "SELECT * FROM e.t WHERE pk = 1 AND v IN (1, 2) ALLOW FILTERING")
case('in_empty', I, "SELECT * FROM e.t WHERE pk IN ()")
case('in_null_element', I, "SELECT * FROM e.t WHERE pk IN (1, null)")
case('in_collection_column', I, "SELECT * FROM e.t WHERE pk = 1 AND l IN ([1]) ALLOW FILTERING")
case('order_by_no_pk', I, "SELECT * FROM e.t ORDER BY ck DESC")
case('order_by_regular', I, "SELECT * FROM e.t WHERE pk = 1 ORDER BY v")
case('order_by_pk', I, "SELECT * FROM e.t WHERE pk = 1 ORDER BY pk")
case('order_by_skip', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 ORDER BY d")
case('order_by_mixed', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 ORDER BY c ASC, d DESC")
case('order_by_unknown', I, "SELECT * FROM e.t WHERE pk = 1 ORDER BY nope")
case('order_by_twice', I, "SELECT * FROM e.t WHERE pk = 1 ORDER BY ck, ck")
case('order_by_secondary_index', I, "SELECT * FROM e.ix WHERE v = 1 ORDER BY ck DESC")
case('in_order_by_paged', I, "SELECT * FROM e.t WHERE pk IN (1, 2) ORDER BY ck DESC")
case('in_order_by_unpaged', I, "SELECT * FROM e.t WHERE pk IN (1, 2) ORDER BY ck DESC", page_size=None)
case('limit_zero', I, "SELECT * FROM e.t LIMIT 0")
case('limit_negative', I, "SELECT * FROM e.t LIMIT -1")
case('per_partition_limit_zero', I, "SELECT * FROM e.t PER PARTITION LIMIT 0")
case('per_partition_limit_distinct', I, "SELECT DISTINCT pk FROM e.t PER PARTITION LIMIT 1")
case('distinct_regular', I, "SELECT DISTINCT v FROM e.t")
case('distinct_clustering', I, "SELECT DISTINCT pk, ck FROM e.t")
case('distinct_partial_pk', I, "SELECT DISTINCT a FROM e.t2")
case('distinct_with_clustering_restriction', I, "SELECT DISTINCT pk FROM e.t WHERE ck = 1 ALLOW FILTERING")
case('group_by_regular', I, "SELECT * FROM e.t WHERE pk = 1 GROUP BY v")
case('group_by_order', I, "SELECT * FROM e.t2 WHERE a = 1 AND b = 1 GROUP BY a, b, d")
case('group_by_partial_pk', I, "SELECT a, count(*) FROM e.t2 GROUP BY a")
case('group_by_unknown', I, "SELECT * FROM e.t GROUP BY nope")
case('unknown_function', I, "SELECT nofunc(v) FROM e.t")
case('aggregate_wrong_type', I, "SELECT sum(txt) FROM e.t WHERE pk = 1")
case('function_wrong_arity', I, "SELECT blobAsInt(v, v) FROM e.t WHERE pk = 1")
case('function_wrong_arg_type', I, "SELECT blobAsInt(v) FROM e.t WHERE pk = 1")
case('function_in_where', I, "SELECT * FROM e.t WHERE pk = 1 AND ck = blobAsInt(0x00000001)")
case('aggregate_in_where', I, "SELECT * FROM e.t WHERE pk = 1 AND ck = max(ck)")
case('aggregate_mixed_with_column', I, "SELECT v, count(*) FROM e.t WHERE pk = 1")
case('nested_aggregate', I, "SELECT max(count(*)) FROM e.t")
case('ttl_on_pk', I, "SELECT ttl(pk) FROM e.t")
case('writetime_on_ck', I, "SELECT writetime(ck) FROM e.t")
case('writetime_on_collection', I, "SELECT writetime(l) FROM e.t WHERE pk = 1")
case('writetime_on_frozen', I, "SELECT writetime(fl) FROM e.t WHERE pk = 1")
case('maxwritetime_on_collection', I, "SELECT maxwritetime(l) FROM e.t WHERE pk = 1")
case('ttl_on_counter', I, "SELECT ttl(cnt) FROM e.c")
case('writetime_on_function', I, "SELECT writetime(blobAsInt(0x00000001)) FROM e.t")
case('cast_invalid', I, "SELECT CAST(l AS text) FROM e.t WHERE pk = 1")
case('cast_unknown_type', I, "SELECT CAST(v AS nope) FROM e.t WHERE pk = 1")
case('cast_text_to_int', I, "SELECT CAST(txt AS int) FROM e.t WHERE pk = 1")
case('json_and_distinct', I, "SELECT JSON DISTINCT pk FROM e.t")
case('json_unknown', I, "SELECT JSON nope FROM e.t")
case('alias_in_where', I, "SELECT v AS x FROM e.t WHERE x = 1 ALLOW FILTERING")
case('duplicate_alias', I, "SELECT v AS x, ck AS x FROM e.t WHERE pk = 1")
case('select_element_of_list', I, "SELECT l[0] FROM e.t WHERE pk = 1")
case('select_element_of_map_wrong_type', I, "SELECT m['a'] FROM e.t WHERE pk = 1")
case('select_slice_of_map', I, "SELECT m[1..2] FROM e.t WHERE pk = 1")
case('select_udt_field_unknown', I, "SELECT t.z FROM e.uses")
case('select_field_of_non_udt', I, "SELECT v.x FROM e.t")
case('arithmetic_type_mismatch', I, "SELECT v + txt FROM e.t WHERE pk = 1")
case('arithmetic_division_by_zero', I, "SELECT v / 0 FROM e.t WHERE pk = 1")
case('select_bind_unprepared', I, "SELECT * FROM e.t WHERE pk = ?")
case('select_named_bind_unprepared', I, "SELECT * FROM e.t WHERE pk = :p")
case('ann_on_non_vector', I, "SELECT * FROM e.t ORDER BY v ANN OF [1, 2, 3] LIMIT 1")
case('ann_without_index', I, "SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3] LIMIT 1")
case('ann_wrong_dimension', I, "SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2] LIMIT 1")
case('select_count_star_alias', I, "SELECT count(*) AS n FROM e.t WHERE pk = 1")
case('select_bypass_cache', I, "SELECT * FROM e.t WHERE pk = 1 BYPASS CACHE")
case('select_using_timeout', I, "SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT 10s")
case('select_using_timeout_tiny', I, "SELECT * FROM e.big USING TIMEOUT 1us")
case('select_where_true', I, "SELECT * FROM e.t WHERE pk = 1 AND true")
case('select_or', I, "SELECT * FROM e.t WHERE pk = 1 OR pk = 2")
case('select_paren_where', I, "SELECT * FROM e.t WHERE (pk = 1)")
case('select_empty_blob_key', I, "SELECT * FROM e.blobk WHERE pk = 0x")
case('select_empty_text_key', I, "SELECT * FROM e.textk WHERE pk = ''")
case('select_from_index_table', I, "SELECT * FROM e.ix_v_index")
case('select_from_index_scylla_name', I, "SELECT * FROM e.ix_v")
case('select_index_eq_and_filter', I, "SELECT * FROM e.ix WHERE v = 1 AND ck = 1")
case('select_index_range', I, "SELECT * FROM e.ix WHERE v > 1")
case('select_writetime_alias_star', I, "SELECT *, writetime(v) FROM e.t")
case('select_token_function', I, "SELECT token(pk), token(ck) FROM e.t")

W = 'write'
case('unknown_column_insert', W, "INSERT INTO e.t (pk, ck, nope) VALUES (1, 1, 1)")
case('unknown_column_update', W, "UPDATE e.t SET nope = 1 WHERE pk = 1 AND ck = 1")
case('unknown_column_delete', W, "DELETE nope FROM e.t WHERE pk = 1 AND ck = 1")
case('type_mismatch_pk', W, "INSERT INTO e.t (pk, ck, v) VALUES ('a', 1, 1)")
case('type_mismatch_where', W, "SELECT * FROM e.t WHERE pk = 'a'")
case('int_overflow', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 2147483648)")
case('smallint_overflow', W, "INSERT INTO e.u (id, sm) VALUES (1, 40000)")
case('tinyint_overflow', W, "INSERT INTO e.u (id, ti) VALUES (1, 300)")
case('bigint_overflow', W, "INSERT INTO e.u (id, bi) VALUES (1, 9223372036854775808)")
case('float_into_int', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1.5)")
case('int_into_text', W, "INSERT INTO e.t (pk, ck, txt) VALUES (1, 1, 1)")
case('text_into_ascii_nonascii', W, "INSERT INTO e.u (id, asc_) VALUES (1, 'é')")
case('bad_timestamp_literal', W, "INSERT INTO e.u (id, ts) VALUES (1, 'notadate')")
case('bad_date_literal', W, "INSERT INTO e.u (id, dt) VALUES (1, '2020-13-45')")
case('bad_time_literal', W, "INSERT INTO e.u (id, tm) VALUES (1, '25:00:00')")
case('bad_inet_literal', W, "INSERT INTO e.u (id, ine) VALUES (1, '999.1.1.1')")
case('bad_boolean_literal', W, "INSERT INTO e.u (id, bo) VALUES (1, 'yes')")
case('bad_decimal_literal', W, "INSERT INTO e.u (id, de) VALUES (1, 'abc')")
case('nan_into_decimal', W, "INSERT INTO e.u (id, de) VALUES (1, NaN)")
case('uuid_from_int', W, "INSERT INTO e.u (id, u) VALUES (1, 123)")
case('uuid_bad_string', W, "INSERT INTO e.u (id, u) VALUES (1, 'not-a-uuid')")
case('uuid_into_timeuuid', W, "INSERT INTO e.u (id, tu) VALUES (1, 6ba7b810-9dad-41d1-80b4-00c04fd430c8)")
case('blob_from_string', W, "INSERT INTO e.u (id, b) VALUES (1, 'text')")
case('blob_odd_hex', W, "INSERT INTO e.u (id, b) VALUES (1, 0x123)")
case('bad_duration', W, "INSERT INTO e.u (id, du) VALUES (1, '1x')")
case('duration_in_pk', W, "CREATE TABLE e.dk (pk duration PRIMARY KEY)")
case('duration_in_clustering', W, "CREATE TABLE e.dk2 (pk int, ck duration, PRIMARY KEY (pk, ck))")
case('duration_in_set', W, "CREATE TABLE e.dk3 (pk int PRIMARY KEY, s set<duration>)")
case('vector_wrong_dimension', W, "INSERT INTO e.vec (pk, emb) VALUES (1, [1.0, 2.0])")
case('vector_null_element', W, "INSERT INTO e.vec (pk, emb) VALUES (1, [1.0, null, 2.0])")
case('missing_clustering_insert', W, "INSERT INTO e.t (pk, v) VALUES (1, 1)")
case('missing_pk_insert', W, "INSERT INTO e.t (ck, v) VALUES (1, 1)")
case('values_count_mismatch', W, "INSERT INTO e.t (pk, ck) VALUES (1, 2, 3)")
case('duplicate_insert_column', W, "INSERT INTO e.t (pk, ck, v, v) VALUES (1, 1, 1, 1)")
case('null_pk_literal', W, "INSERT INTO e.t (pk, ck, v) VALUES (null, 1, 1)")
case('null_ck_literal', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, null, 1)")
case('empty_blob_pk', W, "INSERT INTO e.blobk (pk, v) VALUES (0x, 1)")
case('empty_text_pk', W, "INSERT INTO e.textk (pk, ck, v) VALUES ('', 'a', 1)")
case('empty_text_ck', W, "INSERT INTO e.textk (pk, ck, v) VALUES ('a', '', 1)")
case('pk_too_large', W, "INSERT INTO e.textk (pk, ck, v) VALUES ('" + 'x' * 70000 + "', 'a', 1)")
case('insert_only_pk_static', W, "INSERT INTO e.t (pk, s) VALUES (1, 1)")
case('update_pk_column', W, "UPDATE e.t SET pk = 1 WHERE pk = 1 AND ck = 1")
case('update_ck_column', W, "UPDATE e.t SET ck = 1 WHERE pk = 1 AND ck = 1")
case('update_missing_clustering', W, "UPDATE e.t SET v = 1 WHERE pk = 1")
case('update_static_with_ck_only', W, "UPDATE e.t SET s = 1 WHERE ck = 1")
case('update_static_only_pk', W, "UPDATE e.t SET s = 1 WHERE pk = 1")
case('update_in_clustering', W, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck IN (1, 2)")
case('update_range_clustering', W, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck > 1")
case('update_regular_where', W, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 AND v = 1")
case('update_same_column_twice', W, "UPDATE e.t SET v = 1, v = 2 WHERE pk = 1 AND ck = 1")
case('update_token_where', W, "UPDATE e.t SET v = 1 WHERE token(pk) = 1 AND ck = 1")
case('delete_range_with_column', W, "DELETE v FROM e.t WHERE pk = 1 AND ck > 1")
case('delete_no_pk', W, "DELETE FROM e.t WHERE ck = 1")
case('delete_partial_pk', W, "DELETE FROM e.t2 WHERE a = 1")
case('delete_regular_where', W, "DELETE FROM e.t WHERE pk = 1 AND v = 1")
case('delete_range_ok', W, "DELETE FROM e.t WHERE pk = 1 AND ck > 1 AND ck < 5")
case('delete_clustering_skip', W, "DELETE FROM e.t2 WHERE a = 1 AND b = 1 AND d = 1")
case('delete_pk_column', W, "DELETE pk FROM e.t WHERE pk = 1 AND ck = 1")
case('delete_static_with_ck', W, "DELETE s FROM e.t WHERE pk = 1 AND ck = 1")
case('delete_list_element_by_index_frozen', W, "DELETE fl[0] FROM e.t WHERE pk = 1 AND ck = 1")
case('delete_set_element', W, "DELETE st[1] FROM e.t WHERE pk = 1 AND ck = 1")
case('delete_in_pk', W, "DELETE FROM e.t WHERE pk IN (1, 2) AND ck = 1")
case('delete_using_ttl', W, "DELETE FROM e.t USING TTL 10 WHERE pk = 1 AND ck = 1")
case('list_add_set_literal', W, "UPDATE e.t SET l = l + {1} WHERE pk = 1 AND ck = 1")
case('list_prepend_wrong_type', W, "UPDATE e.t SET l = ['a'] + l WHERE pk = 1 AND ck = 1")
case('list_subtract_from_other', W, "UPDATE e.t SET l = st - l WHERE pk = 1 AND ck = 1")
case('frozen_element_set', W, "UPDATE e.t SET fl[0] = 1 WHERE pk = 1 AND ck = 1")
case('frozen_append', W, "UPDATE e.t SET fl = fl + [1] WHERE pk = 1 AND ck = 1")
case('set_index_set', W, "UPDATE e.t SET st[0] = 1 WHERE pk = 1 AND ck = 1")
case('list_index_out_of_bounds_null', W, "UPDATE e.t SET l[5] = 1 WHERE pk = 1 AND ck = 1")
case('list_index_out_of_bounds_existing', W, "UPDATE e.t SET l[5] = 1 WHERE pk = 1 AND ck = 1")
case('list_index_negative', W, "UPDATE e.t SET l[-1] = 1 WHERE pk = 1 AND ck = 1")
case('map_set_wrong_value_type', W, "UPDATE e.t SET m[1] = 'a' WHERE pk = 1 AND ck = 1")
case('map_add_list', W, "UPDATE e.t SET m = m + [1] WHERE pk = 1 AND ck = 1")
case('null_in_list_literal', W, "INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [1, null])")
case('null_in_set_literal', W, "INSERT INTO e.t (pk, ck, st) VALUES (1, 1, {1, null})")
case('null_map_value', W, "INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {1: null})")
case('null_map_key', W, "INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {null: 1})")
case('map_wrong_key_type', W, "INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {'a': 1})")
case('set_literal_for_map_empty', W, "INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {})")
case('list_literal_for_set', W, "INSERT INTO e.t (pk, ck, st) VALUES (1, 1, [1])")
case('tuple_too_many', W, "INSERT INTO e.t (pk, ck, tu) VALUES (1, 1, (1, 'a', 3))")
case('tuple_wrong_type', W, "INSERT INTO e.t (pk, ck, tu) VALUES (1, 1, ('a', 'a'))")
case('udt_unknown_field', W, "INSERT INTO e.uses (pk, t) VALUES (1, {x: 1, z: 'a'})")
case('udt_wrong_field_type', W, "INSERT INTO e.uses (pk, t) VALUES (1, {x: 'a'})")
case('udt_set_field_frozen', W, "UPDATE e.uses SET t.x = 1 WHERE pk = 1")
case('udt_set_field_nonfrozen_ok', W, "UPDATE e.uses SET nt.x = 1 WHERE pk = 1")
case('udt_set_unknown_field', W, "UPDATE e.uses SET nt.z = 1 WHERE pk = 1")
case('ttl_negative', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL -1")
case('ttl_too_large', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 999999999")
case('ttl_string', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 'a'")
case('timestamp_overflow_literal', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP 9223372036854775808")
case('timestamp_min_long', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP -9223372036854775808")
case('using_ttl_twice', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 1 AND TTL 2")
case('using_timeout_insert', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 5s")
case('insert_json_unknown_field', W, "INSERT INTO e.t JSON '{\"pk\": 1, \"ck\": 1, \"nope\": 3}'")
case('insert_json_malformed', W, "INSERT INTO e.t JSON '{bad'")
case('insert_json_wrong_type', W, "INSERT INTO e.t JSON '{\"pk\": \"x\", \"ck\": 1}'")
case('insert_json_missing_pk', W, "INSERT INTO e.t JSON '{\"ck\": 1}'")
case('insert_json_array', W, "INSERT INTO e.t JSON '[1]'")
case('insert_json_null_pk', W, "INSERT INTO e.t JSON '{\"pk\": null, \"ck\": 1}'")
case('insert_json_default_unset', W, "INSERT INTO e.t JSON '{\"pk\": 1, \"ck\": 1}' DEFAULT UNSET")
case('insert_json_default_bogus', W, "INSERT INTO e.t JSON '{\"pk\": 1, \"ck\": 1}' DEFAULT FOO")
case('fromjson_bad', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, fromJson('\"x\"'))")
case('insert_if_exists', W, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF EXISTS")

C = 'counter'
case('counter_set', C, "UPDATE e.c SET cnt = 1 WHERE pk = 1")
case('counter_insert', C, "INSERT INTO e.c (pk, cnt) VALUES (1, 1)")
case('counter_mixed_table', C, "CREATE TABLE e.cm (pk int PRIMARY KEY, c counter, v int)")
case('counter_ttl', C, "UPDATE e.c USING TTL 10 SET cnt = cnt + 1 WHERE pk = 1")
case('counter_timestamp', C, "UPDATE e.c USING TIMESTAMP 10 SET cnt = cnt + 1 WHERE pk = 1")
case('counter_in_pk', C, "CREATE TABLE e.cp (pk counter PRIMARY KEY, v int)")
case('counter_in_collection', C, "CREATE TABLE e.cp2 (pk int PRIMARY KEY, v list<counter>)")
case('counter_static', C, "CREATE TABLE e.cp3 (pk int, ck int, c counter static, d counter, PRIMARY KEY (pk, ck))")
case('counter_increment_by_text', C, "UPDATE e.c SET cnt = cnt + 'a' WHERE pk = 1")
case('counter_increment_other_column', C, "UPDATE e.c SET cnt = pk + 1 WHERE pk = 1")
case('counter_minus_reversed', C, "UPDATE e.c SET cnt = 1 - cnt WHERE pk = 1")
case('counter_delete_column_ok', C, "DELETE cnt FROM e.c WHERE pk = 1")
case('non_counter_increment', C, "UPDATE e.t SET v = v + 1 WHERE pk = 1 AND ck = 1")
case('counter_lwt', C, "UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1 IF cnt = 1")
case('counter_default_ttl', C, "CREATE TABLE e.cttl (pk int PRIMARY KEY, c counter) WITH default_time_to_live = 10")
case('counter_alter_add_regular', C, "ALTER TABLE e.c ADD v int")
case('counter_overflow_ok', C, "UPDATE e.c SET cnt = cnt + 9223372036854775807 WHERE pk = 5")
case('counter_overflow_wrap', C, "UPDATE e.c SET cnt = cnt + 9223372036854775807 WHERE pk = 5")
case('counter_index', C, "CREATE INDEX ON e.c (cnt)")
case('counter_consistency_any', C, "UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1", consistency='ANY')

L = 'lwt'
case('lwt_custom_timestamp', L, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF NOT EXISTS USING TIMESTAMP 1")
case('lwt_in', L, "UPDATE e.t SET v = 1 WHERE pk IN (1, 2) AND ck = 1 IF v = 1")
case('lwt_in_clustering', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck IN (1, 2) IF v = 1")
case('lwt_range_delete', L, "DELETE FROM e.t WHERE pk = 1 AND ck > 1 IF EXISTS")
case('lwt_condition_on_pk', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF pk = 1")
case('lwt_condition_on_ck', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF ck = 1")
case('lwt_condition_unknown', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF nope = 1")
case('lwt_condition_type', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 'a'")
case('lwt_if_exists_and_condition', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF EXISTS AND v = 1")
case('lwt_insert_if_condition', L, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF v = 1")
case('lwt_static_only_condition', L, "UPDATE e.t SET s = 1 WHERE pk = 1 IF s = 1")
case('lwt_list_element_condition', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF l[0] = 1")
case('lwt_map_element_condition_wrong_type', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF m['a'] = 1")
case('lwt_in_condition', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v IN (1, 2)")
case('lwt_contains_condition', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF l CONTAINS 1")
case('lwt_like_condition', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF txt LIKE 'a%'")
case('lwt_serial_consistency_one', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1", serial='LOCAL_ONE')
case('lwt_serial_read_ok', L, "SELECT * FROM e.t WHERE pk = 1 AND ck = 1", consistency='SERIAL')
case('lwt_serial_read_range', L, "SELECT * FROM e.t", consistency='SERIAL')
case('lwt_ok_not_applied', L, "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 5")
case('lwt_ok_insert', L, "INSERT INTO e.t (pk, ck, v) VALUES (9, 9, 9) IF NOT EXISTS")
case('lwt_ok_insert_again', L, "INSERT INTO e.t (pk, ck, v) VALUES (9, 9, 9) IF NOT EXISTS")
case('lwt_with_ttl_ok', L, "UPDATE e.t USING TTL 100 SET v = 1 WHERE pk = 1 AND ck = 1 IF EXISTS")

B = 'batch'
case('batch_lwt_two_partitions', B, "BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; UPDATE e.t SET v = 1 WHERE pk = 2 AND ck = 1 IF v = 1; APPLY BATCH")
case('batch_lwt_two_tables', B, "BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; UPDATE e.ix SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; APPLY BATCH")
case('batch_lwt_with_timestamp', B, "BEGIN BATCH USING TIMESTAMP 5 UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; APPLY BATCH")
case('batch_lwt_ok_mixed', B, "BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; INSERT INTO e.t (pk, ck, v) VALUES (1, 2, 2); APPLY BATCH")
case('batch_counter_in_logged', B, "BEGIN BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH")
case('batch_counter_in_unlogged', B, "BEGIN UNLOGGED BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH")
case('batch_regular_in_counter', B, "BEGIN COUNTER BATCH INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH")
case('batch_counter_ok', B, "BEGIN COUNTER BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH")
case('batch_mixed_counter_regular', B, "BEGIN UNLOGGED BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH")
case('batch_select', B, "BEGIN BATCH SELECT * FROM e.t; APPLY BATCH")
case('batch_ddl', B, "BEGIN BATCH CREATE TABLE e.zz (a int PRIMARY KEY); APPLY BATCH")
case('batch_truncate', B, "BEGIN BATCH TRUNCATE e.t; APPLY BATCH")
case('batch_nested', B, "BEGIN BATCH BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1); APPLY BATCH; APPLY BATCH")
case('batch_double_timestamp', B, "BEGIN BATCH USING TIMESTAMP 1 INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP 2; APPLY BATCH")
case('batch_ttl_on_batch', B, "BEGIN BATCH USING TTL 10 INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH")
case('batch_empty', B, "BEGIN BATCH APPLY BATCH")
case('batch_no_semicolons', B, "BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1) INSERT INTO e.t (pk, ck) VALUES (1, 2) APPLY BATCH")
case('batch_unknown_table', B, "BEGIN BATCH INSERT INTO e.nope (pk) VALUES (1); APPLY BATCH")
case('batch_missing_apply', B, "BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1);")
case('batch_consistency_serial', B, "BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1); APPLY BATCH", consistency='SERIAL')

D = 'ddl'
case('keyspace_exists', D, "CREATE KEYSPACE e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
case('keyspace_exists_if_not_exists', D, "CREATE KEYSPACE IF NOT EXISTS e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
case('table_exists', D, "CREATE TABLE e.t (pk int PRIMARY KEY)")
case('table_exists_if_not_exists', D, "CREATE TABLE IF NOT EXISTS e.t (pk int PRIMARY KEY)")
case('index_exists_same_name', D, "CREATE INDEX ix_v ON e.ix (v)")
case('index_exists_other_name', D, "CREATE INDEX ix_v2 ON e.ix (v)")
case('index_exists_if_not_exists', D, "CREATE INDEX IF NOT EXISTS ix_v ON e.ix (v)")
case('index_name_clashes_table', D, "CREATE INDEX t ON e.ix (ck)")
case('type_exists', D, "CREATE TYPE e.ty (x int)")
case('ks_unknown_dc', D, "CREATE KEYSPACE x1 WITH replication = {'class': 'NetworkTopologyStrategy', 'nodc': 1}")
case('ks_simple_missing_rf', D, "CREATE KEYSPACE x2 WITH replication = {'class': 'SimpleStrategy'}")
case('ks_unknown_class', D, "CREATE KEYSPACE x3 WITH replication = {'class': 'FooStrategy'}")
case('ks_local_strategy', D, "CREATE KEYSPACE x3b WITH replication = {'class': 'LocalStrategy'}")
case('ks_everywhere_strategy', D, "CREATE KEYSPACE x3c WITH replication = {'class': 'EverywhereStrategy'}")
case('ks_bad_rf', D, "CREATE KEYSPACE x4 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 'abc'}")
case('ks_negative_rf', D, "CREATE KEYSPACE x4b WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': -1}")
case('ks_zero_rf', D, "CREATE KEYSPACE x4c WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 0}")
case('ks_transient_rf', D, "CREATE KEYSPACE x4d WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': '3/1'}")
case('ks_no_replication', D, "CREATE KEYSPACE x5")
case('ks_no_class', D, "CREATE KEYSPACE x6 WITH replication = {'replication_factor': 1}")
case('ks_unknown_option', D, "CREATE KEYSPACE x7 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND nope = 1")
case('ks_rf_too_high', D, "CREATE KEYSPACE x8 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}")
case('ks_tablets_option', D, "CREATE KEYSPACE x9 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'enabled': false}")
case('ks_tablets_initial', D, "CREATE KEYSPACE x9b WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'initial': 8}")
case('ks_nts_rf_option', D, "CREATE KEYSPACE x10 WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")
case('ks_durable_writes_string', D, "CREATE KEYSPACE x11 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND durable_writes = 'maybe'")
case('ks_name_too_long', D, "CREATE KEYSPACE " + 'k' * 60 + " WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
case('ks_name_invalid', D, "CREATE KEYSPACE \"bad-name\" WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
case('ks_replication_not_map', D, "CREATE KEYSPACE x12 WITH replication = 'abc'")
case('ks_full_class_name', D, "CREATE KEYSPACE x13 WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': 1}")
case('ks_alter_class_change', D, "ALTER KEYSPACE e WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
case('ks_alter_rf_jump', D, "ALTER KEYSPACE e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}")
case('ks_alter_tablets', D, "ALTER KEYSPACE e WITH tablets = {'enabled': false}")
case('table_unknown_property', D, "CREATE TABLE e.x1 (pk int PRIMARY KEY) WITH nope = 1")
case('table_bad_compaction', D, "CREATE TABLE e.x2 (pk int PRIMARY KEY) WITH compaction = {'class': 'Nope'}")
case('table_compaction_no_class', D, "CREATE TABLE e.x2b (pk int PRIMARY KEY) WITH compaction = {'min_threshold': '2'}")
case('table_compaction_bad_option', D, "CREATE TABLE e.x2c (pk int PRIMARY KEY) WITH compaction = {'class': 'SizeTieredCompactionStrategy', 'nope': '1'}")
case('table_compaction_bad_threshold', D, "CREATE TABLE e.x2d (pk int PRIMARY KEY) WITH compaction = {'class': 'SizeTieredCompactionStrategy', 'min_threshold': '1'}")
case('table_twcs_bad_unit', D, "CREATE TABLE e.x2e (pk int PRIMARY KEY) WITH compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_unit': 'WEEKS'}")
case('table_negative_gc_grace', D, "CREATE TABLE e.x3 (pk int PRIMARY KEY) WITH gc_grace_seconds = -1")
case('table_negative_ttl', D, "CREATE TABLE e.x3b (pk int PRIMARY KEY) WITH default_time_to_live = -1")
case('table_ttl_too_large', D, "CREATE TABLE e.x3c (pk int PRIMARY KEY) WITH default_time_to_live = 999999999")
case('table_compression_class', D, "CREATE TABLE e.x4 (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor'}")
case('table_compression_sstable_compression', D, "CREATE TABLE e.x4b (pk int PRIMARY KEY) WITH compression = {'sstable_compression': 'LZ4Compressor'}")
case('table_compression_unknown', D, "CREATE TABLE e.x4c (pk int PRIMARY KEY) WITH compression = {'class': 'NopeCompressor'}")
case('table_compression_bad_chunk', D, "CREATE TABLE e.x4d (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor', 'chunk_length_in_kb': '3'}")
case('table_compression_unknown_option', D, "CREATE TABLE e.x4e (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor', 'nope': '1'}")
case('table_speculative_retry_95p', D, "CREATE TABLE e.x5 (pk int PRIMARY KEY) WITH speculative_retry = '95p'")
case('table_speculative_retry_bad', D, "CREATE TABLE e.x5b (pk int PRIMARY KEY) WITH speculative_retry = 'sometimes'")
case('table_speculative_retry_ms', D, "CREATE TABLE e.x5c (pk int PRIMARY KEY) WITH speculative_retry = '50ms'")
case('table_bad_caching', D, "CREATE TABLE e.x6 (pk int PRIMARY KEY) WITH caching = {'keys': 'SOME'}")
case('table_caching_rows_bad', D, "CREATE TABLE e.x6b (pk int PRIMARY KEY) WITH caching = {'keys': 'ALL', 'rows_per_partition': 'MANY'}")
case('table_bloom_out_of_range', D, "CREATE TABLE e.x7 (pk int PRIMARY KEY) WITH bloom_filter_fp_chance = 2")
case('table_bloom_zero', D, "CREATE TABLE e.x7b (pk int PRIMARY KEY) WITH bloom_filter_fp_chance = 0")
case('table_min_index_interval_bad', D, "CREATE TABLE e.x7c (pk int PRIMARY KEY) WITH min_index_interval = 0")
case('table_crc_check_chance_bad', D, "CREATE TABLE e.x7d (pk int PRIMARY KEY) WITH crc_check_chance = 2")
case('table_comment_number', D, "CREATE TABLE e.x7e (pk int PRIMARY KEY) WITH comment = 5")
case('table_property_twice', D, "CREATE TABLE e.x7f (pk int PRIMARY KEY) WITH comment = 'a' AND comment = 'b'")
case('table_no_pk', D, "CREATE TABLE e.x8 (pk int, v int)")
case('table_two_pk_decls', D, "CREATE TABLE e.x9 (pk int PRIMARY KEY, v int, PRIMARY KEY (v))")
case('table_pk_unknown_column', D, "CREATE TABLE e.x9b (pk int, PRIMARY KEY (nope))")
case('table_pk_repeated', D, "CREATE TABLE e.x9c (pk int, ck int, PRIMARY KEY (pk, pk))")
case('table_duplicate_column', D, "CREATE TABLE e.x10 (pk int PRIMARY KEY, pk text)")
case('table_static_no_clustering', D, "CREATE TABLE e.x11 (pk int PRIMARY KEY, v int static)")
case('table_static_pk', D, "CREATE TABLE e.x11b (pk int static, ck int, PRIMARY KEY (pk, ck))")
case('table_nonfrozen_collection_pk', D, "CREATE TABLE e.x12 (pk list<int> PRIMARY KEY)")
case('table_nonfrozen_collection_ck', D, "CREATE TABLE e.x12b (pk int, ck set<int>, PRIMARY KEY (pk, ck))")
case('table_nonfrozen_udt_pk', D, "CREATE TABLE e.x12c (pk ty PRIMARY KEY)")
case('table_nonfrozen_nested_collection', D, "CREATE TABLE e.x12d (pk int PRIMARY KEY, v list<list<int>>)")
case('table_order_by_pk', D, "CREATE TABLE e.x13 (pk int PRIMARY KEY) WITH CLUSTERING ORDER BY (pk DESC)")
case('table_order_by_wrong_order', D, "CREATE TABLE e.x13b (pk int, a int, b int, PRIMARY KEY (pk, a, b)) WITH CLUSTERING ORDER BY (b DESC, a ASC)")
case('table_order_by_missing_prefix', D, "CREATE TABLE e.x13c (pk int, a int, b int, PRIMARY KEY (pk, a, b)) WITH CLUSTERING ORDER BY (b DESC)")
case('table_unknown_type', D, "CREATE TABLE e.x14 (pk int PRIMARY KEY, v nosuchtype)")
case('table_udt_other_keyspace', D, "CREATE TABLE e.x14b (pk int PRIMARY KEY, v system.ty)")
case('table_compact_storage', D, "CREATE TABLE e.x15 (pk int PRIMARY KEY, v int) WITH COMPACT STORAGE")
case('table_cdc_option_map', D, "CREATE TABLE e.x16 (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true}")
case('table_cdc_option_bool', D, "CREATE TABLE e.x16b (pk int PRIMARY KEY, v int) WITH cdc = true")
case('table_unknown_keyspace', D, "CREATE TABLE nope.x (pk int PRIMARY KEY)")
case('table_name_too_long', D, "CREATE TABLE e." + "a" * 300 + " (pk int PRIMARY KEY)")
case('table_name_48', D, "CREATE TABLE e." + "a" * 49 + " (pk int PRIMARY KEY)")
case('table_vector_bad_dim', D, "CREATE TABLE e.x17 (pk int PRIMARY KEY, v vector<float, 0>)")
case('table_vector_non_frozen', D, "CREATE TABLE e.x17b (pk int PRIMARY KEY, v vector<list<int>, 2>)")
case('table_counter_and_ttl', D, "CREATE TABLE e.x18 (pk int PRIMARY KEY, c counter) WITH default_time_to_live = 5")
case('table_empty_columns', D, "CREATE TABLE e.x19 ()")
case('table_with_id_dup', D, "CREATE TABLE e.x20 (pk int PRIMARY KEY) WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18")
case('table_in_memory_scylla', D, "CREATE TABLE e.x21 (pk int PRIMARY KEY) WITH in_memory = true")
case('table_per_partition_rate_limit', D, "CREATE TABLE e.x22 (pk int PRIMARY KEY, v int) WITH per_partition_rate_limit = {'max_writes_per_second': 1}")
case('table_synchronous_updates', D, "CREATE TABLE e.x23 (pk int PRIMARY KEY) WITH synchronous_updates = true")
case('alter_add_existing', D, "ALTER TABLE e.t ADD v int")
case('alter_add_if_not_exists', D, "ALTER TABLE e.t ADD IF NOT EXISTS v int")
case('alter_add_static_no_ck', D, "ALTER TABLE e.c ADD s int static")
case('alter_add_counter_to_regular', D, "ALTER TABLE e.t ADD cc counter")
case('alter_add_multiple', D, "ALTER TABLE e.t ADD (n1 int, n2 text)")
case('alter_drop_pk', D, "ALTER TABLE e.t DROP pk")
case('alter_drop_ck', D, "ALTER TABLE e.t DROP ck")
case('alter_drop_indexed', D, "ALTER TABLE e.ix DROP v")
case('alter_drop_if_exists', D, "ALTER TABLE e.t DROP IF EXISTS nope")
case('alter_drop_with_timestamp', D, "ALTER TABLE e.t DROP n1 USING TIMESTAMP 1")
case('alter_type_change', D, "ALTER TABLE e.t ALTER v TYPE text")
case('alter_type_compatible', D, "ALTER TABLE e.t ALTER txt TYPE blob")
case('alter_rename_regular', D, "ALTER TABLE e.t RENAME v TO w")
case('alter_rename_to_existing', D, "ALTER TABLE e.t RENAME ck TO v")
case('alter_rename_pk_ok', D, "ALTER TABLE e.t2r RENAME a TO a2")
case('alter_unknown_table', D, "ALTER TABLE e.nope ADD x int")
case('alter_drop_unknown_column', D, "ALTER TABLE e.t DROP nope")
case('alter_compact_storage_drop', D, "ALTER TABLE e.t DROP COMPACT STORAGE")
case('alter_unknown_option', D, "ALTER TABLE e.t WITH nope = 1")
case('alter_set_id', D, "ALTER TABLE e.t WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18")
case('alter_counter_ttl', D, "ALTER TABLE e.c WITH default_time_to_live = 10")
case('alter_unknown_keyspace', D, "ALTER KEYSPACE nope WITH durable_writes = true")
case('alter_type_add_existing', D, "ALTER TYPE e.ty ADD x int")
case('alter_type_rename_unknown', D, "ALTER TYPE e.ty RENAME nope TO z")
case('alter_type_unknown', D, "ALTER TYPE e.nope ADD z int")
case('alter_type_change_field', D, "ALTER TYPE e.ty ALTER x TYPE text")
case('alter_mv_unknown', D, "ALTER MATERIALIZED VIEW e.nope WITH comment = 'x'")
case('drop_unknown_table', D, "DROP TABLE e.nope")
case('drop_unknown_table_if_exists', D, "DROP TABLE IF EXISTS e.nope")
case('drop_unknown_keyspace', D, "DROP KEYSPACE nope")
case('drop_unknown_index', D, "DROP INDEX e.nope")
case('drop_unknown_type', D, "DROP TYPE e.nope")
case('drop_unknown_function', D, "DROP FUNCTION e.nope")
case('drop_unknown_aggregate', D, "DROP AGGREGATE e.nope")
case('drop_unknown_view', D, "DROP MATERIALIZED VIEW e.nope")
case('drop_unknown_trigger', D, "DROP TRIGGER nope ON e.t")
case('drop_type_in_use', D, "DROP TYPE e.ty")
case('drop_table_as_view', D, "DROP TABLE e.ix_v_index")
case('use_unknown_keyspace', D, "USE nope")
case('use_ok', D, "USE e")
case('truncate_unknown', D, "TRUNCATE e.nope")
case('truncate_view', D, "TRUNCATE e.ix_v_index")
case('trigger_create', D, "CREATE TRIGGER tr ON e.t USING 'org.example.Trigger'")
case('udf_disabled', D, "CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'")
case('udf_lua_disabled', D, "CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'")
case('udf_unknown_language', D, "CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE cobol AS 'x'")
case('uda_unknown_sfunc', D, "CREATE AGGREGATE e.a(int) SFUNC nope STYPE int")

V = 'views-indexes'
case('mv_missing_is_not_null', V, "CREATE MATERIALIZED VIEW e.mv1 AS SELECT * FROM e.ix WHERE v IS NOT NULL PRIMARY KEY (v, pk, ck)")
case('mv_on_counter', V, "CREATE MATERIALIZED VIEW e.mv2 AS SELECT * FROM e.c WHERE pk IS NOT NULL AND cnt IS NOT NULL PRIMARY KEY (cnt, pk)")
case('mv_missing_base_pk', V, "CREATE MATERIALIZED VIEW e.mv3 AS SELECT pk, ck, v FROM e.ix WHERE v IS NOT NULL AND pk IS NOT NULL PRIMARY KEY (v, pk)")
case('mv_static', V, "CREATE MATERIALIZED VIEW e.mv4 AS SELECT pk, ck, s FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)")
case('mv_select_star_static', V, "CREATE MATERIALIZED VIEW e.mv4b AS SELECT * FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)")
case('mv_two_regular_in_pk', V, "CREATE MATERIALIZED VIEW e.mv5 AS SELECT * FROM e.t2 WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL AND v IS NOT NULL PRIMARY KEY ((v, d), a, b, c)")
case('mv_collection_in_pk', V, "CREATE MATERIALIZED VIEW e.mv6 AS SELECT pk, ck, fl FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL AND fl IS NOT NULL PRIMARY KEY (fl, pk, ck)")
case('mv_unknown_base', V, "CREATE MATERIALIZED VIEW e.mv7 AS SELECT * FROM e.nope WHERE pk IS NOT NULL PRIMARY KEY (pk)")
case('mv_ttl_option', V, "CREATE MATERIALIZED VIEW e.mv9 AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH default_time_to_live = 5")
case('mv_filter_regular', V, "CREATE MATERIALIZED VIEW e.mv10 AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL AND v = 1 PRIMARY KEY (ck, pk)")
case('mv_ok', V, "CREATE MATERIALIZED VIEW e.mvok AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)")
case('mv_write_to_view', V, "INSERT INTO e.mvok (ck, pk, v) VALUES (1, 1, 1)")
case('mv_drop_base_with_view', V, "DROP TABLE e.ix")
case('mv_drop_view_as_table', V, "DROP TABLE e.mvok")
case('mv_alter_view_add', V, "ALTER TABLE e.mvok ADD z int")
case('mv_index_on_view', V, "CREATE INDEX ON e.mvok (v)")
case('mv_drop_base_column', V, "ALTER TABLE e.ix DROP v")
case('index_on_only_pk', V, "CREATE INDEX ON e.vec (pk)")
case('index_on_counter', V, "CREATE INDEX ON e.c (cnt)")
case('index_keys_on_list', V, "CREATE INDEX ON e.t (KEYS(l))")
case('index_entries_on_set', V, "CREATE INDEX ON e.t (ENTRIES(st))")
case('index_full_on_nonfrozen', V, "CREATE INDEX ON e.t (FULL(l))")
case('index_values_on_frozen', V, "CREATE INDEX ON e.t (VALUES(fl))")
case('index_on_duration', V, "CREATE INDEX ON e.u (du)")
case('index_unknown_column', V, "CREATE INDEX ON e.t (nope)")
case('index_on_static', V, "CREATE INDEX ON e.t (s)")
case('index_on_whole_pk_component', V, "CREATE INDEX ON e.t2 (a)")
case('index_custom_unknown_class', V, "CREATE CUSTOM INDEX ON e.t (txt) USING 'com.example.Nope'")
case('index_custom_no_using', V, "CREATE CUSTOM INDEX ON e.t (txt)")
case('index_options_without_custom', V, "CREATE INDEX ON e.t (txt) WITH OPTIONS = {'a': 'b'}")
case('sai_on_regular', V, "CREATE INDEX ON e.t2 (v) USING 'sai'")
case('sai_on_collection', V, "CREATE INDEX ON e.t (l) USING 'sai'")
case('sai_unknown_option', V, "CREATE INDEX ON e.t2 (v) USING 'sai' WITH OPTIONS = {'nope': '1'}")
case('sai_on_vector_ok', V, "CREATE INDEX vec_sai ON e.vec (emb) USING 'sai'")
case('sai_on_vector_bad_similarity', V, "CREATE INDEX vec_sai2 ON e.vec (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'NOPE'}")
case('vector_index_scylla_ok', V, "CREATE CUSTOM INDEX vec_vs ON e.vec (emb) USING 'vector_index'")
case('vector_index_scylla_bad_option', V, "CREATE CUSTOM INDEX vec_vs2 ON e.vec (emb) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'NOPE'}")
case('ann_with_index', V, "SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3] LIMIT 1")
case('ann_with_index_no_limit', V, "SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3]")
case('ann_with_filter', V, "SELECT * FROM e.vec WHERE pk = 1 ORDER BY emb ANN OF [1, 2, 3] LIMIT 1")
case('sasi_disabled', V, "CREATE CUSTOM INDEX ON e.t (txt) USING 'org.apache.cassandra.index.sasi.SASIIndex'")
case('drop_index_on_view_name', V, "DROP INDEX e.mvok")

A = 'auth-off'
case('grant_allowall', A, "GRANT SELECT ON e.t TO nope")
case('revoke_allowall', A, "REVOKE SELECT ON e.t FROM nope")
case('create_role_allowall', A, "CREATE ROLE r1 WITH PASSWORD = 'x' AND LOGIN = true")
case('create_role_nopw_allowall', A, "CREATE ROLE r2")
case('create_user_allowall', A, "CREATE USER u1 WITH PASSWORD 'x'")
case('alter_role_allowall', A, "ALTER ROLE cassandra WITH PASSWORD = 'x'")
case('drop_role_allowall', A, "DROP ROLE nope")
case('list_roles_allowall', A, "LIST ROLES")
case('list_permissions_allowall', A, "LIST ALL PERMISSIONS")
case('list_users_allowall', A, "LIST USERS")
case('grant_role_allowall', A, "GRANT r1 TO r2")
case('service_level_create', A, "CREATE SERVICE LEVEL sl1 WITH timeout = 10s")
case('service_level_list', A, "LIST ALL SERVICE LEVELS")
case('attach_service_level', A, "ATTACH SERVICE LEVEL sl1 TO cassandra")
case('describe_passwords_anon', A, "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS")

S = 'syntax'
for sid, q in [('typo_keyword', "SELEC * FROM e.t"), ('where_eof', "SELECT * FROM e.t WHERE"),
               ('eq_eof', "SELECT * FROM e.t WHERE pk ="), ('limit_string', "SELECT * FROM e.t LIMIT 'a'"),
               ('trailing_comma_ddl', "CREATE TABLE e.x30 (pk int PRIMARY KEY,)"), ('allow_eof', "SELECT * FROM e.t WHERE pk = 1 ALLOW"),
               ('double_semicolon', "SELECT * FROM e.t;;"), ('semicolon_only', ";"), ('unterminated_string', "SELECT * FROM e.t WHERE txt = 'abc"),
               ('unterminated_quoted_ident', 'SELECT * FROM e."t'), ('unterminated_dollar_string', "SELECT * FROM e.t WHERE txt = $$abc"),
               ('and_and', "SELECT * FROM e.t WHERE pk = 1 AND AND ck = 1"), ('quoted_unknown_table', 'SELECT * FROM e."T"'),
               ('empty', ""), ('whitespace_only', "   "), ('comment_only', "-- just a comment"),
               ('bad_hex', "SELECT * FROM e.t WHERE pk = 0x"), ('bad_float', "SELECT * FROM e.t WHERE pk = 1e"),
               ('reserved_table_name', "CREATE TABLE e.select (a int PRIMARY KEY)"), ('reserved_column_name', "CREATE TABLE e.x31 (from int PRIMARY KEY)"),
               ('unreserved_keyword_ok', "CREATE TABLE e.x32 (key int PRIMARY KEY, type text, ttl int)"),
               ('unknown_statement', "FROBNICATE e.t"), ('missing_from', "SELECT * e.t"), ('garbage_after', "SELECT * FROM e.t garbage"),
               ('bad_collection_literal', "INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [1, 2)"),
               ('nested_unterminated_comment', "SELECT * FROM e.t /* unterminated"),
               ('non_ascii_identifier', "SELECT * FROM e.tä"), ('select_star_and_column', "SELECT *, v FROM e.t"),
               ('insert_no_values', "INSERT INTO e.t (pk, ck)"), ('update_no_set', "UPDATE e.t WHERE pk = 1"),
               ('delete_no_where', "DELETE FROM e.t"), ('update_no_where', "UPDATE e.t SET v = 1"),
               ('create_table_no_columns_parens', "CREATE TABLE e.x33"), ('create_keyspace_with_eof', "CREATE KEYSPACE x WITH"),
               ('in_without_parens', "SELECT * FROM e.t WHERE pk IN 1"), ('double_where', "SELECT * FROM e.t WHERE pk = 1 WHERE ck = 1"),
               ('limit_before_where', "SELECT * FROM e.t LIMIT 1 WHERE pk = 1"), ('order_without_by', "SELECT * FROM e.t WHERE pk = 1 ORDER ck"),
               ('integer_too_large_literal', "SELECT * FROM e.t WHERE pk = 99999999999999999999999999"),
               ('negative_hex', "SELECT * FROM e.t WHERE pk = -0x01"), ('uuid_malformed_literal', "SELECT * FROM e.u WHERE id = 1 AND u = 123e4567-e89b-12d3-a456"),
               ('duration_literal_bad', "INSERT INTO e.u (id, du) VALUES (1, 1hx)"), ('lowercase_keywords', "select * from e.t where pk = 1"),
               ('mixed_case_table', "SELECT * FROM E.T"), ('colon_named_and_positional', "SELECT * FROM e.t WHERE pk = ? AND ck = :c"),
               ('long_identifier', "CREATE TABLE e." + "a" * 300 + " (pk int PRIMARY KEY)"),
               ('very_long_query', "SELECT * FROM e.t WHERE pk IN (" + ",".join(['1'] * 5000) + ")"),
               ('deep_nesting', "SELECT * FROM e.t WHERE pk = " + "(" * 200 + "1" + ")" * 200),
               ('grant_bad_permission', "GRANT NOPE ON KEYSPACE e TO r"), ('use_quoted', 'USE "E"')]:
    case(sid, S, q)

WN = 'warning'
case('agg_no_pk', WN, "SELECT count(*) FROM e.t")
case('agg_with_pk', WN, "SELECT count(*) FROM e.t WHERE pk = 1")
case('agg_with_in_pk', WN, "SELECT count(*) FROM e.t WHERE pk IN (1, 2)")
case('agg_group_by_pk', WN, "SELECT pk, count(*) FROM e.t GROUP BY pk")
case('unlogged_multi_partition_11', WN, "BEGIN UNLOGGED BATCH " + " ".join("INSERT INTO e.t (pk, ck, v) VALUES (%d, 1, 1);" % i for i in range(11)) + " APPLY BATCH")
case('unlogged_multi_partition_10', WN, "BEGIN UNLOGGED BATCH " + " ".join("INSERT INTO e.t (pk, ck, v) VALUES (%d, 1, 1);" % i for i in range(10)) + " APPLY BATCH")
case('logged_multi_partition_12', WN, "BEGIN BATCH " + " ".join("INSERT INTO e.t (pk, ck, v) VALUES (%d, 1, 1);" % i for i in range(12)) + " APPLY BATCH")
def batch(n_kb, logged=True, pk=1):
    val = 'x' * 1024
    return ("BEGIN %sBATCH " % ('' if logged else 'UNLOGGED ')) + " ".join("INSERT INTO e.big (pk, ck, v) VALUES (%d, %d, '%s');" % (pk, i, val) for i in range(n_kb)) + " APPLY BATCH"
for kb in (4, 6, 49, 60, 129, 1100):
    RES.append(['batch_size_%dkb' % kb, WN, 'logged batch of %d inserts of a 1 KiB text value into one partition' % kb, run(batch(kb))])
RES.append(['batch_6_partitions_6kb', WN, 'logged batch, 6 x 1 KiB, 6 partitions', run("BEGIN BATCH " + " ".join("INSERT INTO e.big (pk, ck, v) VALUES (%d, 1, '%s');" % (i + 200, 'x' * 1024) for i in range(6)) + " APPLY BATCH")])
RES.append(['batch_40_partitions_40kb', WN, 'logged batch, 40 x 1 KiB, 40 partitions', run("BEGIN BATCH " + " ".join("INSERT INTO e.big (pk, ck, v) VALUES (%d, 1, '%s');" % (i + 300, 'x' * 1024) for i in range(40)) + " APPLY BATCH")])
RES.append(['unlogged_batch_size_60kb', WN, 'unlogged batch, 60 x 1 KiB, one partition', run(batch(60, False))])
RES.append(['unlogged_batch_size_60kb_multi', WN, 'unlogged batch, 60 x 1 KiB, 60 partitions', run("BEGIN UNLOGGED BATCH " + " ".join("INSERT INTO e.big (pk, ck, v) VALUES (%d, 1, '%s');" % (i + 100, 'x' * 1024) for i in range(60)) + " APPLY BATCH")])
p_del = s.prepare("DELETE FROM e.tomb WHERE pk = ? AND ck = ?")
def deletes(pk, n):
    for start in range(0, n, 200):
        b = BatchStatement(batch_type=BatchType.UNLOGGED)
        for i in range(start, min(n, start + 200)): b.add(p_del, (pk, i))
        s.execute(b)
deletes(1, 1100)
case('tombstones_1100', WN, "SELECT * FROM e.tomb WHERE pk = 1")
deletes(2, 100100)
case('tombstones_100100', WN, "SELECT * FROM e.tomb WHERE pk = 2")
case('tombstones_100100_unpaged', WN, "SELECT * FROM e.tomb WHERE pk = 2", page_size=None)
case('select_unpaged_filtering', WN, "SELECT * FROM e.t WHERE v = 1 ALLOW FILTERING", page_size=None)
case('simple_strategy_ks', WN, "CREATE KEYSPACE w1 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
case('simple_strategy_ks_vnodes', WN, "CREATE KEYSPACE w2 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1} AND tablets = {'enabled': false}")
case('mv_create', WN, "CREATE MATERIALIZED VIEW e.mvw AS SELECT * FROM e.t2 WHERE v IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL PRIMARY KEY (v, a, b, c, d)")
case('index_create', WN, "CREATE INDEX ix_w ON e.t2 (v)")
case('in_many_200', WN, "SELECT * FROM e.t WHERE pk IN (" + ",".join(str(i) for i in range(200)) + ")")
case('in_many_100', WN, "SELECT * FROM e.t WHERE pk IN (" + ",".join(str(i) for i in range(100)) + ")")
case('in_cartesian', WN, "SELECT * FROM e.t2 WHERE a IN (" + ",".join(str(i) for i in range(11)) + ") AND b IN (" + ",".join(str(i) for i in range(11)) + ")")
case('collection_large', WN, "INSERT INTO e.t (pk, ck, l) VALUES (3, 3, [" + ",".join(['1'] * 70000) + "])")
case('large_text_cell', WN, "INSERT INTO e.big (pk, ck, v) VALUES (7, 7, '" + 'y' * 1100000 + "')")
case('drop_ks_w1', WN, "DROP KEYSPACE IF EXISTS w1")
case('drop_ks_w2', WN, "DROP KEYSPACE IF EXISTS w2")
case('ks_rf3_default', WN, "CREATE KEYSPACE e3t WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}")

U = 'consistency'
case('ks_rf3_default_table', U, "CREATE TABLE e3t.t (pk int PRIMARY KEY, v int)")
RES.append(['setup', 'setup', '', [[q, run(q)] for q in [
    "CREATE KEYSPACE e3 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3} AND tablets = {'enabled': false}",
    "CREATE KEYSPACE IF NOT EXISTS e3 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}",
    "CREATE TABLE IF NOT EXISTS e3.t (pk int PRIMARY KEY, v int)"]]])
for cl in ('ONE', 'TWO', 'QUORUM', 'ALL', 'LOCAL_QUORUM', 'EACH_QUORUM', 'LOCAL_ONE', 'ANY'):
    case('rf3_write_' + cl, U, "INSERT INTO e3.t (pk, v) VALUES (1, 1)", consistency=cl)
for cl in ('ONE', 'QUORUM', 'ALL', 'LOCAL_QUORUM', 'EACH_QUORUM', 'SERIAL'):
    case('rf3_read_' + cl, U, "SELECT * FROM e3.t WHERE pk = 1", consistency=cl)
case('rf3_lwt', U, "INSERT INTO e3.t (pk, v) VALUES (2, 1) IF NOT EXISTS")
case('rf3_lwt_local_serial', U, "INSERT INTO e3.t (pk, v) VALUES (2, 1) IF NOT EXISTS", serial='LOCAL_SERIAL')
case('rf1_each_quorum_read', U, "SELECT * FROM e.t WHERE pk = 1", consistency='EACH_QUORUM')
case('rf1_any_read', U, "SELECT * FROM e.t WHERE pk = 1", consistency='ANY')
case('rf1_counter_any', U, "UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1", consistency='ANY')
case('truncate_rf3_all', U, "TRUNCATE e3.t")

F = 'function'
for fid, q in [('blobasint_wrong_length', "SELECT blobAsInt(0x0102) FROM e.t WHERE pk = 1"),
               ('blobasbigint_empty', "SELECT blobAsBigint(0x) FROM e.t WHERE pk = 1"),
               ('blobastext_invalid_utf8', "SELECT blobAsText(0xff) FROM e.t WHERE pk = 1"),
               ('blobasascii_invalid', "SELECT blobAsAscii(0xc3a9) FROM e.t WHERE pk = 1"),
               ('fromjson_select_bad', "SELECT fromJson('x') FROM e.t WHERE pk = 1"),
               ('tojson_two_args', "SELECT toJson(v, v) FROM e.t WHERE pk = 1"),
               ('mintimeuuid_text', "SELECT minTimeuuid('nope') FROM e.t WHERE pk = 1"),
               ('totimestamp_text', "SELECT toTimestamp('x') FROM e.t WHERE pk = 1"),
               ('token_select_wrong', "SELECT token(v) FROM e.t WHERE pk = 1"),
               ('ttl_two_args', "SELECT ttl(v, v) FROM e.t WHERE pk = 1"),
               ('now_with_arg', "SELECT now(1) FROM e.t WHERE pk = 1"),
               ('uuid_in_insert_ok', "INSERT INTO e.u (id, u) VALUES (2, uuid())"),
               ('now_into_uuid_ok', "INSERT INTO e.u (id, u) VALUES (3, now())"),
               ('now_into_text', "INSERT INTO e.u (id, tx) VALUES (4, now())"),
               ('currenttimestamp_into_date', "INSERT INTO e.u (id, dt) VALUES (5, currentTimestamp())"),
               ('abs_text', "SELECT abs(txt) FROM e.t WHERE pk = 1"),
               ('abs_ok', "SELECT abs(v) FROM e.t WHERE pk = 1"),
               ('map_keys', "SELECT map_keys(m) FROM e.t WHERE pk = 1"),
               ('collection_count', "SELECT collection_count(l) FROM e.t WHERE pk = 1"),
               ('mask_default', "SELECT mask_default(v) FROM e.t WHERE pk = 1"),
               ('similarity_cosine_zero', "SELECT similarity_cosine([0.0, 0.0, 0.0], [1.0, 2.0, 3.0]) FROM e.t WHERE pk = 1"),
               ('similarity_dim_mismatch', "SELECT similarity_cosine(emb, [1.0, 2.0]) FROM e.vec"),
               ('avg_overflow_int', "SELECT avg(v) FROM e.t WHERE pk = 1"),
               ('sum_overflow_setup', "INSERT INTO e.t (pk, ck, v) VALUES (20, 1, 2147483647)"),
               ('sum_overflow_setup2', "INSERT INTO e.t (pk, ck, v) VALUES (20, 2, 2147483647)"),
               ('sum_overflow', "SELECT sum(v) FROM e.t WHERE pk = 20"),
               ('cast_overflow', "SELECT CAST(v AS tinyint) FROM e.t WHERE pk = 20"),
               ('arith_overflow', "SELECT v + 1 FROM e.t WHERE pk = 20")]:
    case(fid, F, q)

R = 'rate-limit-timeout'
bulk(["CREATE TABLE IF NOT EXISTS e.rl (pk int PRIMARY KEY, v int) WITH per_partition_rate_limit = {'max_writes_per_second': 1, 'max_reads_per_second': 1}"])
rl = [run("INSERT INTO e.rl (pk, v) VALUES (1, %d)" % i) for i in range(40)]
errs = [r for r in rl if not r['ok']]
RES.append(['rate_limit_writes', R, '40 x INSERT INTO e.rl (pk, v) VALUES (1, i) with per_partition_rate_limit max_writes_per_second 1', errs[0] if errs else rl[-1]])
rr = [run("SELECT * FROM e.rl WHERE pk = 1") for i in range(40)]
errs = [r for r in rr if not r['ok']]
RES.append(['rate_limit_reads', R, '40 x SELECT * FROM e.rl WHERE pk = 1', errs[0] if errs else rr[-1]])
case('timeout_select_1us', R, "SELECT * FROM e.tomb WHERE pk = 2 USING TIMEOUT 1us")
case('timeout_insert_1us', R, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 1us")
case('timeout_negative', R, "SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT -1s")
case('timeout_zero', R, "SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT 0s")
case('timeout_and_ttl', R, "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 10s AND TTL 10")

P = 'prepared'
def prep_case(cid, q, values):
    def fn():
        p = s.prepare(q)
        return drun(s, p.bind(values), keep_rows=False)
    dcase(cid, P, '%s with %r' % (q, values), fn)
prep_case('null_pk_bound', "INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?)", [None, 1, 1])
prep_case('null_ck_bound', "INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?)", [1, None, 1])
prep_case('null_pk_bound_select', "SELECT * FROM e.t WHERE pk = ?", [None])
prep_case('null_in_list_bound', "SELECT * FROM e.t WHERE pk IN ?", [[1, None]])
prep_case('null_limit_bound', "SELECT * FROM e.t LIMIT ?", [None])
prep_case('null_ttl_bound', "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL ?", [None])
prep_case('null_timestamp_bound', "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP ?", [None])
prep_case('null_counter_increment', "UPDATE e.c SET cnt = cnt + ? WHERE pk = 1", [None])
prep_case('null_list_index', "UPDATE e.t SET l[?] = 1 WHERE pk = 1 AND ck = 1", [None])
prep_case('null_map_key_bound', "UPDATE e.t SET m[?] = 1 WHERE pk = 1 AND ck = 1", [None])
prep_case('empty_blob_pk_bound', "INSERT INTO e.blobk (pk, v) VALUES (?, 1)", [b''])
prep_case('negative_limit_bound', "SELECT * FROM e.t LIMIT ?", [-1])
prep_case('null_in_set_bound', "INSERT INTO e.t (pk, ck, st) VALUES (1, 1, ?)", [{1, None}])
def unset_case(cid, q, values):
    from cassandra.query import UNSET_VALUE
    def fn():
        p = s.prepare(q)
        return drun(s, p.bind([UNSET_VALUE if v == 'UNSET' else v for v in values]), keep_rows=False)
    dcase(cid, P, '%s with %r' % (q, values), fn)
unset_case('unset_pk', "INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?)", ['UNSET', 1, 1])
unset_case('unset_regular_ok', "INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?)", [1, 1, 'UNSET'])
unset_case('unset_where_select', "SELECT * FROM e.t WHERE pk = ?", ['UNSET'])
unset_case('unset_limit', "SELECT * FROM e.t LIMIT ?", ['UNSET'])
unset_case('unset_ttl', "INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL ?", ['UNSET'])
unset_case('unset_in_list', "SELECT * FROM e.t WHERE pk IN ?", ['UNSET'])
unset_case('unset_lwt_condition', "UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = ?", ['UNSET'])
def prep_only(cid, q):
    def fn():
        try:
            p = s.prepare(q); return {'ok': True, 'prepared': True, 'columns': [c.name for c in p.column_metadata] if p.column_metadata else [], 'bind': [c.name for c in p.bind_metadata] if hasattr(p, 'bind_metadata') else None}
        except Exception as e:
            return {'ok': False, 'error': err(e)}
    dcase(cid, P, 'PREPARE ' + q, fn)
prep_only('prepare_duplicate_named_marker', "SELECT * FROM e.t WHERE pk = :x AND ck = :x")
prep_only('prepare_marker_in_select', "SELECT ? FROM e.t")
prep_only('prepare_marker_in_table', "SELECT * FROM ?")
prep_only('prepare_marker_as_ttl_fn', "SELECT ttl(?) FROM e.t")
prep_only('prepare_use', "USE e")
prep_only('prepare_ddl', "CREATE TABLE e.px (pk int PRIMARY KEY)")
prep_only('prepare_marker_untyped_fn', "SELECT * FROM e.t WHERE pk = abs(?)")
prep_only('prepare_marker_in_collection', "INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [?, ?])")
prep_only('prepare_marker_in_map_key', "INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {?: 1})")
prep_only('prepare_marker_vector', "SELECT * FROM e.vec ORDER BY emb ANN OF ? LIMIT 1")
prep_only('prepare_too_many_markers', "SELECT * FROM e.t WHERE pk IN (" + ",".join(['?'] * 70000) + ")")
for q in ["DROP KEYSPACE IF EXISTS e3", "DROP KEYSPACE IF EXISTS e3t", "DROP KEYSPACE IF EXISTS e"]:
    run(q)
for ks in ['x1', 'x2', 'x3', 'x3b', 'x3c', 'x4', 'x4b', 'x4c', 'x4d', 'x5', 'x6', 'x7', 'x8', 'x9', 'x9b', 'x10', 'x11', 'x12', 'x13', 'w1', 'w2', 'k' * 60]:
    run('DROP KEYSPACE IF EXISTS ' + ks)
json.dump(RES, open(out, 'w'), indent=1, default=str)
print('done', len(RES))
