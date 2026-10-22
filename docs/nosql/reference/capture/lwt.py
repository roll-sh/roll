# Lightweight transaction capture: conditional statements, batches, consistencies, bound values
# and SERIAL reads, run in order on both references. Writes JSON (stdout): a list of steps with
# the canonical result text of each dialect (the form of crates/nosql-engine's
# `captured_lwt_harness`), turned into Rust by crates/nosql-engine/scripts/gen_lwt.py.
#
# Runs inside the Cassandra reference container with the Python driver cqlsh bundles, so no new
# container is needed:
#   docker exec -i nosql-ref-cassandra python3 - <scylla-ip> < lwt.py > ~/.cache/roll-nosql/capture/out/lwt.json
# where <scylla-ip> is the bridge address of nosql-ref-scylla (docker inspect).
import sys, glob, re, json
sys.path[:0] = [z for z in glob.glob('/opt/cassandra/lib/*.zip') if 'driver' not in z] + \
    ['/opt/cassandra/lib/cassandra-driver-internal-only-3.29.0.zip/cassandra-driver-3.29.0']
from cassandra.cluster import Cluster, ExecutionProfile, EXEC_PROFILE_DEFAULT
from cassandra.policies import WhiteListRoundRobinPolicy
from cassandra.query import SimpleStatement, BatchStatement, BatchType, tuple_factory, UNSET_VALUE as U
from cassandra import ConsistencyLevel as CL

K = 'lwt_c'
T = K + '.t'
S = []  # (id, cql, opts); id 'setup' runs without recording

def setup(*qs):
    for q in qs: S.append(('setup', q, {}))

def q(cid, cql, **opts):
    S.append((cid, cql, opts))

# Plain writes use an old explicit timestamp: Cassandra's ballots (milliseconds x 1000) would
# lose to the driver's microsecond timestamp of a plain write in the same millisecond.
setup(
    "DROP KEYSPACE IF EXISTS lwt_c",
    "CREATE KEYSPACE lwt_c WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}",
    "CREATE TYPE lwt_c.addr (street text, zip int)",
    "CREATE TABLE lwt_c.t (pk int, ck int, s int static, v int, txt text, l list<int>, st set<int>, m map<text, int>, fl frozen<list<int>>, fs frozen<set<int>>, fm frozen<map<text, int>>, u addr, fu frozen<addr>, du duration, tu tuple<int, text>, PRIMARY KEY (pk, ck))",
    "CREATE TABLE lwt_c.k (p int PRIMARY KEY, v int, txt text, l list<int>, m map<text, int>)",
    "CREATE TABLE lwt_c.c (pk int PRIMARY KEY, cnt counter)",
    "CREATE TABLE lwt_c.n (pk int PRIMARY KEY, ti tinyint, si smallint, i int, bi bigint, vi varint, de decimal, f float, d double, txt text, a ascii, b blob, ts timestamp, du duration, bo boolean)",
    "CREATE TABLE lwt_c.r (p int, c int, s int static, r int, PRIMARY KEY (p, c))",
    "CREATE TABLE lwt_c.o (p int, a int, b int, v int, PRIMARY KEY (p, a, b))",
    "CREATE TABLE lwt_c.w (p int, c int, v int, l list<int>, PRIMARY KEY (p, c))",
)

# ---- INSERT IF NOT EXISTS, UPDATE / DELETE IF EXISTS ------------------------------------------
q('ins_applied', "INSERT INTO lwt_c.t (pk, ck, v) VALUES (1, 1, 1) IF NOT EXISTS")
q('ins_not_applied', "INSERT INTO lwt_c.t (pk, ck, v) VALUES (1, 1, 2) IF NOT EXISTS")
q('ins_select', "SELECT pk, ck, v FROM lwt_c.t WHERE pk = 1")
q('ins_null_regular', "INSERT INTO lwt_c.r (p, c, r) VALUES (3, 1, null) IF NOT EXISTS")
q('ins_null_regular_again', "INSERT INTO lwt_c.r (p, c, r) VALUES (3, 1, null) IF NOT EXISTS")
q('ins_ttl', "INSERT INTO lwt_c.k (p, v) VALUES (1, 1) IF NOT EXISTS USING TTL 1000")
q('ins_ttl_select', "SELECT p, v FROM lwt_c.k WHERE p = 1")
q('upd_exists', "UPDATE lwt_c.t SET v = 2 WHERE pk = 1 AND ck = 1 IF EXISTS")
q('upd_exists_missing', "UPDATE lwt_c.t SET v = 2 WHERE pk = 1 AND ck = 9 IF EXISTS")
q('upd_exists_missing_partition', "UPDATE lwt_c.t SET v = 2 WHERE pk = 99 AND ck = 9 IF EXISTS")
q('del_exists', "DELETE FROM lwt_c.k WHERE p = 1 IF EXISTS")
q('del_exists_again', "DELETE FROM lwt_c.k WHERE p = 1 IF EXISTS")
q('del_col_exists', "DELETE v FROM lwt_c.t WHERE pk = 1 AND ck = 1 IF EXISTS")
q('del_col_select', "SELECT pk, ck, v FROM lwt_c.t WHERE pk = 1")
q('upd_ttl', "UPDATE lwt_c.t USING TTL 500 SET v = 3 WHERE pk = 1 AND ck = 1 IF v = null")
q('upd_timeout', "UPDATE lwt_c.t USING TIMEOUT 5s SET v = 4 WHERE pk = 1 AND ck = 1 IF v = 3")
q('multi_ck_exists', "INSERT INTO lwt_c.o (p, a, b, v) VALUES (1, 1, 1, 1) IF NOT EXISTS")
q('multi_ck_cond', "UPDATE lwt_c.o SET v = 2 WHERE p = 1 AND a = 1 AND b = 1 IF v = 1")
q('multi_ck_partial', "UPDATE lwt_c.o SET v = 2 WHERE p = 1 AND a = 1 IF v = 1")
q('multi_ck_tuple', "UPDATE lwt_c.o SET v = 3 WHERE p = 1 AND (a, b) = (1, 1) IF v = 2")

