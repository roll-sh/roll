# Derived data capture: secondary indexes (2i, ScyllaDB global and local, SAI), vector search,
# materialized views and CDC, run in order on both references. Writes JSON (stdout): a list of
# steps with the canonical result text of each dialect (the form of crates/nosql-engine's
# `captured_derived_harness`), turned into Rust by crates/nosql-engine/scripts/gen_derived.py.
#
# Runs inside the Cassandra reference container with the Python driver cqlsh bundles, so no new
# container is needed; DDL goes through the raw client (raw.py, prepended) for exact result kinds
# and error texts:
#   cat raw.py derived.py | docker exec -i nosql-ref-cassandra python3 - <scylla-ip> \
#     > ~/.cache/roll-nosql/capture/out/derived.json
# where <scylla-ip> is the bridge address of nosql-ref-scylla (docker inspect).
import sys, glob, re, json
sys.path[:0] = [z for z in glob.glob('/opt/cassandra/lib/*.zip') if 'driver' not in z] + \
    ['/opt/cassandra/lib/cassandra-driver-internal-only-3.29.0.zip/cassandra-driver-3.29.0']
from cassandra.cluster import Cluster, ExecutionProfile, EXEC_PROFILE_DEFAULT
from cassandra.policies import WhiteListRoundRobinPolicy
from cassandra.query import SimpleStatement, tuple_factory

S = []  # (id, cql, opts); id 'setup' runs without recording

def setup(*qs):
    for q in qs: S.append(('setup', q, {}))

def q(cid, cql, **opts):
    S.append((cid, cql, opts))

def wait(ks):
    # ScyllaDB builds views and indexes in the background: wait until the keyspace's are built.
    S.append(('wait', ks, {}))

NTS = "{'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"
TABLETS_OFF = " AND tablets = {'enabled': false}"

# ==== secondary indexes: DDL ========================================================================
setup(
    "DROP KEYSPACE IF EXISTS idx_i",
    "CREATE KEYSPACE idx_i WITH replication = " + NTS,
    "CREATE TYPE idx_i.ty (a int, b text)",
    "CREATE TABLE idx_i.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<int>, m map<int, text>, fl frozen<list<int>>, fm frozen<map<int, int>>, fs frozen<set<int>>, tu tuple<int, text>, du duration, u ty, fu frozen<ty>, vec vector<float, 3>, PRIMARY KEY (pk, ck))",
    "CREATE TABLE idx_i.k (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d))",
    "CREATE TABLE idx_i.one (pk int PRIMARY KEY, v int, w text)",
    "CREATE TABLE idx_i.cnt (pk int PRIMARY KEY, c counter)",
    'CREATE TABLE idx_i."Quoted" ("Key" int PRIMARY KEY, "camelCase" text, "with space" int)',
)
for cid, x in [
    ('ci_regular', "CREATE INDEX ON idx_i.t (v)"),
    ('ci_regular_again_noname', "CREATE INDEX ON idx_i.t (v)"),
    ('ci_regular_named_dup', "CREATE INDEX t_v_other ON idx_i.t (v)"),
    ('ci_if_not_exists_dup', "CREATE INDEX IF NOT EXISTS t_v_other ON idx_i.t (v)"),
    ('ci_name_exists', "CREATE INDEX t_v_idx ON idx_i.t (txt)"),
    ('ci_name_exists_if_not_exists', "CREATE INDEX IF NOT EXISTS t_v_idx ON idx_i.t (txt)"),
    ('ci_clustering', "CREATE INDEX ON idx_i.t (ck)"),
    ('ci_static', "CREATE INDEX ON idx_i.t (s)"),
    ('ci_text_named', "CREATE INDEX t_txt ON idx_i.t (txt)"),
    ('ci_list_values', "CREATE INDEX ON idx_i.t (l)"),
    ('ci_list_values_explicit_dup', "CREATE INDEX ON idx_i.t (VALUES(l))"),
    ('ci_set_values', "CREATE INDEX ON idx_i.t (VALUES(st))"),
    ('ci_map_values', "CREATE INDEX ON idx_i.t (m)"),
    ('ci_map_keys', "CREATE INDEX ON idx_i.t (KEYS(m))"),
    ('ci_map_entries', "CREATE INDEX ON idx_i.t (ENTRIES(m))"),
    ('ci_frozen_list_full', "CREATE INDEX ON idx_i.t (FULL(fl))"),
    ('ci_frozen_map_full', "CREATE INDEX fm_full ON idx_i.t (FULL(fm))"),
    ('ci_frozen_set_plain', "CREATE INDEX ON idx_i.t (fs)"),
    ('ci_tuple', "CREATE INDEX ON idx_i.t (tu)"),
    ('ci_frozen_udt', "CREATE INDEX ON idx_i.t (fu)"),
    ('ci_udt_nonfrozen', "CREATE INDEX ON idx_i.t (u)"),
    ('ci_vector_plain', "CREATE INDEX ON idx_i.t (vec)"),
    ('ci_duration', "CREATE INDEX ON idx_i.t (du)"),
    ('ci_only_pk', "CREATE INDEX ON idx_i.one (pk)"),
    ('ci_pk_component', "CREATE INDEX ON idx_i.k (b)"),
    ('ci_pk_component_a', "CREATE INDEX ON idx_i.k (a)"),
    ('ci_ck_second', "CREATE INDEX ON idx_i.k (d)"),
    ('ci_counter', "CREATE INDEX ON idx_i.cnt (c)"),
    ('ci_unknown_column', "CREATE INDEX ON idx_i.t (nope)"),
    ('ci_unknown_table', "CREATE INDEX ON idx_i.nope (v)"),
    ('ci_unknown_keyspace', "CREATE INDEX ON idx_nope.t (v)"),
    ('ci_no_keyspace', "CREATE INDEX ON t (v)"),
    ('ci_system', "CREATE INDEX ON system.local (rack)"),
    ('ci_two_columns', "CREATE INDEX ON idx_i.one (v, w)"),
    ('ci_keys_on_list', "CREATE INDEX ON idx_i.t (KEYS(l))"),
    ('ci_keys_on_set', "CREATE INDEX ON idx_i.t (KEYS(st))"),
    ('ci_entries_on_list', "CREATE INDEX ON idx_i.t (ENTRIES(l))"),
    ('ci_full_on_list', "CREATE INDEX ON idx_i.t (FULL(l))"),
    ('ci_full_on_int', "CREATE INDEX ON idx_i.t (FULL(v))"),
    ('ci_values_on_int', "CREATE INDEX ON idx_i.one (VALUES(w))"),
    ('ci_keys_on_int', "CREATE INDEX ON idx_i.one (KEYS(w))"),
    ('ci_entries_on_int', "CREATE INDEX ON idx_i.one (ENTRIES(w))"),
    ('ci_values_on_frozen', "CREATE INDEX ON idx_i.t (VALUES(fl))"),
    ('ci_keys_on_frozen_map', "CREATE INDEX ON idx_i.t (KEYS(fm))"),
    ('ci_entries_on_frozen_map', "CREATE INDEX ON idx_i.t (ENTRIES(fm))"),
    ('ci_options_non_custom', "CREATE INDEX ON idx_i.one (w) WITH OPTIONS = {'a': 'b'}"),
    ('ci_custom_no_class', "CREATE CUSTOM INDEX ON idx_i.one (w)"),
    ('ci_custom_unknown_class', "CREATE CUSTOM INDEX ON idx_i.one (w) USING 'org.example.Nope'"),
    ('ci_using_unknown_class', "CREATE INDEX ON idx_i.one (w) USING 'nope'"),
    ('ci_using_legacy', "CREATE INDEX ON idx_i.one (w) USING 'legacy_local_table'"),
    ('ci_sasi_disabled', "CREATE CUSTOM INDEX ON idx_i.one (w) USING 'org.apache.cassandra.index.sasi.SASIIndex'"),
    ('ci_quoted_column', 'CREATE INDEX ON idx_i."Quoted" ("camelCase")'),
    ('ci_quoted_space_column', 'CREATE INDEX ON idx_i."Quoted" ("with space")'),
    ('ci_quoted_name', 'CREATE INDEX "Odd Name" ON idx_i.one (w)'),
    ('ci_name_like_table', "CREATE INDEX one ON idx_i.k (c)"),
    ('ci_local', "CREATE INDEX ON idx_i.t ((pk), v)"),
    ('ci_local_named', "CREATE INDEX t_local_txt ON idx_i.t ((pk), txt)"),
    ('ci_local_clustering', "CREATE INDEX ON idx_i.k ((a, b), d)"),
    ('ci_local_partial_pk', "CREATE INDEX ON idx_i.k ((a), v)"),
    ('ci_local_redundant', "CREATE INDEX ON idx_i.t ((pk, ck), v)"),
    ('ci_local_collection', "CREATE INDEX ON idx_i.t ((pk), l)"),
    ('ci_local_static', "CREATE INDEX ON idx_i.t ((pk), s)"),
]:
    q(cid, x)