# ---- validation ---------------------------------------------------------------------------------
q('cond_pk', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF pk = 1")
q('cond_ck', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF ck = 1")
q('cond_pk_bad_value', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF pk = 'a'")
q('cond_unknown', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF nope = 1")
q('cond_order_set', "UPDATE lwt_c.t SET nope = 1 WHERE pk = 1 AND ck = 1 IF nope2 = 1")
q('cond_order_where', "UPDATE lwt_c.t SET v = 1 WHERE nope = 1 IF nope2 = 1")
q('cond_order_set_value', "UPDATE lwt_c.t SET v = 'a' WHERE pk = 1 AND ck = 1 IF v = 'b'")
q('cond_order_where_value', "UPDATE lwt_c.t SET v = 1 WHERE pk = 'a' AND ck = 1 IF v = 'b'")
q('cond_order_delete', "DELETE nope FROM lwt_c.t WHERE pk = 1 AND ck = 1 IF nope2 = 1")
q('cond_timestamp', "UPDATE lwt_c.t USING TIMESTAMP 5 SET v = 3 WHERE pk = 1 AND ck = 1 IF v = 1")
q('cond_timestamp_exists', "UPDATE lwt_c.t USING TIMESTAMP 5 SET v = 3 WHERE pk = 1 AND ck = 1 IF EXISTS")
q('cond_timestamp_insert', "INSERT INTO lwt_c.t (pk, ck, v) VALUES (1, 1, 1) IF NOT EXISTS USING TIMESTAMP 5")
q('cond_timestamp_delete', "DELETE FROM lwt_c.t USING TIMESTAMP 5 WHERE pk = 1 AND ck = 1 IF EXISTS")
q('in_pk', "UPDATE lwt_c.t SET v = 3 WHERE pk IN (1, 2) AND ck = 1 IF v = 1")
q('in_pk_one', "UPDATE lwt_c.t SET v = 3 WHERE pk IN (1) AND ck = 1 IF v = 1")
q('in_ck', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck IN (1, 2) IF v = 1")
q('in_pk_delete', "DELETE FROM lwt_c.k WHERE p IN (1, 2) IF EXISTS")
q('in_ck_delete', "DELETE FROM lwt_c.t WHERE pk = 1 AND ck IN (1, 2) IF EXISTS")
q('del_range_cond', "DELETE FROM lwt_c.t WHERE pk = 1 AND ck > 1 IF v = 1")
q('del_partition_cond', "DELETE FROM lwt_c.t WHERE pk = 1 IF v = 1")
q('del_partition_exists', "DELETE FROM lwt_c.t WHERE pk = 1 IF EXISTS")
q('del_partition_static_cond', "DELETE FROM lwt_c.t WHERE pk = 1 IF s = 1")
q('del_static_regular_cond', "DELETE s FROM lwt_c.r WHERE p = 1 IF r = 1")
q('del_static_with_ck', "DELETE s FROM lwt_c.r WHERE p = 1 AND c = 1 IF s = 1")
q('upd_static_with_ck_static_cond', "UPDATE lwt_c.r SET s = 1 WHERE p = 1 AND c = 1 IF s = 1")
q('upd_static_with_ck_exists', "UPDATE lwt_c.r SET s = 1 WHERE p = 1 AND c = 1 IF EXISTS")
q('upd_regular_without_ck', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 IF s = 7")
q('counter_cond', "UPDATE lwt_c.c SET cnt = cnt + 1 WHERE pk = 1 IF cnt = 1")
q('counter_exists', "UPDATE lwt_c.c SET cnt = cnt + 1 WHERE pk = 1 IF EXISTS")
q('counter_delete_exists', "DELETE FROM lwt_c.c WHERE pk = 1 IF EXISTS")
q('counter_delete_cond', "DELETE FROM lwt_c.c WHERE pk = 1 IF cnt = 1")
q('counter_insert', "INSERT INTO lwt_c.c (pk, cnt) VALUES (1, 1) IF NOT EXISTS")
q('sub_set', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF st[1] = 1")
q('sub_non_collection', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF v[0] = 1")
q('field_non_udt', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF v.x = 1")
q('field_non_udt_list', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF fl.x = 1")
q('field_unknown', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF u.nope = 7")
q('contains_non_collection', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF v CONTAINS 1")
q('contains_key_list', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF l CONTAINS KEY 1")
q('like_int', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF v LIKE 'a'")
q('like_int_value', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF txt LIKE 1")
q('like_frozen', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF fl LIKE 'a'")
q('duration_slice', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF du > 1d")
q('duration_eq', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF du = 1d")
q('null_slice', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF v > null")
q('exists_and_cond', "UPDATE lwt_c.t SET v = 3 WHERE pk = 1 AND ck = 1 IF EXISTS AND v = 2")
q('insert_if_exists', "INSERT INTO lwt_c.t (pk, ck, v) VALUES (1, 1, 1) IF EXISTS")
q('update_if_not_exists', "UPDATE lwt_c.t SET v = 1 WHERE pk = 1 AND ck = 1 IF NOT EXISTS")

# ---- condition evaluation -----------------------------------------------------------------------
setup("INSERT INTO lwt_c.t (pk, ck, v, txt, l, st, m, fl, fs, fm, u, fu, du, tu) VALUES (2, 1, 5, 'abc', [1, 2, 3], {1, 2}, {'a': 1, 'b': 2}, [1, 2], {3}, {'x': 9}, {street: 's', zip: 7}, {street: 'f', zip: 8}, 1d, (1, 'a')) USING TIMESTAMP 1000")
def cond(cid, c, pk=2):
    q(cid, "UPDATE lwt_c.t SET txt = 'abc' WHERE pk = %d AND ck = 1 IF %s" % (pk, c))
cond('ev_slices', "v > 4 AND v < 6 AND v >= 5 AND v <= 5 AND v != 4")
cond('ev_slices_false', "v > 5")
cond('ev_in', "v IN (1, 5) AND txt IN ('abc', null)")
cond('ev_in_false', "v IN (1, 6)")
cond('ev_in_null', "v IN (null)")
cond('ev_neq_null', "v != null")
cond('ev_eq_null', "v = null")
cond('ev_text_slices', "txt > 'abb' AND txt < 'b'")
cond('ev_collections_eq', "l = [1, 2, 3] AND st = {1, 2} AND m = {'a': 1, 'b': 2}")
cond('ev_collections_neq', "l != [1, 2, 3]")
cond('ev_collections_slices', "l > [1, 2] AND l < [1, 3] AND st >= {1} AND m <= {'a': 1, 'c': 0}")
cond('ev_set_unordered', "st = {2, 1}")
cond('ev_frozen', "fl = [1, 2] AND fs = {3} AND fm = {'x': 9} AND fl > [1] AND fs < {4}")
cond('ev_empty_list_eq', "l = []")
cond('ev_empty_list_neq', "l != []")
cond('ev_empty_list_gt', "l > []")
cond('ev_empty_set_lt', "st < {}")
cond('ev_empty_map_gte', "m >= {}")
cond('ev_list_null', "l = null")
cond('ev_list_in', "l IN ([1, 2, 3], [4])")
cond('ev_list_in_null', "l IN (null, [9])")
cond('ev_elements', "l[0] = 1 AND l[2] = 3 AND l[5] = null AND m['a'] = 1 AND m['zz'] = null")
cond('ev_element_negative', "l[-1] = 1")
cond('ev_element_negative_neq', "l[-1] != 1")
cond('ev_element_null_list', "l[null] = 1")
cond('ev_element_null_map', "m[null] = 1")
cond('ev_element_null_map_neq', "m[null] != 1")
cond('ev_element_slices', "m['a'] > 0 AND m['b'] IN (2, 3) AND fl[1] = 2 AND fm['x'] = 9")
cond('ev_element_out_of_range', "l[10] = 1")
cond('ev_set_element', "st[1] = 1")
cond('ev_set_element_missing', "st[7] = null")
cond('ev_set_element_null', "st[1] = null")
cond('ev_frozen_set_element', "fs[3] = 3")
cond('ev_udt', "u = {street: 's', zip: 7} AND fu = {street: 'f', zip: 8}")
cond('ev_udt_fields', "u.zip = 7 AND fu.zip = 8 AND u.street > 'a' AND fu.street IN ('f')")
cond('ev_udt_slice', "u > {street: 'a', zip: 7}")
cond('ev_tuple', "tu = (1, 'a') AND tu > (0, 'z')")
cond('ev_tuple_null_field', "tu > (1, null)")
cond('ev_duration_in', "du IN (1d, 2d) AND du != 2d")
cond('ev_like', "txt LIKE 'ab%'")
cond('ev_like_false', "txt LIKE 'x%'")
cond('ev_not_in', "v NOT IN (1, 2)")
cond('ev_not_in_false', "v NOT IN (5)")
cond('ev_contains', "st CONTAINS 1 AND m CONTAINS 2 AND m CONTAINS KEY 'a' AND l CONTAINS 3 AND fl CONTAINS 1 AND fs CONTAINS 3 AND fm CONTAINS KEY 'x' AND fm CONTAINS 9")
cond('ev_contains_false', "l CONTAINS 7")
cond('ev_contains_null', "st CONTAINS null")
cond('ev_failing_columns', "v = 1 AND l[0] = 9 AND m['a'] = 1 AND u.zip = 0")
cond('ev_order_false_then_null', "v = 999 AND v > null")
cond('ev_order_null_then_false', "v > null AND v = 999")
cond('ev_order_false_then_empty', "v = 999 AND l > []")
cond('ev_order_columns', "txt = 'zz' AND v > null")
cond('ev_order_columns_2', "v > null AND txt = 'zz'")
cond('ev_order_map', "m['a'] = 5 AND m['b'] > null")
cond('ev_missing_row_all_null', "l = [] AND fl = null AND st = {} AND m = {} AND u.zip = null AND fu.zip = null AND m['a'] = null AND l[0] = null", pk=9)
cond('ev_missing_row_frozen_empty', "fl = []", pk=9)
cond('ev_missing_row_frozen_empty_neq', "fl != []", pk=9)
cond('ev_missing_row_slice', "u.zip > 1", pk=9)
cond('ev_missing_row_neq', "l != [1]", pk=9)
cond('ev_missing_row_contains', "l CONTAINS 7", pk=9)
cond('ev_missing_row_null_slice', "v > null", pk=9)
q('ev_map_value_key_type', "UPDATE lwt_c.t SET m['n'] = -1 WHERE pk = 2 AND ck = 1 IF EXISTS")
cond('ev_map_value_key_type_gt', "m['n'] > 0")
cond('ev_map_value_key_type_lt', "m['n'] < 0")

# ---- static columns and missing rows ------------------------------------------------------------
q('st_missing_partition', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 1 IF r = 1")
q('st_missing_partition_null', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 1 IF r = null")
q('st_other_rows', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF r = 5")
q('st_other_rows_exists', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF EXISTS")
q('st_other_rows_static_cond', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF s = 5")
q('st_set_static', "UPDATE lwt_c.r SET s = 7 WHERE p = 1 IF s = null")
q('st_regular_missing', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF r = 5")
q('st_regular_missing_exists', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF EXISTS")
q('st_regular_missing_static_cond', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 2 IF s = 7")
q('st_mixed_conds', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 3 IF s = 8 AND r = null")
q('st_mixed_conds_existing', "UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 1 IF s = 8 AND r = 1")
q('st_static_op_regular_cond', "UPDATE lwt_c.r SET s = 9 WHERE p = 1 AND c = 1 IF r = 1")
q('st_static_only_missing', "UPDATE lwt_c.r SET s = 1 WHERE p = 2 IF s = 3")
q('st_static_only_exists_missing', "UPDATE lwt_c.r SET s = 1 WHERE p = 2 IF EXISTS")
q('st_insert_static_null', "INSERT INTO lwt_c.r (p, s) VALUES (2, null) IF NOT EXISTS")
q('st_insert_static_null_again', "INSERT INTO lwt_c.r (p, s) VALUES (2, null) IF NOT EXISTS")
q('st_insert_static', "INSERT INTO lwt_c.r (p, s) VALUES (2, 5) IF NOT EXISTS")
q('st_insert_static_again', "INSERT INTO lwt_c.r (p, s) VALUES (2, 6) IF NOT EXISTS")
q('st_static_exists', "UPDATE lwt_c.r SET s = 1 WHERE p = 2 IF EXISTS")
q('st_delete_static_exists', "DELETE s FROM lwt_c.r WHERE p = 2 IF EXISTS")
q('st_delete_static_cond', "DELETE s FROM lwt_c.r WHERE p = 2 IF s = 1")
q('st_delete_row_static_cond', "DELETE FROM lwt_c.r WHERE p = 1 AND c = 1 IF s = 1")
setup("INSERT INTO lwt_c.r (p, c, r) VALUES (10, 1, 1) USING TIMESTAMP 1000", "INSERT INTO lwt_c.r (p, c, r, s) VALUES (11, 1, 1, 1) USING TIMESTAMP 1000")
q('st_rows_no_static_exists', "UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 5 IF EXISTS")
q('st_rows_no_static_static_op', "UPDATE lwt_c.r SET r = 2, s = 3 WHERE p = 10 AND c = 5 IF EXISTS")
q('st_static_static_op_missing', "UPDATE lwt_c.r SET r = 2, s = 3 WHERE p = 11 AND c = 5 IF EXISTS")
q('st_static_missing_exists', "UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 5 IF EXISTS")
q('st_static_existing_exists', "UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 1 IF EXISTS")
q('st_static_only_exists_rows', "UPDATE lwt_c.r SET s = 1 WHERE p = 10 IF EXISTS")
q('st_static_only_cond_rows', "UPDATE lwt_c.r SET s = 1 WHERE p = 10 IF s = 4")
q('st_insert_existing_with_static', "INSERT INTO lwt_c.r (p, c, r) VALUES (11, 1, 9) IF NOT EXISTS")
q('st_insert_static_regular', "INSERT INTO lwt_c.r (p, c, s) VALUES (11, 1, 7) IF NOT EXISTS")
q('st_insert_static_new_row', "INSERT INTO lwt_c.r (p, c, r, s) VALUES (11, 5, 9, 9) IF NOT EXISTS")
q('st_select', "SELECT * FROM lwt_c.r")

# ---- batches ------------------------------------------------------------------------------------
def b(*stmts, kind=''):
    return "BEGIN %sBATCH %s; APPLY BATCH" % (kind + ' ' if kind else '', '; '.join(stmts))
R = 'lwt_c.r'
q('b_exists_two', b("UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 1 IF EXISTS", "UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 7 IF EXISTS"))
q('b_conds_two_rows', b("UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 7 IF r = 4", "UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 1 IF r = 4"))
q('b_static_cond', b("UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 7 IF r = 4", "UPDATE lwt_c.r SET r = 2 WHERE p = 11 AND c = 1 IF s = 4"))
q('b_missing_rows', b("UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 7 IF r = 4", "UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 8 IF r = 4"))
q('b_same_row_twice', b("UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 7 IF r = 4", "UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 7 IF r = 5"))
q('b_mixed_not_applied', b("UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 1 IF r = 4", "INSERT INTO lwt_c.r (p, c, r) VALUES (10, 2, 2)"))
q('b_mixed_applied', b("UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 1 IF r = 1", "INSERT INTO lwt_c.r (p, c, r) VALUES (10, 2, 2)"))
q('b_mixed_first_plain', b("INSERT INTO lwt_c.r (p, c, r) VALUES (10, 3, 3)", "UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 1 IF r = 1"))
q('b_mixed_existing_plain', b("UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 2 IF r = 7", "INSERT INTO lwt_c.r (p, c, r) VALUES (10, 1, 1)"))
q('b_partition_delete_present', b("DELETE FROM lwt_c.r WHERE p = 10", "UPDATE lwt_c.r SET r = 2 WHERE p = 10 AND c = 1 IF r = 999"))
q('b_partition_delete_applied', b("DELETE FROM lwt_c.r WHERE p = 10", "UPDATE lwt_c.r SET r = 4 WHERE p = 10 AND c = 2 IF r = 2"))
q('b_partition_delete_select', "SELECT * FROM lwt_c.r WHERE p = 10")
q('b_select_r', "SELECT * FROM lwt_c.r WHERE p IN (10, 11)")
q('b_unlogged', b("UPDATE lwt_c.r SET r = 6 WHERE p = 11 AND c = 1 IF r = 1", kind='UNLOGGED'))
q('b_two_partitions', b("UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 1 IF r = 1", "UPDATE lwt_c.r SET r = 1 WHERE p = 2 AND c = 1 IF r = 1"))
q('b_two_partitions_plain', b("UPDATE lwt_c.r SET r = 6 WHERE p = 1 AND c = 2 IF r = 1", "UPDATE lwt_c.r SET r = 6 WHERE p = 2 AND c = 2"))
q('b_two_tables', b("UPDATE lwt_c.r SET r = 1 WHERE p = 1 AND c = 1 IF r = 1", "UPDATE lwt_c.w SET v = 1 WHERE p = 1 AND c = 1"))
q('b_timestamp', "BEGIN BATCH USING TIMESTAMP 5 UPDATE lwt_c.r SET r = 6 WHERE p = 1 AND c = 2 IF r = 1; APPLY BATCH")
q('b_member_timestamp', b("UPDATE lwt_c.r SET r = 6 WHERE p = 1 AND c = 2 IF r = 1", "UPDATE lwt_c.r USING TIMESTAMP 5 SET r = 6 WHERE p = 1 AND c = 3"))
q('b_cond_member_timestamp', b("UPDATE lwt_c.r USING TIMESTAMP 5 SET r = 6 WHERE p = 1 AND c = 2 IF r = 1"))
q('b_in_pk', b("UPDATE lwt_c.w SET v = 6 WHERE p IN (1, 2) AND c = 2 IF v = 1"))
q('b_in_ck', b("UPDATE lwt_c.w SET v = 6 WHERE p = 1 AND c IN (1, 2) IF v = 1"))
q('b_in_pk_plain', b("UPDATE lwt_c.w SET v = 6 WHERE p = 1 AND c = 2 IF v = 1", "UPDATE lwt_c.w SET v = 6 WHERE p IN (1, 2) AND c = 2"))
q('b_in_ck_plain', b("UPDATE lwt_c.w SET v = 6 WHERE p = 1 AND c = 2 IF v = 1", "UPDATE lwt_c.w SET v = 6 WHERE p = 1 AND c IN (1, 2)"))
q('b_counter', "BEGIN COUNTER BATCH UPDATE lwt_c.c SET cnt = cnt + 1 WHERE pk = 1 IF cnt = 1; APPLY BATCH")
setup("INSERT INTO lwt_c.w (p, c, v) VALUES (1, 1, 1) USING TIMESTAMP 1000", "INSERT INTO lwt_c.w (p, c, v) VALUES (1, 2, 2) USING TIMESTAMP 1000")
q('b_mix_cond_exists', b("UPDATE lwt_c.w SET v = 5 WHERE p = 1 AND c = 2 IF v = 4", "UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF EXISTS"))
q('b_mix_exists_cond', b("UPDATE lwt_c.w SET v = 5 WHERE p = 1 AND c = 2 IF EXISTS", "UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF v = 4"))
q('b_mix_not_exists_exists', b("INSERT INTO lwt_c.w (p, c, v) VALUES (1, 2, 1) IF NOT EXISTS", "UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF EXISTS"))
q('b_mix_exists_not_exists', b("UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF EXISTS", "INSERT INTO lwt_c.w (p, c, v) VALUES (1, 2, 1) IF NOT EXISTS"))
q('b_mix_not_exists_cond', b("INSERT INTO lwt_c.w (p, c, v) VALUES (1, 2, 1) IF NOT EXISTS", "UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF v = 1"))
q('b_mix_cond_not_exists', b("UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF v = 1", "INSERT INTO lwt_c.w (p, c, v) VALUES (1, 2, 1) IF NOT EXISTS"))
q('b_two_exists_same_row', b("UPDATE lwt_c.w SET v = 6 WHERE p = 1 AND c = 2 IF EXISTS", "UPDATE lwt_c.w SET l = [1] WHERE p = 1 AND c = 2 IF EXISTS"))
q('b_two_not_exists_same_row', b("INSERT INTO lwt_c.w (p, c, v) VALUES (1, 5, 1) IF NOT EXISTS", "INSERT INTO lwt_c.w (p, c, l) VALUES (1, 5, [5]) IF NOT EXISTS"))
q('b_insert_two_rows', b("INSERT INTO lwt_c.w (p, c, v) VALUES (2, 1, 1) IF NOT EXISTS", "INSERT INTO lwt_c.w (p, c, v) VALUES (2, 2, 1) IF NOT EXISTS"))
q('b_delete_exists_insert', b("DELETE FROM lwt_c.w WHERE p = 2 AND c = 2 IF EXISTS", "INSERT INTO lwt_c.w (p, c, v) VALUES (2, 2, 2)"))
q('b_range_delete_update', b("DELETE FROM lwt_c.w WHERE p = 2 AND c > 0", "UPDATE lwt_c.w SET v = 4 WHERE p = 2 AND c = 1 IF EXISTS"))
q('b_range_delete_select', "SELECT * FROM lwt_c.w WHERE p = 2")
q('b_partition_delete_update', b("DELETE FROM lwt_c.w WHERE p = 2", "UPDATE lwt_c.w SET v = 5 WHERE p = 2 AND c = 3 IF v = null"))
q('b_partition_delete_update_select', "SELECT * FROM lwt_c.w WHERE p = 2")
q('b_arith_twice', b("UPDATE lwt_c.w SET v = v + 1 WHERE p = 1 AND c = 1 IF EXISTS", "UPDATE lwt_c.w SET v = v + 1 WHERE p = 1 AND c = 1 IF EXISTS"))
q('b_arith_select', "SELECT c, v FROM lwt_c.w WHERE p = 1")
q('b_list_ops', b("UPDATE lwt_c.w SET l = [1, 2, 3] WHERE p = 1 AND c = 1 IF EXISTS"))
q('b_list_ops_2', b("UPDATE lwt_c.w SET l[0] = 9 WHERE p = 1 AND c = 1 IF EXISTS", "UPDATE lwt_c.w SET l = l - [2] WHERE p = 1 AND c = 1 IF EXISTS", "DELETE l[2] FROM lwt_c.w WHERE p = 1 AND c = 1"))
q('b_list_ops_select', "SELECT c, l FROM lwt_c.w WHERE p = 1")
q('list_index_out_of_bound', "UPDATE lwt_c.w SET l[5] = 9 WHERE p = 1 AND c = 1 IF EXISTS")
q('list_index_missing_row', "UPDATE lwt_c.w SET l[0] = 1 WHERE p = 1 AND c = 7 IF EXISTS")
q('list_index_null_list', "UPDATE lwt_c.w SET l[0] = 1 WHERE p = 1 AND c = 7 IF v = null")
q('list_index_not_applied', "UPDATE lwt_c.w SET l[0] = 1 WHERE p = 1 AND c = 1 IF v = 999")
q('list_append_cond', "UPDATE lwt_c.w SET l = l + [4] WHERE p = 1 AND c = 1 IF l = [9]")
q('list_prepend_cond', "UPDATE lwt_c.w SET l = [0] + l WHERE p = 1 AND c = 1 IF l = [9, 4]")
q('list_select', "SELECT c, l FROM lwt_c.w WHERE p = 1")

# ---- arithmetic -----------------------------------------------------------------------------------
setup("INSERT INTO lwt_c.n (pk, ti, si, i, bi, vi, de, f, d, txt, a, b, ts, du, bo) VALUES (1, 1, 1, 1, 1, 1, 1.5, 1.5, 1.5, 'a', 'a', 0x01, 0, 1d, true) USING TIMESTAMP 1000")
N = "UPDATE lwt_c.n SET %s WHERE pk = 1 %s"
q('ar_ints', N % ("ti = ti + 1, si = si + 1, i = i + 1, bi = bi + 1", "IF EXISTS"))
q('ar_big', N % ("vi = vi + 1, de = de + 1", "IF EXISTS"))
q('ar_floats', N % ("f = f + 1, d = d + 1", "IF EXISTS"))
q('ar_select', "SELECT ti, si, i, bi, vi, de, f, d FROM lwt_c.n WHERE pk = 1")
q('ar_ascii', N % ("a = a + 'b'", "IF EXISTS"))
q('ar_text', N % ("txt = txt + 'cd'", "IF EXISTS"))
q('ar_blob', N % ("b = b + 0x02", "IF EXISTS"))
q('ar_timestamp', N % ("ts = ts + 1", "IF EXISTS"))
q('ar_duration', N % ("du = du + 1d", "IF EXISTS"))
q('ar_boolean', N % ("bo = bo + true", "IF EXISTS"))
q('ar_sub_ints', N % ("ti = ti - 1, si = si - 1, i = i - 1, bi = bi - 1", "IF EXISTS"))
q('ar_sub_big', N % ("vi = vi - 1, de = de - 1, f = f - 1, d = d - 1", "IF EXISTS"))
q('ar_select_2', "SELECT ti, si, i, bi, vi, de, f, d, txt, a FROM lwt_c.n WHERE pk = 1")
q('ar_overflow_tinyint', N % ("ti = ti + 127", "IF EXISTS"))
q('ar_overflow_int', N % ("i = i + 2147483647", "IF EXISTS"))
q('ar_overflow_bigint_sub', N % ("bi = bi - 9223372036854775807", "IF EXISTS"))
q('ar_overflow_bigint_sub_2', N % ("bi = bi - 9223372036854775807", "IF EXISTS"))
q('ar_twice', N % ("i = i + 1, i = i + 1", "IF EXISTS"))
q('ar_select_3', "SELECT ti, i, bi FROM lwt_c.n WHERE pk = 1")
q('ar_wrong_type', N % ("i = i + 'a'", "IF EXISTS"))
q('ar_wrong_type_text', N % ("txt = txt + 1", "IF EXISTS"))
q('ar_other_column', N % ("i = bi + 1", "IF EXISTS"))
q('ar_reversed', N % ("i = 1 + i", "IF EXISTS"))
q('ar_reversed_text', N % ("txt = 'x' + txt", "IF EXISTS"))
q('ar_with_set', N % ("i = i + 1, i = 3", "IF EXISTS"))
q('ar_no_condition', N % ("i = i + 1", ""))
q('ar_no_condition_sub', N % ("i = i - 1", ""))
q('ar_no_condition_float', N % ("f = f + 1", ""))
q('ar_no_condition_text', N % ("txt = txt + 'a'", ""))
q('ar_no_condition_blob', N % ("b = b + 0x01", ""))
q('ar_cond_sub', N % ("i = i - 1", "IF i = 4"))
q('ar_null_current', "UPDATE lwt_c.n SET i = i + 1 WHERE pk = 2 IF i = null")
q('ar_null_current_select', "SELECT pk, i FROM lwt_c.n WHERE pk = 2")
setup("INSERT INTO lwt_c.n (pk, txt) VALUES (3, 'x') USING TIMESTAMP 1000")
q('ar_null_current_existing', "UPDATE lwt_c.n SET i = i + 1, txt = txt + 'y' WHERE pk = 3 IF EXISTS")
q('ar_null_current_existing_select', "SELECT pk, i, txt FROM lwt_c.n WHERE pk = 3")
q('ar_frozen_list', "UPDATE lwt_c.t SET fl = fl + [1] WHERE pk = 2 AND ck = 1 IF EXISTS")
q('ar_static', "UPDATE lwt_c.r SET s = s + 1 WHERE p = 11 IF EXISTS")
q('ar_static_select', "SELECT s FROM lwt_c.r WHERE p = 11")

# ---- JSON -----------------------------------------------------------------------------------------
q('json_insert', "INSERT INTO lwt_c.k JSON '{\"p\": 20, \"v\": 1}' IF NOT EXISTS")
q('json_insert_again', "INSERT INTO lwt_c.k JSON '{\"p\": 20, \"v\": 5}' IF NOT EXISTS")
q('json_select', "SELECT p, v FROM lwt_c.k WHERE p = 20")
q('json_batch', b("INSERT INTO lwt_c.k JSON '{\"p\": 20, \"v\": 6}' IF NOT EXISTS"))
q('json_batch_select', "SELECT p, v FROM lwt_c.k WHERE p = 20")
q('json_prepare', "INSERT INTO lwt_c.k JSON ? IF NOT EXISTS", prep=True)

# ---- consistency ----------------------------------------------------------------------------------
q('cl_serial_commit', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", cl='SERIAL')
q('cl_local_serial_commit', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", cl='LOCAL_SERIAL')
q('cl_serial_plain', "UPDATE lwt_c.k SET v = 5 WHERE p = 30", cl='SERIAL')
q('cl_serial_insert', "INSERT INTO lwt_c.k (p, v) VALUES (30, 1)", cl='SERIAL')
q('cl_serial_delete', "DELETE FROM lwt_c.k WHERE p = 30", cl='SERIAL')
q('cl_serial_counter', "UPDATE lwt_c.c SET cnt = cnt + 1 WHERE pk = 1", cl='SERIAL')
q('cl_serial_counter_batch', "BEGIN COUNTER BATCH UPDATE lwt_c.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH", cl='SERIAL')
q('cl_serial_batch', b("UPDATE lwt_c.k SET v = 5 WHERE p = 30"), cl='SERIAL')
q('cl_serial_one', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", serial='ONE')
q('cl_serial_quorum', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", serial='QUORUM')
q('cl_serial_local_one', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", serial='LOCAL_ONE')
q('cl_both_bad', "UPDATE lwt_c.k SET v = 5 WHERE p = 30 IF v = null", cl='SERIAL', serial='ONE')
q('cl_in_and_serial', "UPDATE lwt_c.t SET v = 5 WHERE pk IN (1, 2) AND ck = 1 IF v = null", serial='ONE')
q('cl_plain_serial_one', "UPDATE lwt_c.k USING TIMESTAMP 1000 SET v = 6 WHERE p = 30", serial='ONE')
q('cl_batch_serial_one', b("UPDATE lwt_c.k SET v = 7 WHERE p = 30 IF v = 6"), serial='ONE')
q('cl_local_serial', "UPDATE lwt_c.k SET v = 7 WHERE p = 30 IF v = 6", serial='LOCAL_SERIAL')
q('cl_any', "UPDATE lwt_c.k SET v = 8 WHERE p = 30 IF v = 7", cl='ANY')
q('cl_each_quorum', "UPDATE lwt_c.k SET v = 9 WHERE p = 30 IF v = 8", cl='EACH_QUORUM')
q('cl_all', "UPDATE lwt_c.k SET v = 10 WHERE p = 30 IF v = 9", cl='ALL')
q('cl_select', "SELECT p, v FROM lwt_c.k WHERE p = 30")

# ---- SERIAL reads -----------------------------------------------------------------------------------
q('sr_partition', "SELECT * FROM lwt_c.r WHERE p = 11", cl='SERIAL')
q('sr_row', "SELECT * FROM lwt_c.r WHERE p = 11 AND c = 1", cl='SERIAL')
q('sr_in_two', "SELECT * FROM lwt_c.r WHERE p IN (10, 11)", cl='SERIAL')
q('sr_in_one', "SELECT * FROM lwt_c.r WHERE p IN (11)", cl='SERIAL')
q('sr_in_same', "SELECT * FROM lwt_c.r WHERE p IN (11, 11)", cl='SERIAL')
q('sr_scan', "SELECT * FROM lwt_c.k", cl='SERIAL')
q('sr_token', "SELECT * FROM lwt_c.r WHERE token(p) > 0", cl='SERIAL')
q('sr_count', "SELECT count(*) FROM lwt_c.r WHERE p = 11", cl='SERIAL')
q('sr_filtering', "SELECT * FROM lwt_c.r WHERE r = 5 ALLOW FILTERING", cl='SERIAL')
q('sr_filtering_partition', "SELECT * FROM lwt_c.r WHERE p = 11 AND r = 2 ALLOW FILTERING", cl='SERIAL')
q('sr_distinct', "SELECT DISTINCT p FROM lwt_c.r", cl='LOCAL_SERIAL')
q('sr_distinct_partition', "SELECT DISTINCT p FROM lwt_c.r WHERE p = 11", cl='LOCAL_SERIAL')
q('sr_limit', "SELECT * FROM lwt_c.r WHERE p = 11 LIMIT 1", cl='LOCAL_SERIAL')
q('sr_system_local', "SELECT key FROM system.local", cl='SERIAL')
q('sr_system_schema', "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'lwt_c'", cl='SERIAL')
q('sr_serial_ignored', "SELECT p, v FROM lwt_c.k WHERE p = 30", cl='SERIAL', serial='ONE')

# ---- bound values -------------------------------------------------------------------------------------
setup("INSERT INTO lwt_c.k (p, v, txt, m, l) VALUES (40, 1, 'abc', {'a': 1}, [1, 2]) USING TIMESTAMP 1000")
KU = "UPDATE lwt_c.k SET v = ? WHERE p = ? IF "
q('bv_null_set', "UPDATE lwt_c.k SET v = ? WHERE p = ? IF EXISTS", values=[None, 40])
q('bv_restore', "UPDATE lwt_c.k SET v = ? WHERE p = ? IF EXISTS", values=[1, 40])
q('bv_eq_null', KU + "v = ?", values=[1, 40, None])
q('bv_eq_unset', KU + "v = ?", values=[1, 40, U])
q('bv_in_null', KU + "v IN ?", values=[1, 40, None])
q('bv_in_unset', KU + "v IN ?", values=[1, 40, U])
q('bv_in_null_item', KU + "v IN ?", values=[1, 40, [None]])
q('bv_in_list', KU + "v IN ?", values=[1, 40, [7, 1]])
q('bv_in_items_unset', KU + "v IN (?, ?)", values=[1, 40, 2, U])
q('bv_in_items_all_unset', KU + "v IN (?, ?)", values=[1, 40, U, U])
q('bv_in_items_null', KU + "v IN (?, ?)", values=[1, 40, None, 1])
q('bv_key_null', KU + "m[?] = ?", values=[1, 40, None, 3])
q('bv_key_null_neq', KU + "m[?] != ?", values=[1, 40, None, 3])
q('bv_value_null', KU + "m[?] = ?", values=[1, 40, 'zz', None])
q('bv_key_unset', KU + "m[?] = ?", values=[1, 40, U, 3])
q('bv_value_unset', KU + "m[?] = ?", values=[1, 40, 'a', U])
q('bv_list_index_unset', KU + "l[?] = 1", values=[1, 40, U])
q('bv_list_index_null', KU + "l[?] = 1", values=[1, 40, None])
q('bv_list_index_negative', KU + "l[?] = 1", values=[1, 40, -2])
q('bv_like_null', KU + "txt LIKE ?", values=[1, 40, None])
q('bv_like_unset', KU + "txt LIKE ?", values=[1, 40, U])
q('bv_like', KU + "txt LIKE ?", values=[1, 40, '%c'])
q('bv_set_unset', "UPDATE lwt_c.k SET v = ? WHERE p = ? IF EXISTS", values=[U, 40])
q('bv_pk_null', "UPDATE lwt_c.k SET v = ? WHERE p = ? IF EXISTS", values=[1, None])
q('bv_arith_unset', "UPDATE lwt_c.k SET v = v + ? WHERE p = ? IF EXISTS", values=[U, 40])
q('bv_arith_null_sub', "UPDATE lwt_c.k SET v = v - ? WHERE p = ? IF EXISTS", values=[None, 40])
q('bv_arith_select', "SELECT p, v FROM lwt_c.k WHERE p = 40")
q('bv_insert_unset_value', "INSERT INTO lwt_c.k (p, v) VALUES (?, ?) IF NOT EXISTS", values=[41, U])
q('bv_insert_select', "SELECT p, v FROM lwt_c.k WHERE p = 41")

# ---- prepared metadata ------------------------------------------------------------------------------
q('pm_update', "UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF r = ? AND s = ?", prep=True)
q('pm_update_rev', "UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF s = ? AND r = ?", prep=True)
q('pm_in', "UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF r IN ? AND s IN (?, ?)", prep=True)
q('pm_exists', "UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF EXISTS", prep=True)
q('pm_insert', "INSERT INTO lwt_c.r (p, c, r) VALUES (?, ?, ?) IF NOT EXISTS", prep=True)
q('pm_delete', "DELETE FROM lwt_c.r WHERE p = ? AND c = ? IF r = ?", prep=True)
q('pm_dup_column', "UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF r = ? AND r = ?", prep=True)
q('pm_batch', "BEGIN BATCH UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF r = ?; INSERT INTO lwt_c.r (p, c, r) VALUES (?, ?, ?); APPLY BATCH", prep=True)
q('pm_batch_static', "BEGIN BATCH UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF s = ?; APPLY BATCH", prep=True)
q('pm_batch_exists', "BEGIN BATCH UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ? IF EXISTS; APPLY BATCH", prep=True)
q('pm_batch_plain', "BEGIN BATCH UPDATE lwt_c.r SET r = ? WHERE p = ? AND c = ?; APPLY BATCH", prep=True)
q('pm_elements', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m[?] = ? AND l[?] IN ? AND fl[?] > ?", prep=True)
q('pm_named', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m[:k] = :v", prep=True)
q('pm_contains', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m CONTAINS ? AND m CONTAINS KEY ? AND l CONTAINS ?", prep=True)
q('pm_like_not_in', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF txt LIKE ? AND v NOT IN ? AND txt NOT IN (?, ?) AND st[?] = ?", prep=True)
q('pm_in_marker', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF txt IN ?", prep=True)
q('pm_field', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF u.zip = ? AND fu.street IN ?", prep=True)
q('pm_in_pk', "UPDATE lwt_c.t SET v = ? WHERE pk IN ? AND ck = ? IF txt = ?", prep=True)
q('pm_in_ck', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck IN ? IF txt = ?", prep=True)
q('pm_arith', "UPDATE lwt_c.t SET v = v + ? WHERE pk = ? AND ck = ? IF txt = ?", prep=True)
q('pm_static', "UPDATE lwt_c.r SET s = ? WHERE p = ? IF s = ?", prep=True)

# ---- protocol batches ---------------------------------------------------------------------------------
q('pb_exists_insert', None, pbatch=('LOGGED', [("UPDATE lwt_c.k SET v = 8 WHERE p = ? IF EXISTS", [40]), ("INSERT INTO lwt_c.k (p, v) VALUES (?, ?)", [40, 8])]))
q('pb_two_partitions', None, pbatch=('UNLOGGED', [("UPDATE lwt_c.k SET v = 8 WHERE p = ? IF v = 9", [40]), ("INSERT INTO lwt_c.k (p, v) VALUES (?, ?)", [42, 8])]))
q('pb_two_tables', None, pbatch=('LOGGED', [("UPDATE lwt_c.k SET v = 8 WHERE p = ? IF v = 9", [40]), ("UPDATE lwt_c.w SET v = 1 WHERE p = ? AND c = 1", [40])]))
q('pb_not_applied', None, pbatch=('LOGGED', [("UPDATE lwt_c.k SET v = 9 WHERE p = ? IF v = ?", [40, 99]), ("UPDATE lwt_c.k SET txt = ? WHERE p = ?", ['z', 40])]))
q('pb_select', "SELECT p, v, txt FROM lwt_c.k WHERE p = 40")

# ---- more statement forms -------------------------------------------------------------------------
setup("INSERT INTO lwt_c.t (pk, ck, v, l, m, u, fu) VALUES (5, 1, 1, [1, 2, 3], {'a': 1, 'b': 2}, {street: 's', zip: 1}, {street: 'f', zip: null}) USING TIMESTAMP 1000")
q('mf_delete_element', "DELETE l[0] FROM lwt_c.t WHERE pk = 5 AND ck = 1 IF v = 1")
q('mf_delete_map_key', "DELETE m['a'] FROM lwt_c.t WHERE pk = 5 AND ck = 1 IF m['a'] = 1")
q('mf_delete_field', "DELETE u.street FROM lwt_c.t WHERE pk = 5 AND ck = 1 IF u.zip = 1")
q('mf_set_field', "UPDATE lwt_c.t SET u.zip = 2 WHERE pk = 5 AND ck = 1 IF u.zip = 1")
q('mf_set_map', "UPDATE lwt_c.t SET m['c'] = 3, m = m + {'d': 4} WHERE pk = 5 AND ck = 1 IF EXISTS")
q('mf_set_remove', "UPDATE lwt_c.t SET m = m - {'b'} WHERE pk = 5 AND ck = 1 IF m CONTAINS KEY 'b'")
q('mf_select', "SELECT l, m, u FROM lwt_c.t WHERE pk = 5 AND ck = 1")
q('mf_frozen_udt_null_field', "UPDATE lwt_c.t SET v = 2 WHERE pk = 5 AND ck = 1 IF fu.zip = null")
q('mf_frozen_udt_null_field_slice', "UPDATE lwt_c.t SET v = 2 WHERE pk = 5 AND ck = 1 IF fu.zip > 0")
q('mf_like_element', "UPDATE lwt_c.t SET v = 3 WHERE pk = 5 AND ck = 1 IF u.street LIKE 'x%'")
q('mf_not_in_marker', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF v NOT IN ?", values=[4, 5, 1, [7, 8]])
q('mf_not_in_marker_null', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF v NOT IN ?", values=[4, 5, 1, None])
q('mf_element_in_marker', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m[?] IN ?", values=[5, 5, 1, 'c', [3, None]])
q('mf_contains_bound', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m CONTAINS ? AND m CONTAINS KEY ?", values=[6, 5, 1, 3, 'c'])
q('mf_contains_unset', "UPDATE lwt_c.t SET v = ? WHERE pk = ? AND ck = ? IF m CONTAINS ?", values=[6, 5, 1, U])
q('mf_whole_udt_null', "UPDATE lwt_c.t SET v = 7 WHERE pk = 5 AND ck = 1 IF u = null")
q('mf_select_2', "SELECT v FROM lwt_c.t WHERE pk = 5 AND ck = 1")
q('mb_static_and_row', b("UPDATE lwt_c.r SET s = 1 WHERE p = 11 IF s = 10", "UPDATE lwt_c.r SET r = 1 WHERE p = 11 AND c = 1 IF r = 2"))
q('mb_static_and_row_select', "SELECT * FROM lwt_c.r WHERE p = 11")
q('mb_static_only_conds', b("UPDATE lwt_c.r SET s = 1 WHERE p = 11 IF s = 99", "UPDATE lwt_c.r SET r = 5 WHERE p = 11 AND c = 5"))
q('mb_static_exists_and_row', b("UPDATE lwt_c.r SET s = 2 WHERE p = 11 IF EXISTS", "UPDATE lwt_c.r SET r = 9 WHERE p = 11 AND c = 9 IF r = 1"))
q('mb_plain_static_op', b("UPDATE lwt_c.r SET r = 1 WHERE p = 11 AND c = 1 IF r = 99", "UPDATE lwt_c.r SET s = 3 WHERE p = 11"))
q('mb_cond_static_op', b("UPDATE lwt_c.r SET r = 1, s = 4 WHERE p = 11 AND c = 1 IF r = 99"))
q('mb_three_rows', b("UPDATE lwt_c.r SET r = 1 WHERE p = 11 AND c = 5 IF r = 9", "UPDATE lwt_c.r SET r = 1 WHERE p = 11 AND c = 1 IF r = 99", "INSERT INTO lwt_c.r (p, c, r) VALUES (11, 0, 0) IF NOT EXISTS"))
q('mb_select', "SELECT * FROM lwt_c.r WHERE p = 11")
q('mb_no_clustering_table', b("UPDATE lwt_c.k SET v = 1 WHERE p = 50 IF v = null", "UPDATE lwt_c.k SET txt = 'a' WHERE p = 50 IF EXISTS"))
q('mb_no_clustering_table_2', b("INSERT INTO lwt_c.k (p, v) VALUES (50, 1) IF NOT EXISTS", "UPDATE lwt_c.k SET txt = 'a' WHERE p = 50 IF txt = null"))
q('mb_no_clustering_select', "SELECT p, v, txt FROM lwt_c.k WHERE p = 50")

# ---- durations in slices ------------------------------------------------------------------------------
setup("CREATE TYPE lwt_c.dur_t (d duration)",
      "CREATE TABLE lwt_c.dd (p int PRIMARY KEY, ld list<duration>, td tuple<int, duration>, ud frozen<dur_t>, md map<int, duration>, a ascii)")
q('du_list_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF ld > [1d]")
q('du_tuple_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF td > (1, 1d)")
q('du_udt_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF ud > {d: 1d}")
q('du_element_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF ld[0] > 1d")
q('du_map_element_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF md[1] < 1d")
q('du_field_slice', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF ud.d >= 1d")
q('du_list_eq', "UPDATE lwt_c.dd SET a = 'x' WHERE p = 1 IF ld = [1d]")
q('du_like_ascii', "UPDATE lwt_c.dd SET a = 'y' WHERE p = 1 IF a LIKE 'x%'")
q('du_in_slice', "UPDATE lwt_c.dd SET a = 'y' WHERE p = 1 IF ld IN ([1d])")

setup("DROP KEYSPACE IF EXISTS lwt_c")

# ---- runner -------------------------------------------------------------------------------------------
def err(e):
    msg = getattr(e, 'message', None)
    if not isinstance(msg, str): msg = str(e)
    m = re.search(r'code=([0-9a-fA-F]+) \[(.*?)\] message="(.*)"', msg, re.S)
    if m: return 'ERR 0x%s %s' % (m.group(1).upper().rjust(4, '0'), m.group(3))
    code = getattr(e, 'code', None)
    if isinstance(code, int): return 'ERR 0x%04X %s' % (code, msg)
    return 'CLIENT %s: %s' % (type(e).__name__, msg)

def canon(rs):
    cols = list(rs.column_names or [])
    w = rs.response_future.warnings
    warn = (' WARN %r' % (list(w),)) if w else ''
    if not cols and not rs.current_rows:
        return 'ROWS cols=[] types=[] rows=[]' + warn
    types = [t.cql_parameterized_type() for t in (rs.column_types or [])]
    rows = json.loads(json.dumps([list(r) for r in rs.current_rows], default=repr))
    return 'ROWS cols=%r types=%r rows=%r%s' % (cols, types, rows, warn)

def ser(v):
    if v is None: return None
    if v is U: return 'unset'
    return v.hex()

def run(host):
    prof = ExecutionProfile(request_timeout=60, row_factory=tuple_factory,
                            load_balancing_policy=WhiteListRoundRobinPolicy([host]))
    c = Cluster([host], execution_profiles={EXEC_PROFILE_DEFAULT: prof}, protocol_version=4)
    s = c.connect()
    out = []
    for cid, cql, o in S:
        cl = getattr(CL, o.get('cl', 'ONE'))
        serial = getattr(CL, o['serial']) if o.get('serial') else None
        rec = {'id': cid, 'cql': cql, 'cl': o.get('cl', 'ONE'), 'serial': o.get('serial'),
               'prep': bool(o.get('prep')), 'values': None, 'pbatch': None}
        try:
            if o.get('prep'):
                p = s.prepare(cql)
                bind = [[x.name, x.type.cql_parameterized_type()] for x in (p.column_metadata or [])]
                res = [[x[2], x[3].cql_parameterized_type()] for x in (p.result_metadata or [])]
                r = 'PREP bind=%r result=%r' % (bind, res)
            elif o.get('pbatch'):
                kind, stmts = o['pbatch']
                bs = BatchStatement(batch_type=getattr(BatchType, kind), consistency_level=cl)
                bs._serial_consistency_level = serial
                recs = []
                for sq, vals in stmts:
                    p = s.prepare(sq)
                    bound = p.bind(vals)
                    bs.add(bound)
                    recs.append([sq, [ser(v) for v in bound.values]])
                rec['pbatch'] = [kind, recs]
                r = canon(s.execute(bs))
            elif 'values' in o:
                p = s.prepare(cql)
                bound = p.bind(o['values'])
                rec['values'] = [ser(v) for v in bound.values]
                bound.consistency_level = cl
                bound._serial_consistency_level = serial
                r = canon(s.execute(bound))
            else:
                st = SimpleStatement(cql, consistency_level=cl)
                st._serial_consistency_level = serial
                r = canon(s.execute(st))
        except Exception as e:
            r = err(e)
        rec['result'] = r
        out.append(rec)
    c.shutdown()
    return out

sc = run(sys.argv[1])
ca = run('127.0.0.1')
steps = []
for a, b_ in zip(sc, ca):
    x = dict(a)
    x['scylla'] = a['result']; x['cassandra'] = b_['result']
    del x['result']
    if a['values'] is None and b_['values'] is not None: x['values'] = b_['values']
    if a['pbatch'] is None and b_['pbatch'] is not None: x['pbatch'] = b_['pbatch']
    steps.append(x)
json.dump(steps, sys.stdout, indent=1)