wait('idx_i')
q('si_indexes', "SELECT * FROM system_schema.indexes WHERE keyspace_name = 'idx_i'")
q('si_views', "SELECT view_name, base_table_name, include_all_columns, where_clause FROM system_schema.views WHERE keyspace_name = 'idx_i'")
q('si_view_options', "SELECT view_name, bloom_filter_fp_chance, caching, comment, compaction, compression, crc_check_chance, default_time_to_live, extensions, gc_grace_seconds, max_index_interval, memtable_flush_period_in_ms, min_index_interval, speculative_retry FROM system_schema.views WHERE keyspace_name = 'idx_i' AND view_name = 't_v_idx_index'")
q('si_view_columns', "SELECT table_name, column_name, clustering_order, kind, position, type FROM system_schema.columns WHERE keyspace_name = 'idx_i' AND table_name IN ('t_v_idx_index', 't_ck_idx_index', 't_s_idx_index', 't_l_idx_index', 't_st_idx_index', 't_m_idx_index', 't_m_idx_1_index', 't_m_idx_2_index', 't_fl_idx_index', 'k_b_idx_index', 'k_d_idx_index', 't_v_idx_1_index', 'k_d_idx_1_index', 'Quoted_camelCase_idx_index')")
q('si_computed', "SELECT * FROM system_schema.computed_columns WHERE keyspace_name = 'idx_i'")
q('si_virtual', "SELECT * FROM system_schema.view_virtual_columns WHERE keyspace_name = 'idx_i'")
q('si_index_info', "SELECT * FROM system.\"IndexInfo\" WHERE table_name = 'idx_i'")
q('si_built_views', "SELECT * FROM system.built_views WHERE keyspace_name = 'idx_i'")
q('si_build_status', "SELECT keyspace_name, view_name, status FROM system_distributed.view_build_status WHERE keyspace_name = 'idx_i' AND view_name = 't_v_idx_index'")
q('si_select_index_view', "SELECT * FROM idx_i.t_v_idx_index")
q('si_select_index_by_name', "SELECT * FROM idx_i.t_v_idx")
q('si_write_index_view', "INSERT INTO idx_i.t_v_idx_index (v, idx_token, pk, ck) VALUES (1, 1, 1, 1)")
q('si_drop_table_index_view', "DROP TABLE idx_i.t_v_idx_index")
q('si_alter_index_view', "ALTER TABLE idx_i.t_v_idx_index WITH comment = 'x'")
q('si_alter_mv_index_view', "ALTER MATERIALIZED VIEW idx_i.t_v_idx_index WITH comment = 'x'")
q('si_drop_mv_index_view', "DROP MATERIALIZED VIEW idx_i.t_v_idx_index")
q('si_truncate_index_view', "TRUNCATE idx_i.t_v_idx_index")
q('si_index_on_index_view', "CREATE INDEX ON idx_i.t_v_idx_index (pk)")
q('si_alter_drop_indexed', "ALTER TABLE idx_i.t DROP v")
q('si_alter_drop_indexed_collection', "ALTER TABLE idx_i.t DROP m")
q('si_alter_rename_indexed', "ALTER TABLE idx_i.k RENAME c TO cc")
q('si_alter_drop_unindexed', "ALTER TABLE idx_i.one DROP w")
for cid, x in [
    ('di_existing', "DROP INDEX idx_i.t_txt"),
    ('di_again', "DROP INDEX idx_i.t_txt"),
    ('di_if_exists', "DROP INDEX IF EXISTS idx_i.t_txt"),
    ('di_unknown_keyspace', "DROP INDEX idx_nope.x"),
    ('di_unknown_keyspace_if_exists', "DROP INDEX IF EXISTS idx_nope.x"),
    ('di_table_name', "DROP INDEX idx_i.one"),
    ('di_view_name', "DROP INDEX idx_i.t_v_idx_index"),
    ('di_local', "DROP INDEX idx_i.t_local_txt"),
]:
    q(cid, x)
q('si_indexes_after_drop', "SELECT index_name FROM system_schema.indexes WHERE keyspace_name = 'idx_i' AND table_name = 't'")
q('si_views_after_drop', "SELECT view_name FROM system_schema.views WHERE keyspace_name = 'idx_i'")
q('si_drop_base', "DROP TABLE idx_i.k")
q('si_views_after_drop_base', "SELECT view_name FROM system_schema.views WHERE keyspace_name = 'idx_i'")
q('si_indexes_after_drop_base', "SELECT table_name, index_name FROM system_schema.indexes WHERE keyspace_name = 'idx_i'")

# ==== secondary indexes: queries =====================================================================
setup(
    "DROP KEYSPACE IF EXISTS idx_q",
    "CREATE KEYSPACE idx_q WITH replication = " + NTS,
    "CREATE TABLE idx_q.t (pk int, ck int, v int, w int, s int static, txt text, l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, PRIMARY KEY (pk, ck))",
    "INSERT INTO idx_q.t (pk, ck, v, w, s, txt, l, st, m, fl) VALUES (1, 1, 10, 1, 100, 'a', [1, 2, 1], {'x', 'y'}, {'a': 1, 'b': 2}, [1, 2])",
    "INSERT INTO idx_q.t (pk, ck, v, w, txt, l, st, m, fl) VALUES (1, 2, 10, 2, 'b', [3], {'y'}, {'a': 1}, [3])",
    "INSERT INTO idx_q.t (pk, ck, v, w, txt) VALUES (1, 3, 20, 1, 'c')",
    "INSERT INTO idx_q.t (pk, ck, v, w, s, txt, m) VALUES (2, 1, 10, 1, 100, 'a', {'b': 2})",
    "INSERT INTO idx_q.t (pk, ck, v, w, s, txt) VALUES (3, 1, 20, 2, 300, 'b')",
    "INSERT INTO idx_q.t (pk, ck, v, txt, fl) VALUES (4, 5, 10, '', [1, 2])",
    "INSERT INTO idx_q.t (pk, ck, v) VALUES (5, 1, 30)",
    "INSERT INTO idx_q.t (pk, ck, v) VALUES (6, 1, 10)",
    "INSERT INTO idx_q.t (pk, ck, v) VALUES (7, 1, 10)",
    "INSERT INTO idx_q.t (pk, s) VALUES (8, 100)",
    # Backfill: indexes created after the data.
    "CREATE INDEX t_v ON idx_q.t (v)",
    "CREATE INDEX t_ck ON idx_q.t (ck)",
    "CREATE INDEX t_s ON idx_q.t (s)",
    "CREATE INDEX t_txt ON idx_q.t (txt)",
    "CREATE INDEX t_l ON idx_q.t (l)",
    "CREATE INDEX t_st ON idx_q.t (st)",
    "CREATE INDEX t_mk ON idx_q.t (KEYS(m))",
    "CREATE INDEX t_mv ON idx_q.t (VALUES(m))",
    "CREATE INDEX t_me ON idx_q.t (ENTRIES(m))",
    "CREATE INDEX t_fl ON idx_q.t (FULL(fl))",
    "CREATE TABLE idx_q.k (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d))",
    "CREATE INDEX k_b ON idx_q.k (b)",
    "CREATE INDEX k_d ON idx_q.k (d)",
    "INSERT INTO idx_q.k (a, b, c, d, v) VALUES (1, 1, 1, 1, 1)",
    "INSERT INTO idx_q.k (a, b, c, d, v) VALUES (2, 1, 1, 2, 1)",
    "INSERT INTO idx_q.k (a, b, c, d, v) VALUES (3, 1, 2, 1, 1)",
    "INSERT INTO idx_q.k (a, b, c, d, v) VALUES (1, 2, 2, 1, 1)",
    "INSERT INTO idx_q.k (a, b, c, d, v) VALUES (1, 1, 3, 1, 2)",
)
wait('idx_q')
SEL = "SELECT pk, ck, v FROM idx_q.t"
for cid, x in [
    ('q_eq', SEL + " WHERE v = 10"),
    ('q_eq_star', "SELECT * FROM idx_q.t WHERE v = 20"),
    ('q_eq_none', SEL + " WHERE v = 99"),
    ('q_eq_null', SEL + " WHERE v = null"),
    ('q_eq_pk', SEL + " WHERE pk = 1 AND v = 10"),
    ('q_eq_pk_ck', SEL + " WHERE pk = 1 AND ck = 2 AND v = 10"),
    ('q_eq_pk_in', SEL + " WHERE pk IN (1, 2) AND v = 10"),
    ('q_eq_ck', SEL + " WHERE ck = 1 AND v = 10"),
    ('q_eq_ck_filtering', SEL + " WHERE ck = 1 AND v = 10 ALLOW FILTERING"),
    ('q_eq_ck_slice', SEL + " WHERE ck > 1 AND v = 10"),
    ('q_eq_ck_slice_filtering', SEL + " WHERE ck > 1 AND v = 10 ALLOW FILTERING"),
    ('q_eq_pk_ck_slice', SEL + " WHERE pk = 1 AND ck > 1 AND v = 10"),
    ('q_eq_token', SEL + " WHERE token(pk) > 0 AND v = 10"),
    ('q_eq_token_range', SEL + " WHERE token(pk) >= -5000000000000000000 AND token(pk) < 0 AND v = 10"),
    ('q_eq_and_unindexed', SEL + " WHERE v = 10 AND w = 1"),
    ('q_eq_and_unindexed_filtering', SEL + " WHERE v = 10 AND w = 1 ALLOW FILTERING"),
    ('q_two_indexed', SEL + " WHERE v = 10 AND txt = 'a'"),
    ('q_two_indexed_filtering', SEL + " WHERE v = 10 AND txt = 'a' ALLOW FILTERING"),
    ('q_in_indexed', SEL + " WHERE v IN (10, 20)"),
    ('q_in_indexed_filtering', SEL + " WHERE v IN (20, 10) ALLOW FILTERING"),
    ('q_range_indexed', SEL + " WHERE v > 10"),
    ('q_range_indexed_filtering', SEL + " WHERE v > 10 ALLOW FILTERING"),
    ('q_neq_indexed', SEL + " WHERE v != 10"),
    ('q_order_by', SEL + " WHERE v = 10 ORDER BY ck DESC"),
    ('q_order_by_pk', SEL + " WHERE pk = 1 AND v = 10 ORDER BY ck DESC"),
    ('q_limit', SEL + " WHERE v = 10 LIMIT 3"),
    ('q_per_partition_limit', SEL + " WHERE v = 10 PER PARTITION LIMIT 1"),
    ('q_distinct', "SELECT DISTINCT pk FROM idx_q.t WHERE v = 10"),
    ('q_count', "SELECT count(*) FROM idx_q.t WHERE v = 10"),
    ('q_group_by', "SELECT pk, count(*) FROM idx_q.t WHERE v = 10 GROUP BY pk"),
    ('q_json', "SELECT JSON pk, ck FROM idx_q.t WHERE v = 20"),
    ('q_ck', SEL + " WHERE ck = 1"),
    ('q_ck_pk', SEL + " WHERE pk = 1 AND ck = 1"),
    ('q_static', "SELECT pk, ck, s FROM idx_q.t WHERE s = 100"),
    ('q_static_distinct', "SELECT DISTINCT pk, s FROM idx_q.t WHERE s = 100"),
    ('q_text', SEL + " WHERE txt = 'a'"),
    ('q_text_empty', SEL + " WHERE txt = ''"),
    ('q_list_contains', SEL + " WHERE l CONTAINS 1"),
    ('q_list_contains_none', SEL + " WHERE l CONTAINS 9"),
    ('q_set_contains', SEL + " WHERE st CONTAINS 'y'"),
    ('q_map_contains_key', SEL + " WHERE m CONTAINS KEY 'b'"),
    ('q_map_contains', SEL + " WHERE m CONTAINS 1"),
    ('q_map_entry', SEL + " WHERE m['a'] = 1"),
    ('q_map_entry_none', SEL + " WHERE m['a'] = 2"),
    ('q_two_contains', SEL + " WHERE l CONTAINS 1 AND l CONTAINS 2"),
    ('q_two_contains_filtering', SEL + " WHERE l CONTAINS 1 AND l CONTAINS 2 ALLOW FILTERING"),
    ('q_frozen_full', SEL + " WHERE fl = [1, 2]"),
    ('q_frozen_contains', SEL + " WHERE fl CONTAINS 1"),
    ('q_pk_component', "SELECT a, b, c, d FROM idx_q.k WHERE b = 1"),
    ('q_pk_component_other', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1"),
    ('q_pk_component_filtering', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1 ALLOW FILTERING"),
    ('q_pk_component_and_ck', "SELECT a, b, c, d FROM idx_q.k WHERE b = 1 AND c = 1"),
    ('q_ck_index_second', "SELECT a, b, c, d FROM idx_q.k WHERE d = 1"),
    ('q_ck_index_full_pk', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1 AND b = 1 AND d = 1"),
    ('q_ck_index_partial_pk', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1 AND d = 1"),
    ('q_ck_index_partial_pk_filtering', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1 AND d = 1 ALLOW FILTERING"),
    ('q_ck_index_with_prefix', "SELECT a, b, c, d FROM idx_q.k WHERE a = 1 AND b = 1 AND c = 1 AND d = 1"),
    ('q_pk_and_ck_index', "SELECT a, b, c, d FROM idx_q.k WHERE b = 1 AND d = 1"),
    ('q_pk_and_ck_index_filtering', "SELECT a, b, c, d FROM idx_q.k WHERE b = 1 AND d = 1 ALLOW FILTERING"),
]:
    q(cid, x)
for n in (1, 2, 3, 5):
    q('q_paged_%d' % n, SEL + " WHERE v = 10", page=n)
q('q_paged_static', "SELECT pk, ck, s FROM idx_q.t WHERE s = 100", page=1)
q('q_paged_list', SEL + " WHERE l CONTAINS 1", page=1)
q('q_paged_limit', SEL + " WHERE v = 10 LIMIT 4", page=3)
q('q_paged_pk_component', "SELECT a, b, c, d FROM idx_q.k WHERE b = 1", page=2)
q('q_paged_ck_index', "SELECT a, b, c, d FROM idx_q.k WHERE d = 1", page=1)
# Maintenance: every write kind.
for cid, w in [
    ('m_update_value', "UPDATE idx_q.t SET v = 20 WHERE pk = 6 AND ck = 1"),
    ('m_update_null', "UPDATE idx_q.t SET v = null WHERE pk = 7 AND ck = 1"),
    ('m_delete_column', "DELETE v FROM idx_q.t WHERE pk = 4 AND ck = 5"),
    ('m_delete_row', "DELETE FROM idx_q.t WHERE pk = 5 AND ck = 1"),
    ('m_delete_range', "DELETE FROM idx_q.t WHERE pk = 1 AND ck >= 2"),
    ('m_delete_partition', "DELETE FROM idx_q.t WHERE pk = 2"),
    ('m_insert_new', "INSERT INTO idx_q.t (pk, ck, v, txt) VALUES (9, 1, 10, 'z')"),
    ('m_older_timestamp', "UPDATE idx_q.t USING TIMESTAMP 1 SET v = 30 WHERE pk = 9 AND ck = 1"),
    ('m_list_append', "UPDATE idx_q.t SET l = l + [9] WHERE pk = 3 AND ck = 1"),
    ('m_list_remove', "UPDATE idx_q.t SET l = l - [1] WHERE pk = 1 AND ck = 1"),
    ('m_set_remove', "UPDATE idx_q.t SET st = st - {'x'} WHERE pk = 1 AND ck = 1"),
    ('m_map_put', "UPDATE idx_q.t SET m['c'] = 3 WHERE pk = 3 AND ck = 1"),
    ('m_map_overwrite', "UPDATE idx_q.t SET m = {'z': 26} WHERE pk = 1 AND ck = 1"),
    ('m_static_update', "UPDATE idx_q.t SET s = 300 WHERE pk = 1"),
    ('m_batch', "BEGIN BATCH UPDATE idx_q.t SET v = 40 WHERE pk = 3 AND ck = 1; UPDATE idx_q.t SET v = 41 WHERE pk = 3 AND ck = 1; INSERT INTO idx_q.t (pk, ck, v) VALUES (10, 1, 40); APPLY BATCH"),
    ('m_lwt', "UPDATE idx_q.t SET v = 50 WHERE pk = 10 AND ck = 1 IF v = 40"),
]:
    q(cid, w)
for cid, x in [
    ('a_eq_10', SEL + " WHERE v = 10"),
    ('a_eq_20', SEL + " WHERE v = 20"),
    ('a_eq_30', SEL + " WHERE v = 30"),
    ('a_eq_40', SEL + " WHERE v = 40"),
    ('a_eq_41', SEL + " WHERE v = 41"),
    ('a_eq_50', SEL + " WHERE v = 50"),
    ('a_static_100', "SELECT pk, ck, s FROM idx_q.t WHERE s = 100"),
    ('a_static_300', "SELECT pk, ck, s FROM idx_q.t WHERE s = 300"),
    ('a_list_1', SEL + " WHERE l CONTAINS 1"),
    ('a_list_2', SEL + " WHERE l CONTAINS 2"),
    ('a_list_9', SEL + " WHERE l CONTAINS 9"),
    ('a_set_x', SEL + " WHERE st CONTAINS 'x'"),
    ('a_set_y', SEL + " WHERE st CONTAINS 'y'"),
    ('a_map_key_a', SEL + " WHERE m CONTAINS KEY 'a'"),
    ('a_map_key_c', SEL + " WHERE m CONTAINS KEY 'c'"),
    ('a_map_key_z', SEL + " WHERE m CONTAINS KEY 'z'"),
    ('a_map_value_26', SEL + " WHERE m CONTAINS 26"),
    ('a_map_entry_c', SEL + " WHERE m['c'] = 3"),
    ('a_ck_1', SEL + " WHERE ck = 1"),
    ('a_txt_a', SEL + " WHERE txt = 'a'"),
]:
    q(cid, x)
q('a_index_view_v', "SELECT * FROM idx_q.t_v_index")
q('a_index_view_l', "SELECT pk, ck FROM idx_q.t_l_index")
q('a_index_view_mk', "SELECT * FROM idx_q.t_mk_index")
q('a_index_view_me', "SELECT * FROM idx_q.t_me_index")
q('a_index_view_s', "SELECT * FROM idx_q.t_s_index")
q('a_index_view_where', "SELECT * FROM idx_q.t_v_index WHERE v = 10")
q('a_truncate', "TRUNCATE idx_q.t")
q('a_after_truncate', SEL + " WHERE v = 10")
q('a_index_view_after_truncate', "SELECT * FROM idx_q.t_v_index")

# Local indexes (ScyllaDB).
setup(
    "CREATE TABLE idx_q.lo (pk int, ck int, v int, PRIMARY KEY (pk, ck))",
    "INSERT INTO idx_q.lo (pk, ck, v) VALUES (1, 1, 10)",
    "INSERT INTO idx_q.lo (pk, ck, v) VALUES (1, 2, 20)",
    "INSERT INTO idx_q.lo (pk, ck, v) VALUES (1, 3, 10)",
    "INSERT INTO idx_q.lo (pk, ck, v) VALUES (2, 1, 10)",
)
q('lo_create', "CREATE INDEX lo_v ON idx_q.lo ((pk), v)")
wait('idx_q')
q('lo_eq_pk', "SELECT * FROM idx_q.lo WHERE pk = 1 AND v = 10")
q('lo_eq_no_pk', "SELECT * FROM idx_q.lo WHERE v = 10")
q('lo_eq_no_pk_filtering', "SELECT * FROM idx_q.lo WHERE v = 10 ALLOW FILTERING")
q('lo_view', "SELECT * FROM idx_q.lo_v_index")
q('lo_view_columns', "SELECT column_name, kind, position, clustering_order FROM system_schema.columns WHERE keyspace_name = 'idx_q' AND table_name = 'lo_v_index'")
q('lo_global_too', "CREATE INDEX lo_v_global ON idx_q.lo (v)")
wait('idx_q')
q('lo_eq_no_pk_global', "SELECT * FROM idx_q.lo WHERE v = 10")
q('lo_eq_pk_both', "SELECT * FROM idx_q.lo WHERE pk = 1 AND v = 10")

# ==== SAI (Cassandra; ScyllaDB maps 'sai' to vector indexes) =======================================
setup(
    "DROP KEYSPACE IF EXISTS idx_s",
    "CREATE KEYSPACE idx_s WITH replication = " + NTS,
    "CREATE TABLE idx_s.t (pk int, ck int, a int, b int, c text, d boolean, u uuid, l list<int>, m map<text, int>, fl frozen<list<int>>, PRIMARY KEY (pk, ck))",
    "INSERT INTO idx_s.t (pk, ck, a, b, c, d, l, m, fl) VALUES (1, 1, 10, 1, 'x', true, [1], {'k': 1}, [1])",
    "INSERT INTO idx_s.t (pk, ck, a, b, c, d, l, m) VALUES (1, 2, 20, 2, 'y', false, [2], {'k': 2})",
    "INSERT INTO idx_s.t (pk, ck, a, b, c, d) VALUES (2, 1, 30, 1, 'x', true)",
    "INSERT INTO idx_s.t (pk, ck, a, b, c) VALUES (3, 1, 10, 2, 'z')",
)
for cid, x in [
    ('sai_create_a', "CREATE INDEX ON idx_s.t (a) USING 'sai'"),
    ('sai_create_b_class', "CREATE CUSTOM INDEX ON idx_s.t (b) USING 'StorageAttachedIndex'"),
    ('sai_create_c_fqcn', "CREATE CUSTOM INDEX c_sai ON idx_s.t (c) USING 'org.apache.cassandra.index.sai.StorageAttachedIndex' WITH OPTIONS = {'case_sensitive': 'false'}"),
    ('sai_create_d', "CREATE INDEX ON idx_s.t (d) USING 'sai'"),
    ('sai_create_l', "CREATE INDEX ON idx_s.t (l) USING 'sai'"),
    ('sai_create_mk', "CREATE INDEX ON idx_s.t (KEYS(m)) USING 'sai'"),
    ('sai_create_fl', "CREATE INDEX ON idx_s.t (FULL(fl)) USING 'sai'"),
    ('sai_create_ck', "CREATE INDEX ON idx_s.t (ck) USING 'sai'"),
    ('sai_dup_column', "CREATE INDEX a_again ON idx_s.t (a) USING 'sai'"),
    ('sai_unknown_option', "CREATE INDEX ON idx_s.t (u) USING 'sai' WITH OPTIONS = {'nope': '1'}"),
    ('sai_analyzer_on_int', "CREATE INDEX ON idx_s.t (u) USING 'sai' WITH OPTIONS = {'case_sensitive': 'false'}"),
    ('sai_vector_option_on_int', "CREATE INDEX ON idx_s.t (u) USING 'sai' WITH OPTIONS = {'similarity_function': 'cosine'}"),
    ('sai_analyzer_on_key', "CREATE INDEX ON idx_s.t (pk) USING 'sai' WITH OPTIONS = {'case_sensitive': 'false'}"),
    ('sai_on_pk_only', "CREATE INDEX ON idx_s.t (pk) USING 'sai'"),
    ('sai_target_option', "CREATE INDEX ON idx_s.t (u) USING 'sai' WITH OPTIONS = {'target': 'a'}"),
]:
    q(cid, x)
wait('idx_s')
q('sai_indexes', "SELECT * FROM system_schema.indexes WHERE keyspace_name = 'idx_s'")
for cid, x in [
    ('sai_q_eq', "SELECT pk, ck FROM idx_s.t WHERE a = 10"),
    ('sai_q_range', "SELECT pk, ck FROM idx_s.t WHERE a > 10"),
    ('sai_q_range_both', "SELECT pk, ck FROM idx_s.t WHERE a >= 10 AND a < 30"),
    ('sai_q_two', "SELECT pk, ck FROM idx_s.t WHERE a = 10 AND b = 2"),
    ('sai_q_two_ranges', "SELECT pk, ck FROM idx_s.t WHERE a > 5 AND b < 2"),
    ('sai_q_text_eq', "SELECT pk, ck FROM idx_s.t WHERE c = 'X'"),
    ('sai_q_text_range', "SELECT pk, ck FROM idx_s.t WHERE c > 'x'"),
    ('sai_q_bool', "SELECT pk, ck FROM idx_s.t WHERE d = true"),
    ('sai_q_contains', "SELECT pk, ck FROM idx_s.t WHERE l CONTAINS 2"),
    ('sai_q_contains_key', "SELECT pk, ck FROM idx_s.t WHERE m CONTAINS KEY 'k'"),
    ('sai_q_full', "SELECT pk, ck FROM idx_s.t WHERE fl = [1]"),
    ('sai_q_ck', "SELECT pk, ck FROM idx_s.t WHERE ck = 1"),
    ('sai_q_ck_range', "SELECT pk, ck FROM idx_s.t WHERE ck > 1"),
    ('sai_q_pk_and', "SELECT pk, ck FROM idx_s.t WHERE pk = 1 AND a = 20"),
    ('sai_q_unindexed', "SELECT pk, ck FROM idx_s.t WHERE a = 10 AND u = 5bd1d1b0-8a9b-11ee-b9d1-0242ac120002"),
    ('sai_q_in', "SELECT pk, ck FROM idx_s.t WHERE a IN (10, 20)"),
    ('sai_q_like', "SELECT pk, ck FROM idx_s.t WHERE c LIKE 'x%'"),
    ('sai_q_order', "SELECT pk, ck FROM idx_s.t WHERE a = 10 ORDER BY ck DESC"),
]:
    q(cid, x)
q('sai_q_paged', "SELECT pk, ck FROM idx_s.t WHERE a >= 10", page=1)
q('sai_q_eq_paged', "SELECT pk, ck FROM idx_s.t WHERE b = 1", page=1)
setup("UPDATE idx_s.t SET a = 11 WHERE pk = 1 AND ck = 1", "DELETE FROM idx_s.t WHERE pk = 3")
q('sai_after_writes', "SELECT pk, ck, a FROM idx_s.t WHERE a > 0")
q('sai_after_writes_eq', "SELECT pk, ck, a FROM idx_s.t WHERE a = 10")
q('sai_drop', "DROP INDEX idx_s.t_a_idx")
q('sai_after_drop', "SELECT pk, ck FROM idx_s.t WHERE a > 0")

# ==== vector search ==================================================================================
setup(
    "DROP KEYSPACE IF EXISTS idx_v",
    "CREATE KEYSPACE idx_v WITH replication = " + NTS,
    "CREATE TABLE idx_v.t (pk int, ck int, v vector<float, 3>, x int, PRIMARY KEY (pk, ck))",
    "CREATE TABLE idx_v.one (pk int PRIMARY KEY, v vector<float, 1>, w vector<int, 2>)",
    "INSERT INTO idx_v.t (pk, ck, v, x) VALUES (1, 1, [1, 0, 0], 1)",
    "INSERT INTO idx_v.t (pk, ck, v, x) VALUES (1, 2, [0, 1, 0], 2)",
    "INSERT INTO idx_v.t (pk, ck, v, x) VALUES (2, 1, [1, 1, 0], 3)",
    "INSERT INTO idx_v.t (pk, ck, v, x) VALUES (3, 1, [1, 0, 0], 4)",
    "INSERT INTO idx_v.t (pk, ck, v, x) VALUES (4, 1, [0.5, 0.5, 0.1], 5)",
    "INSERT INTO idx_v.t (pk, ck, x) VALUES (5, 1, 6)",
)
ANN = "SELECT pk, ck, x FROM idx_v.t"
q('v_ann_no_index', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 3")
for cid, x in [
    ('v_create_sai', "CREATE INDEX v_idx ON idx_v.t (v) USING 'sai'"),
    ('v_create_vector_index_dup', "CREATE CUSTOM INDEX v_vs ON idx_v.t (v) USING 'vector_index'"),
    ('v_create_bad_similarity', "CREATE INDEX v_bad ON idx_v.one (v) USING 'sai' WITH OPTIONS = {'similarity_function': 'NOPE'}"),
    ('v_create_one_dim_cosine', "CREATE INDEX ON idx_v.one (v) USING 'sai'"),
    ('v_create_int_vector', "CREATE INDEX ON idx_v.one (w) USING 'sai'"),
    ('v_create_vector_index_option', "CREATE CUSTOM INDEX ON idx_v.one (w) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'euclidean'}"),
]:
    q(cid, x)
wait('idx_v')
q('v_indexes', "SELECT keyspace_name, table_name, index_name, kind FROM system_schema.indexes WHERE keyspace_name = 'idx_v'")
for cid, x in [
    ('v_ann', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_all', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 10"),
    ('v_ann_similarity', "SELECT pk, ck, similarity_cosine(v, [1, 0, 0]) FROM idx_v.t ORDER BY v ANN OF [1, 0, 0] LIMIT 10"),
    ('v_ann_other_vector', ANN + " ORDER BY v ANN OF [0, 1, 0] LIMIT 2"),
    ('v_ann_no_limit', ANN + " ORDER BY v ANN OF [1, 0, 0]"),
    ('v_ann_limit_too_big', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 1001"),
    ('v_ann_pk', ANN + " WHERE pk = 1 ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_pk_ck', ANN + " WHERE pk = 1 AND ck = 2 ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_ck', ANN + " WHERE ck = 1 ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_unindexed', ANN + " WHERE x = 1 ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_unindexed_filtering', ANN + " WHERE x = 1 ORDER BY v ANN OF [1, 0, 0] LIMIT 3 ALLOW FILTERING"),
    ('v_ann_token', ANN + " WHERE token(pk) > 0 ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_wrong_dim', ANN + " ORDER BY v ANN OF [1, 0] LIMIT 3"),
    ('v_ann_non_vector', ANN + " ORDER BY x ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_two_orderings', ANN + " ORDER BY v ANN OF [1, 0, 0], ck ASC LIMIT 3"),
    ('v_ann_desc', ANN + " ORDER BY v ANN OF [1, 0, 0] DESC LIMIT 3"),
    ('v_ann_count', "SELECT count(*) FROM idx_v.t ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_ann_group_by', "SELECT pk, count(*) FROM idx_v.t GROUP BY pk ORDER BY v ANN OF [1, 0, 0] LIMIT 3"),
    ('v_eq_vector', ANN + " WHERE v = [1, 0, 0] ALLOW FILTERING"),
    ('v_ann_null', ANN + " ORDER BY v ANN OF null LIMIT 3"),
]:
    q(cid, x)
q('v_ann_paged', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 4", page=2)
q('v_ann_paged_bigger', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 2", page=5)
q('v_ann_quorum', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 2", cl='QUORUM')
q('v_ann_serial', ANN + " ORDER BY v ANN OF [1, 0, 0] LIMIT 2", cl='SERIAL')
setup("CREATE TABLE idx_v.e (pk int PRIMARY KEY, v vector<float, 2>)",
      "CREATE INDEX ON idx_v.e (v) USING 'sai' WITH OPTIONS = {'similarity_function': 'euclidean'}",
      "INSERT INTO idx_v.e (pk, v) VALUES (1, [0, 0])",
      "INSERT INTO idx_v.e (pk, v) VALUES (2, [3, 4])",
      "INSERT INTO idx_v.e (pk, v) VALUES (3, [1, 1])",
      "CREATE TABLE idx_v.dp (pk int PRIMARY KEY, v vector<float, 2>)",
      "CREATE INDEX ON idx_v.dp (v) USING 'sai' WITH OPTIONS = {'similarity_function': 'DOT_PRODUCT'}",
      "INSERT INTO idx_v.dp (pk, v) VALUES (1, [0.6, 0.8])",
      "INSERT INTO idx_v.dp (pk, v) VALUES (2, [0.8, 0.6])",
      "INSERT INTO idx_v.dp (pk, v) VALUES (3, [1, 0])")
wait('idx_v')
q('v_euclidean', "SELECT pk FROM idx_v.e ORDER BY v ANN OF [3, 3] LIMIT 3")
q('v_dot_product', "SELECT pk FROM idx_v.dp ORDER BY v ANN OF [1, 0] LIMIT 3")

# ==== materialized views =============================================================================
setup(
    "DROP KEYSPACE IF EXISTS idx_m",
    "CREATE KEYSPACE idx_m WITH replication = " + NTS,
    "CREATE TABLE idx_m.b (pk int, ck int, v int, w text, x int, l list<int>, fm frozen<map<int, int>>, PRIMARY KEY (pk, ck)) WITH CLUSTERING ORDER BY (ck DESC)",
    "CREATE TABLE idx_m.st (pk int, ck int, s int static, v int, PRIMARY KEY (pk, ck))",
    "CREATE TABLE idx_m.one (pk int PRIMARY KEY, v int, w int)",
    "CREATE TABLE idx_m.cnt (pk int PRIMARY KEY, c counter)",
    "CREATE TABLE idx_m.k (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d))",
    "INSERT INTO idx_m.b (pk, ck, v, w, x, l, fm) VALUES (1, 1, 10, 'a', 1, [1], {1: 1}) USING TIMESTAMP 1000",
    "INSERT INTO idx_m.b (pk, ck, v, w) VALUES (1, 2, 20, 'b') USING TIMESTAMP 1000",
    "INSERT INTO idx_m.b (pk, ck, w) VALUES (2, 1, 'c') USING TIMESTAMP 1000",
    "UPDATE idx_m.b USING TIMESTAMP 1000 SET v = 30 WHERE pk = 3 AND ck = 1",
)
MV = "CREATE MATERIALIZED VIEW idx_m."
for cid, x in [
    ('mv_by_v', MV + "by_v AS SELECT * FROM idx_m.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)"),
    ('mv_by_ck', MV + "by_ck AS SELECT pk, ck, w FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (pk DESC) AND comment = 'by ck'"),
    ('mv_filter_pk', MV + "pk1 AS SELECT pk, ck, v FROM idx_m.b WHERE pk = 1 AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_filter_ck_range', MV + "ck_gt AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND ck > 1 PRIMARY KEY (ck, pk)"),
    ('mv_filter_ck_in', MV + "ck_in AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IN (1, 2) PRIMARY KEY (ck, pk)"),
    ('mv_composite_pk', MV + "comp AS SELECT * FROM idx_m.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY ((v, pk), ck)"),
    ('mv_frozen_in_pk', MV + "by_fm AS SELECT pk, ck, fm FROM idx_m.b WHERE fm IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (fm, pk, ck)"),
    ('mv_same_pk', MV + "same AS SELECT * FROM idx_m.one WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_same_pk_subset', MV + "same_v AS SELECT pk, v FROM idx_m.one WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_k', MV + "k_by_d AS SELECT * FROM idx_m.k WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL PRIMARY KEY (d, a, b, c)"),
    ('mv_token_filter', MV + "tok AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND token(pk) > 0 PRIMARY KEY (ck, pk)"),
    ('mv_exists', MV + "by_v AS SELECT * FROM idx_m.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)"),
    ('mv_exists_if_not_exists', "CREATE MATERIALIZED VIEW IF NOT EXISTS idx_m.by_v AS SELECT * FROM idx_m.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)"),
    ('mv_named_like_table', MV + "one AS SELECT * FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_unknown_base', MV + "x AS SELECT * FROM idx_m.nope WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_unknown_keyspace', "CREATE MATERIALIZED VIEW idx_nope.x AS SELECT * FROM idx_nope.b WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_other_keyspace', "CREATE MATERIALIZED VIEW idx_q.x AS SELECT * FROM idx_m.one WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_counter', MV + "x AS SELECT * FROM idx_m.cnt WHERE pk IS NOT NULL PRIMARY KEY (pk)"),
    ('mv_on_view', MV + "x AS SELECT * FROM idx_m.by_v WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (pk, v, ck)"),
    ('mv_static_star', MV + "x AS SELECT * FROM idx_m.st WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_static_explicit', MV + "x AS SELECT pk, ck, s FROM idx_m.st WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_unknown_column', MV + "x AS SELECT pk, ck, nope FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_alias', MV + "x AS SELECT pk AS p, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_function', MV + "x AS SELECT pk, ck, ttl(v) FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_pk_not_selected', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck)"),
    ('mv_pk_unknown', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, nope)"),
    ('mv_pk_duplicate', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk, ck)"),
    ('mv_missing_base_pk', MV + "x AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk)"),
    ('mv_two_regular', MV + "x AS SELECT * FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL AND x IS NOT NULL PRIMARY KEY (v, x, pk, ck)"),
    ('mv_collection_pk', MV + "x AS SELECT pk, ck, l FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND l IS NOT NULL PRIMARY KEY (l, pk, ck)"),
    ('mv_missing_not_null', MV + "x AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)"),
    ('mv_missing_not_null_pk', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_filter_regular', MV + "x AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v = 1 PRIMARY KEY (ck, pk)"),
    ('mv_regular_not_null_not_pk', MV + "x AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_bind_marker', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck = ? PRIMARY KEY (ck, pk)"),
    ('mv_clustering_order_unknown', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (nope DESC)"),
    ('mv_clustering_order_wrong', MV + "x AS SELECT pk, ck, v FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck) WITH CLUSTERING ORDER BY (ck DESC)"),
    ('mv_ttl', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH default_time_to_live = 5"),
    ('mv_unknown_option', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH nope = 1"),
    ('mv_options', MV + "opts AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH gc_grace_seconds = 5 AND synchronous_updates = true"),
    ('mv_custom_expression', MV + "x AS SELECT pk, ck FROM idx_m.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND expr(i, 'x') PRIMARY KEY (ck, pk)"),
]:
    q(cid, x)
wait('idx_m')
q('mv_rows_views', "SELECT keyspace_name, view_name, base_table_name, include_all_columns, where_clause, comment, gc_grace_seconds, default_time_to_live FROM system_schema.views WHERE keyspace_name = 'idx_m'")
q('mv_rows_columns', "SELECT table_name, column_name, clustering_order, kind, position, type FROM system_schema.columns WHERE keyspace_name = 'idx_m' AND table_name IN ('by_v', 'by_ck', 'comp', 'same_v', 'k_by_d')")
q('mv_rows_virtual', "SELECT * FROM system_schema.view_virtual_columns WHERE keyspace_name = 'idx_m'")
q('mv_rows_tables', "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'idx_m'")
q('mv_built', "SELECT * FROM system.built_views WHERE keyspace_name = 'idx_m'")
for cid, x in [
    ('mv_sel_by_v', "SELECT * FROM idx_m.by_v"),
    ('mv_sel_by_v_where', "SELECT pk, ck, w FROM idx_m.by_v WHERE v = 10"),
    ('mv_sel_by_v_writetime', "SELECT v, pk, ck, writetime(w) FROM idx_m.by_v"),
    ('mv_sel_by_ck', "SELECT * FROM idx_m.by_ck"),
    ('mv_sel_pk1', "SELECT * FROM idx_m.pk1"),
    ('mv_sel_ck_gt', "SELECT * FROM idx_m.ck_gt"),
    ('mv_sel_ck_in', "SELECT * FROM idx_m.ck_in"),
    ('mv_sel_comp', "SELECT * FROM idx_m.comp WHERE v = 10 AND pk = 1"),
    ('mv_sel_by_fm', "SELECT * FROM idx_m.by_fm"),
    ('mv_sel_tok', "SELECT * FROM idx_m.tok"),
    ('mv_sel_paged', "SELECT * FROM idx_m.by_v"),
    ('mv_sel_filtering', "SELECT * FROM idx_m.by_v WHERE w = 'a' ALLOW FILTERING"),
    ('mv_sel_unknown_column', "SELECT x FROM idx_m.by_ck"),
]:
    q(cid, x)
q('mv_sel_by_v_paged', "SELECT * FROM idx_m.by_v", page=1)
for cid, x in [
    ('mv_write_view', "INSERT INTO idx_m.by_v (v, pk, ck) VALUES (1, 1, 1)"),
    ('mv_update_view', "UPDATE idx_m.by_v SET w = 'x' WHERE v = 1 AND pk = 1 AND ck = 1"),
    ('mv_delete_view', "DELETE FROM idx_m.by_v WHERE v = 1"),
    ('mv_truncate_view', "TRUNCATE idx_m.by_v"),
    ('mv_alter_table_on_view', "ALTER TABLE idx_m.by_v WITH comment = 'x'"),
    ('mv_drop_table_on_view', "DROP TABLE idx_m.by_v"),
    ('mv_index_on_view', "CREATE INDEX ON idx_m.by_v (w)"),
    ('mv_drop_base_column', "ALTER TABLE idx_m.b DROP w"),
    ('mv_drop_base_column_unselected', "ALTER TABLE idx_m.one DROP w"),
    ('mv_add_base_column', "ALTER TABLE idx_m.b ADD y int"),
    ('mv_drop_base', "DROP TABLE idx_m.b"),
]:
    q(cid, x)
q('mv_sel_by_v_after_add', "SELECT * FROM idx_m.by_v")
# Maintenance.
for cid, w in [
    ('mvw_update_key_column', "UPDATE idx_m.b USING TIMESTAMP 2000 SET v = 11 WHERE pk = 1 AND ck = 1"),
    ('mvw_update_regular', "UPDATE idx_m.b USING TIMESTAMP 2000 SET w = 'bb' WHERE pk = 1 AND ck = 2"),
    ('mvw_set_key_null', "UPDATE idx_m.b USING TIMESTAMP 2000 SET v = null WHERE pk = 3 AND ck = 1"),
    ('mvw_set_key_on_unset', "UPDATE idx_m.b USING TIMESTAMP 2000 SET v = 40 WHERE pk = 2 AND ck = 1"),
    ('mvw_older_timestamp', "UPDATE idx_m.b USING TIMESTAMP 1500 SET v = 99 WHERE pk = 1 AND ck = 1"),
    ('mvw_insert_new', "INSERT INTO idx_m.b (pk, ck, v, w) VALUES (4, 1, 10, 'd') USING TIMESTAMP 2000"),
    ('mvw_delete_row', "DELETE FROM idx_m.b USING TIMESTAMP 2001 WHERE pk = 4 AND ck = 1"),
    ('mvw_reinsert_older', "INSERT INTO idx_m.b (pk, ck, v, w) VALUES (4, 1, 10, 'd') USING TIMESTAMP 2000"),
    ('mvw_reinsert_newer', "INSERT INTO idx_m.b (pk, ck, v, w) VALUES (4, 1, 12, 'e') USING TIMESTAMP 2002"),
    ('mvw_list_append', "UPDATE idx_m.b USING TIMESTAMP 2003 SET l = l + [5] WHERE pk = 1 AND ck = 2"),
    ('mvw_range_delete', "DELETE FROM idx_m.b USING TIMESTAMP 3000 WHERE pk = 1 AND ck < 2"),
    ('mvw_partition_delete', "DELETE FROM idx_m.b USING TIMESTAMP 3000 WHERE pk = 2"),
    ('mvw_batch', "BEGIN BATCH USING TIMESTAMP 4000 INSERT INTO idx_m.b (pk, ck, v) VALUES (5, 1, 50); UPDATE idx_m.b SET v = 51 WHERE pk = 5 AND ck = 1; APPLY BATCH"),
    ('mvw_same_pk_insert', "INSERT INTO idx_m.one (pk, v, w) VALUES (1, 1, 1) USING TIMESTAMP 1000"),
    ('mvw_same_pk_update_unselected', "UPDATE idx_m.one USING TIMESTAMP 1000 SET w = 2 WHERE pk = 2"),
    ('mvw_same_pk_delete_unselected', "DELETE w FROM idx_m.one USING TIMESTAMP 1001 WHERE pk = 2"),
    ('mvw_ttl', "INSERT INTO idx_m.b (pk, ck, v, w) VALUES (6, 1, 60, 'ttl') USING TTL 100000"),
]:
    q(cid, w)
for cid, x in [
    ('mva_by_v', "SELECT v, pk, ck, w, x FROM idx_m.by_v"),
    ('mva_by_v_writetime', "SELECT v, pk, ck, writetime(w) FROM idx_m.by_v WHERE v = 11"),
    ('mva_by_ck', "SELECT * FROM idx_m.by_ck"),
    ('mva_pk1', "SELECT * FROM idx_m.pk1"),
    ('mva_ck_gt', "SELECT * FROM idx_m.ck_gt"),
    ('mva_comp', "SELECT v, pk, ck, w FROM idx_m.comp"),
    ('mva_same', "SELECT * FROM idx_m.same"),
    ('mva_same_v', "SELECT * FROM idx_m.same_v"),
    ('mva_by_v_l', "SELECT pk, ck, l FROM idx_m.by_v WHERE v = 20"),
    ('mva_ttl_present', "SELECT pk, ck, w FROM idx_m.by_v WHERE v = 60"),
]:
    q(cid, x)
for cid, x in [
    ('amv_comment', "ALTER MATERIALIZED VIEW idx_m.by_v WITH comment = 'altered'"),
    ('amv_ttl', "ALTER MATERIALIZED VIEW idx_m.by_v WITH default_time_to_live = 10"),
    ('amv_unknown_option', "ALTER MATERIALIZED VIEW idx_m.by_v WITH nope = 1"),
    ('amv_on_table', "ALTER MATERIALIZED VIEW idx_m.one WITH comment = 'x'"),
    ('amv_unknown', "ALTER MATERIALIZED VIEW idx_m.nope WITH comment = 'x'"),
    ('amv_gc_zero', "ALTER MATERIALIZED VIEW idx_m.by_v WITH gc_grace_seconds = 0"),
    ('amv_id', "ALTER MATERIALIZED VIEW idx_m.by_v WITH id = 5a1c395e-b41f-11e5-9f22-ba0be0483c18"),
]:
    q(cid, x)
q('amv_rows', "SELECT view_name, comment, gc_grace_seconds FROM system_schema.views WHERE keyspace_name = 'idx_m' AND view_name = 'by_v'")
for cid, x in [
    ('dmv_on_table', "DROP MATERIALIZED VIEW idx_m.one"),
    ('dmv_unknown', "DROP MATERIALIZED VIEW idx_m.nope"),
    ('dmv_unknown_if_exists', "DROP MATERIALIZED VIEW IF EXISTS idx_m.nope"),
    ('dmv_existing', "DROP MATERIALIZED VIEW idx_m.tok"),
    ('dmv_select_after', "SELECT * FROM idx_m.tok"),
    ('dmv_base_still_has_views', "DROP TABLE idx_m.b"),
    ('dmv_rest_one', "DROP MATERIALIZED VIEW idx_m.same"),
    ('dmv_rest_two', "DROP MATERIALIZED VIEW idx_m.same_v"),
    ('dmv_base_free', "DROP TABLE idx_m.one"),
]:
    q(cid, x)

# ==== CDC (ScyllaDB; Cassandra stores the `cdc` flag) ================================================
setup(
    "DROP KEYSPACE IF EXISTS idx_c",
    "CREATE KEYSPACE idx_c WITH replication = " + NTS,
)
for cid, x in [
    ('cdc_create', "CREATE TABLE idx_c.t (pk int, ck int, v int, w text, s int static, l list<int>, m map<int, text>, st set<int>, fm frozen<map<int, int>>, PRIMARY KEY (pk, ck)) WITH cdc = {'enabled': true}"),
    ('cdc_create_cassandra', "CREATE TABLE idx_c.cs (pk int PRIMARY KEY, v int) WITH cdc = true"),
    ('cdc_create_images', "CREATE TABLE idx_c.img (pk int, ck int, v int, w text, s int static, PRIMARY KEY (pk, ck)) WITH cdc = {'enabled': true, 'preimage': true, 'postimage': true, 'ttl': 3600}"),
    ('cdc_create_full_preimage', "CREATE TABLE idx_c.fullpre (pk int, ck int, v int, w int, PRIMARY KEY (pk, ck)) WITH cdc = {'enabled': true, 'preimage': 'full'}"),
    ('cdc_create_no_ttl', "CREATE TABLE idx_c.nottl (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true, 'ttl': 0}"),
    ('cdc_create_udt_list', "CREATE TABLE idx_c.adv (pk int PRIMARY KEY, u frozen<map<int, int>>, s set<text>, l list<text>) WITH cdc = {'enabled': true, 'postimage': true}"),
    ('cdc_create_delta_keys', "CREATE TABLE idx_c.keys (pk int, ck int, v int, PRIMARY KEY (pk, ck)) WITH cdc = {'enabled': true, 'delta': 'keys'}"),
    ('cdc_bad_option', "CREATE TABLE idx_c.x (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true, 'nope': 1}"),
    ('cdc_bad_ttl', "CREATE TABLE idx_c.x (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true, 'ttl': -1}"),
    ('cdc_bad_enabled', "CREATE TABLE idx_c.x (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': 'maybe'}"),
    ('cdc_bad_preimage', "CREATE TABLE idx_c.x (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true, 'preimage': 'some'}"),
    ('cdc_bad_delta', "CREATE TABLE idx_c.x (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true, 'delta': 'some'}"),
    ('cdc_counter', "CREATE TABLE idx_c.cnt (pk int PRIMARY KEY, c counter) WITH cdc = {'enabled': true}"),
    ('cdc_log_name_taken', "CREATE TABLE idx_c.t_scylla_cdc_log (pk int PRIMARY KEY)"),
    ('cdc_disabled_table', "CREATE TABLE idx_c.off (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': false}"),
    ('cdc_enable_later', "ALTER TABLE idx_c.off WITH cdc = {'enabled': true}"),
]:
    q(cid, x)
q('cdc_tables', "SELECT table_name, comment, caching, compaction, gc_grace_seconds, default_time_to_live, extensions FROM system_schema.tables WHERE keyspace_name = 'idx_c'")
q('cdc_log_columns', "SELECT table_name, column_name, clustering_order, kind, position, type FROM system_schema.columns WHERE keyspace_name = 'idx_c' AND table_name IN ('t_scylla_cdc_log', 'img_scylla_cdc_log')")
q('cdc_scylla_tables', "SELECT table_name, cdc, partitioner FROM system_schema.scylla_tables WHERE keyspace_name = 'idx_c'")
LOG = 'SELECT "cdc$batch_seq_no", "cdc$end_of_batch", "cdc$operation", "cdc$ttl", pk, ck, v, "cdc$deleted_v", w, "cdc$deleted_w", s, "cdc$deleted_s" FROM idx_c.%s_scylla_cdc_log'
LOGC = 'SELECT "cdc$batch_seq_no", "cdc$operation", pk, ck, l, "cdc$deleted_l", "cdc$deleted_elements_l", m, "cdc$deleted_m", "cdc$deleted_elements_m", st, "cdc$deleted_st", "cdc$deleted_elements_st", fm, "cdc$deleted_fm" FROM idx_c.t_scylla_cdc_log'
for cid, w in [
    ('cw_insert', "INSERT INTO idx_c.t (pk, ck, v, w) VALUES (1, 1, 10, 'a')"),
    ('cw_insert_static', "INSERT INTO idx_c.t (pk, ck, v, s) VALUES (1, 2, 20, 5)"),
    ('cw_update', "UPDATE idx_c.t SET v = 11, w = null WHERE pk = 1 AND ck = 1"),
    ('cw_update_ttl', "UPDATE idx_c.t USING TTL 100 SET v = 12 WHERE pk = 1 AND ck = 1"),
    ('cw_update_ttl_mixed', "UPDATE idx_c.t USING TTL 100 SET v = 13, w = null WHERE pk = 1 AND ck = 1"),
    ('cw_update_static', "UPDATE idx_c.t SET s = 6 WHERE pk = 1"),
    ('cw_delete_column', "DELETE w FROM idx_c.t WHERE pk = 1 AND ck = 1"),
    ('cw_delete_row', "DELETE FROM idx_c.t WHERE pk = 1 AND ck = 2"),
    ('cw_delete_range', "DELETE FROM idx_c.t WHERE pk = 1 AND ck > 5 AND ck <= 9"),
    ('cw_delete_range_open', "DELETE FROM idx_c.t WHERE pk = 1 AND ck < 3"),
    ('cw_delete_partition', "DELETE FROM idx_c.t WHERE pk = 1"),
    ('cw_batch', "BEGIN UNLOGGED BATCH UPDATE idx_c.t SET v = 1 WHERE pk = 2 AND ck = 1; UPDATE idx_c.t SET v = 2 WHERE pk = 2 AND ck = 2; APPLY BATCH"),
]:
    q(cid, w)
q('cdc_log_basic', LOG % 't', sort=True)
setup("TRUNCATE idx_c.t_scylla_cdc_log")
for cid, w in [
    ('cw_list_append', "UPDATE idx_c.t SET l = l + [7] WHERE pk = 3 AND ck = 1"),
    ('cw_map_put', "UPDATE idx_c.t SET m = m + {1: 'x'} WHERE pk = 3 AND ck = 1"),
    ('cw_set_add', "UPDATE idx_c.t SET st = st + {3} WHERE pk = 3 AND ck = 1"),
    ('cw_set_remove', "UPDATE idx_c.t SET st = st - {3, 4} WHERE pk = 3 AND ck = 1"),
    ('cw_map_overwrite', "UPDATE idx_c.t SET m = {2: 'y'} WHERE pk = 3 AND ck = 1"),
    ('cw_map_delete', "UPDATE idx_c.t SET m = null WHERE pk = 3 AND ck = 1"),
    ('cw_map_remove_key', "DELETE m[2] FROM idx_c.t WHERE pk = 3 AND ck = 1"),
    ('cw_frozen', "UPDATE idx_c.t SET fm = {1: 1} WHERE pk = 3 AND ck = 1"),
    ('cw_insert_collections', "INSERT INTO idx_c.t (pk, ck, m, st) VALUES (3, 2, {5: 'z'}, {9})"),
]:
    q(cid, w)
q('cdc_log_collections', LOGC, sort=True, mask=['l', 'cdc$deleted_elements_l'])
for cid, w in [
    ('ci_insert', "INSERT INTO idx_c.img (pk, ck, v, w, s) VALUES (1, 1, 10, 'a', 5)"),
    ('ci_update', "UPDATE idx_c.img SET v = 11, w = null WHERE pk = 1 AND ck = 1"),
    ('ci_update_one', "UPDATE idx_c.img SET v = 12 WHERE pk = 1 AND ck = 1"),
    ('ci_delete_row', "DELETE FROM idx_c.img WHERE pk = 1 AND ck = 1"),
    ('ci_insert_again', "INSERT INTO idx_c.img (pk, ck, v) VALUES (2, 1, 20)"),
    ('ci_delete_partition', "DELETE FROM idx_c.img WHERE pk = 2"),
]:
    q(cid, w)
q('cdc_log_images', LOG % 'img', sort=True)
q('cdc_log_ttl', 'SELECT pk, ck, ttl(v) FROM idx_c.img_scylla_cdc_log WHERE "cdc$operation" = 2 ALLOW FILTERING', mask=['ttl(v)'], sort=True)
for cid, w in [
    ('cf_insert', "INSERT INTO idx_c.fullpre (pk, ck, v, w) VALUES (1, 1, 1, 1)"),
    ('cf_update', "UPDATE idx_c.fullpre SET v = 2 WHERE pk = 1 AND ck = 1"),
    ('ck_insert', "INSERT INTO idx_c.keys (pk, ck, v) VALUES (1, 1, 1)"),
    ('ck_update', "UPDATE idx_c.keys SET v = 2 WHERE pk = 1 AND ck = 1"),
]:
    q(cid, w)
for cid, w in [
    ('ca_insert', "INSERT INTO idx_c.adv (pk, u, s, l) VALUES (1, {1: 1}, {'a', 'b'}, ['x'])"),
    ('ca_update', "UPDATE idx_c.adv SET s = s - {'a'}, u = null WHERE pk = 1"),
    ('ca_ttl_insert', "INSERT INTO idx_c.adv (pk, s) VALUES (2, {'z'}) USING TTL 50"),
]:
    q(cid, w)
q('cdc_log_adv', 'SELECT "cdc$batch_seq_no", "cdc$end_of_batch", "cdc$operation", "cdc$ttl", pk, u, "cdc$deleted_u", s, "cdc$deleted_s", "cdc$deleted_elements_s", "cdc$deleted_l" FROM idx_c.adv_scylla_cdc_log', sort=True)
q('cdc_log_full', 'SELECT "cdc$batch_seq_no", "cdc$operation", pk, ck, v, w FROM idx_c.fullpre_scylla_cdc_log', sort=True)
q('cdc_log_keys', 'SELECT "cdc$batch_seq_no", "cdc$operation", pk, ck, v FROM idx_c.keys_scylla_cdc_log', sort=True)
q('cdc_log_types', 'SELECT * FROM idx_c.keys_scylla_cdc_log', mask=['cdc$stream_id', 'cdc$time'])
q('cdc_log_tounix', 'SELECT pk FROM idx_c.keys_scylla_cdc_log WHERE "cdc$operation" = 2 ALLOW FILTERING')
for cid, x in [
    ('cdc_write_log', "INSERT INTO idx_c.t_scylla_cdc_log (\"cdc$stream_id\", \"cdc$time\", \"cdc$batch_seq_no\") VALUES (0x00, now(), 0)"),
    ('cdc_drop_log', "DROP TABLE idx_c.t_scylla_cdc_log"),
    ('cdc_alter_log', "ALTER TABLE idx_c.t_scylla_cdc_log WITH comment = 'x'"),
    ('cdc_disable', "ALTER TABLE idx_c.t WITH cdc = {'enabled': false}"),
    ('cdc_write_disabled', "INSERT INTO idx_c.t (pk, ck, v) VALUES (9, 9, 9)"),
    ('cdc_log_after_disable', 'SELECT pk FROM idx_c.t_scylla_cdc_log WHERE pk = 9 ALLOW FILTERING'),
    ('cdc_tables_after_disable', "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'idx_c'"),
    ('cdc_drop_base', "DROP TABLE idx_c.img"),
    ('cdc_tables_after_drop', "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'idx_c'"),
    ('cdc_cassandra_flag', "SELECT table_name, cdc FROM system_schema.tables WHERE keyspace_name = 'idx_c' AND table_name = 'cs'"),
    ('cdc_generation_columns', "SELECT key FROM system_distributed.cdc_generation_timestamps"),
    ('cdc_streams_columns', "SELECT * FROM system_distributed.cdc_streams_descriptions_v2 LIMIT 0"),
    ('cdc_tablet_streams_columns', "SELECT * FROM system.cdc_streams LIMIT 0"),
    ('cdc_tablet_timestamps_columns', "SELECT * FROM system.cdc_timestamps LIMIT 0"),
]:
    q(cid, x)

# Vector Search reads the CDC log (ScyllaDB): the first vector index turns logging on.
setup("CREATE TABLE idx_c.vs (pk int PRIMARY KEY, v vector<float, 2>, x int)",
      "CREATE TABLE idx_c.vs_off (pk int PRIMARY KEY, v vector<float, 2>) WITH cdc = {'enabled': false}",
      "CREATE TABLE idx_c.vs_ttl (pk int PRIMARY KEY, v vector<float, 2>) WITH cdc = {'enabled': true, 'ttl': 100}",
      "CREATE TABLE idx_c.vs_keys (pk int PRIMARY KEY, v vector<float, 2>) WITH cdc = {'enabled': true, 'delta': 'keys', 'postimage': true}",
      "CREATE TABLE idx_c.vs_clash (pk int PRIMARY KEY, v vector<float, 2>)",
      "CREATE TABLE idx_c.vs_clash_scylla_cdc_log (pk int PRIMARY KEY)",
      "CREATE TABLE idx_c.vs2 (pk int PRIMARY KEY, v vector<float, 2>)",
      "CREATE TABLE idx_c.vs3 (pk int PRIMARY KEY, v vector<float, 2>)")
VS_TABLES = "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'idx_c'"
for cid, x in [
    ('vs_create', "CREATE INDEX vs_v ON idx_c.vs (v) USING 'vector_index'"),
    ('vs_tables', VS_TABLES),
    ('vs_log_table', "SELECT table_name, comment, compaction, default_time_to_live, extensions, gc_grace_seconds FROM system_schema.tables WHERE keyspace_name = 'idx_c' AND table_name IN ('vs', 'vs_scylla_cdc_log')"),
    ('vs_insert', "INSERT INTO idx_c.vs (pk, v, x) VALUES (1, [1, 2], 3)"),
    ('vs_update', "UPDATE idx_c.vs SET x = 4 WHERE pk = 1"),
    ('vs_log', 'SELECT "cdc$batch_seq_no", "cdc$operation", "cdc$ttl", pk, v, x FROM idx_c.vs_scylla_cdc_log'),
    ('vs_drop_log', "DROP TABLE idx_c.vs_scylla_cdc_log"),
    ('vs_add_column', "ALTER TABLE idx_c.vs ADD y int"),
    ('vs_log_columns', "SELECT column_name, type FROM system_schema.columns WHERE keyspace_name = 'idx_c' AND table_name = 'vs_scylla_cdc_log'"),
    ('vs_disable', "ALTER TABLE idx_c.vs WITH cdc = {'enabled': false}"),
    ('vs_short_ttl', "ALTER TABLE idx_c.vs WITH cdc = {'enabled': true, 'ttl': 100}"),
    ('vs_keys_only', "ALTER TABLE idx_c.vs WITH cdc = {'enabled': true, 'delta': 'keys'}"),
    ('vs_keys_postimage', "ALTER TABLE idx_c.vs WITH cdc = {'enabled': true, 'delta': 'keys', 'postimage': true}"),
    ('vs_no_ttl', "ALTER TABLE idx_c.vs WITH cdc = {'enabled': true, 'ttl': 0}"),
    ('vs_second', "CREATE INDEX vs_v2 ON idx_c.vs (v) USING 'vector_index'"),
    ('vs_off_create', "CREATE INDEX vs_off_v ON idx_c.vs_off (v) USING 'vector_index'"),
    ('vs_off_create_bad_option', "CREATE INDEX vs_off_v ON idx_c.vs_off (v) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'bogus'}"),
    ('vs_off_create_int', "CREATE INDEX vs_off_pk ON idx_c.vs_off (pk) USING 'vector_index'"),
    ('vs_ttl_create', "CREATE INDEX IF NOT EXISTS vs_ttl_v ON idx_c.vs_ttl (v) USING 'vector_index'"),
    ('vs_keys_create', "CREATE INDEX vs_keys_v ON idx_c.vs_keys (v) USING 'vector_index'"),
    ('vs_clash_create', "CREATE INDEX vs_clash_v ON idx_c.vs_clash (v) USING 'vector_index'"),
    ('vs2_create', "CREATE INDEX vs2_v ON idx_c.vs2 (v) USING 'vector_index'"),
    ('vs2_drop_index', "DROP INDEX idx_c.vs2_v"),
    ('vs2_insert', "INSERT INTO idx_c.vs2 (pk, v) VALUES (1, [1, 2])"),
    ('vs2_log', 'SELECT "cdc$operation", pk FROM idx_c.vs2_scylla_cdc_log'),
    ('vs2_drop_log', "DROP TABLE idx_c.vs2_scylla_cdc_log"),
    ('vs3_create', "CREATE INDEX vs3_v ON idx_c.vs3 (v) USING 'vector_index'"),
    ('vs3_drop_index', "DROP INDEX idx_c.vs3_v"),
    ('vs3_drop_base', "DROP TABLE idx_c.vs3"),
    ('vs_tables_after', VS_TABLES),
]:
    q(cid, x)

setup("DROP KEYSPACE IF EXISTS idx_i", "DROP KEYSPACE IF EXISTS idx_q", "DROP KEYSPACE IF EXISTS idx_s",
      "DROP KEYSPACE IF EXISTS idx_v", "DROP KEYSPACE IF EXISTS idx_m", "DROP KEYSPACE IF EXISTS idx_c")

# ---- runner -----------------------------------------------------------------------------------------
from cassandra import ConsistencyLevel as DCL

def err(e):
    msg = getattr(e, 'message', None)
    if not isinstance(msg, str): msg = str(e)
    m = re.search(r'code=([0-9a-fA-F]+) \[(.*?)\] message="(.*)"', msg, re.S)
    if m: return 'ERR 0x%s %s' % (m.group(1).upper().rjust(4, '0'), m.group(3))
    code = getattr(e, 'code', None)
    if isinstance(code, int): return 'ERR 0x%04X %s' % (code, msg)
    return 'CLIENT %s: %s' % (type(e).__name__, msg)

def rows_of(rs, mask, cols):
    rows = json.loads(json.dumps([list(r) for r in rs], default=repr))
    if mask:
        idx = [i for i, c in enumerate(cols) if c in mask]
        for r in rows:
            for i in idx: r[i] = '*'
    return rows

def canon_rows(cols, types, rows, warn, sort):
    if sort: rows = sorted(rows, key=repr)
    return 'ROWS cols=%r types=%r rows=%r%s' % (cols, types, rows, warn)

def canon(rs, o):
    cols = list(rs.column_names or [])
    w = rs.response_future.warnings
    warn = (' WARN %r' % (list(w),)) if w else ''
    if not cols and not rs.current_rows:
        return 'ROWS cols=[] types=[] rows=[]' + warn
    types = [t.cql_parameterized_type() for t in (rs.column_types or [])]
    return canon_rows(cols, types, rows_of(rs.current_rows, o.get('mask'), cols), warn, o.get('sort'))

def canon_pages(s, cql, o):
    st = SimpleStatement(cql, fetch_size=o['page'], consistency_level=getattr(DCL, o.get('cl', 'ONE')))
    rs = s.execute(st)
    pages = [canon(rs, o)]
    while rs.has_more_pages and len(pages) < 50:
        rs.fetch_next_page()
        pages.append(canon(rs, o))
    return 'PAGES ' + ' | '.join(pages)

def canon_raw(r):
    if not r['ok']:
        e = r['error']
        return 'ERR %s %s' % (e['code'], e['message'])
    w = r.get('warnings') or []
    warn = (' WARN %r' % (w,)) if w else ''
    k = r['result_kind']
    if k == 'SchemaChange':
        return 'OK SchemaChange ' + ' '.join(r['schema_change']) + warn
    if k == 'Rows':
        return 'RAW Rows n=%s%s' % (r.get('column_count'), warn)
    return 'OK %s%s' % (k, warn)

DDL = re.compile(r'^\s*(CREATE|ALTER|DROP|TRUNCATE)\b', re.I)

def run(host):
    prof = ExecutionProfile(request_timeout=120, row_factory=tuple_factory,
                            load_balancing_policy=WhiteListRoundRobinPolicy([host]))
    c = Cluster([host], execution_profiles={EXEC_PROFILE_DEFAULT: prof}, protocol_version=4)
    s = c.connect()
    raw = Raw(host)
    out = []
    for cid, cql, o in S:
        if cid == 'wait':
            import time
            for _ in range(600):
                if host != '127.0.0.1':
                    want = {r[0] for r in s.execute("SELECT view_name FROM system_schema.views WHERE keyspace_name = %s", (cql,))}
                    built = {r[0] for r in s.execute("SELECT view_name FROM system.built_views WHERE keyspace_name = %s", (cql,))}
                else:
                    # Cassandra marks built indexes in system."IndexInfo".
                    want = {r[0] for r in s.execute("SELECT index_name FROM system_schema.indexes WHERE keyspace_name = %s", (cql,))}
                    built = {r[0] for r in s.execute('SELECT index_name FROM system."IndexInfo" WHERE table_name = %s', (cql,))}
                if want <= built: break
                time.sleep(0.5)
            time.sleep(1)
            continue
        try:
            if DDL.match(cql):
                r = canon_raw(raw.query(cql, page_size=None))
            elif o.get('page'):
                r = canon_pages(s, cql, o)
            else:
                st = SimpleStatement(cql, consistency_level=getattr(DCL, o.get('cl', 'ONE')))
                r = canon(s.execute(st), o)
        except Exception as e:
            r = err(e)
        out.append({'id': cid, 'cql': cql, 'page': o.get('page'), 'mask': o.get('mask'),
                    'sort': bool(o.get('sort')), 'cl': o.get('cl', 'ONE'), 'result': r})
    c.shutdown()
    return out

sc = run(sys.argv[1])
ca = run('127.0.0.1')
S[:] = [x for x in S if x[0] != 'wait']
steps = []
for a, b_ in zip(sc, ca):
    x = dict(a)
    x['scylla'] = a['result']; x['cassandra'] = b_['result']
    del x['result']
    steps.append(x)
json.dump(steps, sys.stdout, indent=1)
