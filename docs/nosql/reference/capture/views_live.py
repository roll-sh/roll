# Cassandra materialized views and SASI, live: `python3 -I views_live.py <port> <out.json>` against
# nosql-ref-cassandra-auth (29043: materialized_views_enabled, sasi_indexes_enabled, superuser
# cassandra/cassandra). The default reference has both features off, so derived.py only shows
# their "disabled" errors; this capture shows what they do.
#
# A raw protocol v4 client (auth_live.py's connection): every result is recorded as the error
# (code, message) or the result kind, schema change, columns (name, protocol type), rows rendered
# as text (text as is, ints, booleans True/False, collections python-like, other types 0x-hex),
# warnings and, for paged steps, every page. Steps:
#   ('setup', cql)       run, not recorded
#   (id, cql, opts)      recorded; opts: page=<fetch size>, sort=True (order-insensitive rows)
#   ('wait', ks)         Cassandra builds views and SASI indexes in the background: wait until the
#                        keyspace's are built (roll builds them before CREATE answers)
#   ('sleep', secs)      let TTLs expire
# Keyspaces are prefixed udfv_mv_ and dropped at the end. crates/nosql-engine/scripts/gen_views.py
# turns the output into crates/nosql-engine/src/tests/captured_views.rs.
import os, struct, sys, time

S = []


def setup(*qs):
    for q in qs:
        S.append(('setup', q, {}))


def q(cid, cql, **opts):
    S.append((cid, cql, opts))


def cases(items, **opts):
    for cid, cql in items:
        q(cid, cql, **opts)


def wait(ks):
    S.append(('wait', ks, {}))


def sleep(secs):
    S.append(('sleep', str(secs), {}))


NTS = "{'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"
K = 'udfv_mv_a'
MV = 'CREATE MATERIALIZED VIEW %s.' % K

# ==== CREATE MATERIALIZED VIEW: forms and every error ===============================================
setup(
    'DROP KEYSPACE IF EXISTS udfv_mv_a',
    'DROP KEYSPACE IF EXISTS udfv_mv_other',
    'CREATE KEYSPACE udfv_mv_a WITH replication = ' + NTS,
    'CREATE KEYSPACE udfv_mv_other WITH replication = ' + NTS,
    'CREATE TYPE udfv_mv_a.ty (a int, b text)',
    'CREATE TABLE udfv_mv_a.b (pk int, ck int, v int, w text, x int, l list<int>, st set<int>, m map<int, text>, fm frozen<map<int, int>>, u ty, fu frozen<ty>, du duration, PRIMARY KEY (pk, ck))',
    'CREATE TABLE udfv_mv_a.st (pk int, ck int, s int static, v int, PRIMARY KEY (pk, ck))',
    'CREATE TABLE udfv_mv_a.one (pk int PRIMARY KEY, v int, w int)',
    'CREATE TABLE udfv_mv_a.cnt (pk int PRIMARY KEY, c counter)',
    'CREATE TABLE udfv_mv_a.k (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d))',
    'CREATE TABLE udfv_mv_a.rev (pk int, ck int, v int, PRIMARY KEY (pk, ck)) WITH CLUSTERING ORDER BY (ck DESC)',
    'CREATE TABLE udfv_mv_a.gc0 (pk int PRIMARY KEY, v int) WITH gc_grace_seconds = 0',
    'CREATE TABLE udfv_mv_a.dttl (pk int PRIMARY KEY, v int) WITH default_time_to_live = 1000',
    'CREATE TABLE udfv_mv_a."Quoted" ("Pk" int, "Ck" int, "Val" text, PRIMARY KEY ("Pk", "Ck"))',
    'CREATE TABLE udfv_mv_other.t (pk int PRIMARY KEY, v int)',
    'CREATE TABLE udfv_mv_a.rn (pk int, ck int, v int, w int, PRIMARY KEY (pk, ck))',
    'INSERT INTO udfv_mv_a.rn (pk, ck, v, w) VALUES (1, 2, 3, 4)',
    "INSERT INTO udfv_mv_a.b (pk, ck, v, w, x, l, fm) VALUES (1, 1, 10, 'a', 1, [1], {1: 1}) USING TIMESTAMP 1000",
    "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (1, 2, 20, 'b') USING TIMESTAMP 1000",
    "INSERT INTO udfv_mv_a.b (pk, ck, w) VALUES (2, 1, 'c') USING TIMESTAMP 1000",
    'UPDATE udfv_mv_a.b USING TIMESTAMP 1000 SET v = 30 WHERE pk = 3 AND ck = 1',
    "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (4, 1, 10, 'd') USING TIMESTAMP 1000",
    'DELETE FROM udfv_mv_a.b USING TIMESTAMP 1001 WHERE pk = 4 AND ck = 1',
    'INSERT INTO udfv_mv_a.rev (pk, ck, v) VALUES (1, 1, 1)',
    'INSERT INTO udfv_mv_a.rev (pk, ck, v) VALUES (1, 2, 2)',
    'INSERT INTO udfv_mv_a.rev (pk, ck, v) VALUES (1, 3, 3)',
    'INSERT INTO udfv_mv_a.dttl (pk, v) VALUES (1, 1)',
    'INSERT INTO udfv_mv_a."Quoted" ("Pk", "Ck", "Val") VALUES (1, 2, \'q\')',
)
cases([
    # Forms.
    ('mv_by_v', MV + 'by_v AS SELECT * FROM udfv_mv_a.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_by_ck', MV + "by_ck AS SELECT pk, ck, w FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (pk DESC) AND comment = 'by ck'"),
    ('mv_filter_pk', MV + 'pk1 AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk = 1 AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_filter_ck_range', MV + 'ck_gt AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck > 1 PRIMARY KEY (ck, pk)'),
    ('mv_filter_ck_in', MV + 'ck_in AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IN (1, 2) PRIMARY KEY (ck, pk)'),
    ('mv_filter_pk_in', MV + 'pk_in AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IN (1, 3) AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_filter_multi', MV + 'multi AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND (ck) >= (2) PRIMARY KEY (ck, pk)'),
    ('mv_filter_view_key', MV + 'v10 AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE v = 10 AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_filter_view_key_range', MV + 'vgt AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE v > 15 AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_composite_pk', MV + 'comp AS SELECT * FROM udfv_mv_a.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY ((v, pk), ck)'),
    ('mv_frozen_in_pk', MV + 'by_fm AS SELECT pk, ck, fm FROM udfv_mv_a.b WHERE fm IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (fm, pk, ck)'),
    ('mv_frozen_udt_in_pk', MV + 'by_fu AS SELECT pk, ck FROM udfv_mv_a.b WHERE fu IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (fu, pk, ck)'),
    ('mv_same_pk', MV + 'same AS SELECT * FROM udfv_mv_a.one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_same_pk_subset', MV + 'same_v AS SELECT pk, v FROM udfv_mv_a.one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_keys_only', MV + 'keys_only AS SELECT pk FROM udfv_mv_a.one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_k', MV + 'k_by_d AS SELECT * FROM udfv_mv_a.k WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL PRIMARY KEY (d, a, b, c)'),
    ('mv_k_regular', MV + 'k_by_v AS SELECT * FROM udfv_mv_a.k WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL AND v IS NOT NULL PRIMARY KEY ((v, a), b, c, d)'),
    ('mv_rev', MV + 'rev_v AS SELECT * FROM udfv_mv_a.rev WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (pk, ck, v)'),
    ('mv_rev_order', MV + 'rev_asc AS SELECT * FROM udfv_mv_a.rev WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (pk, ck) WITH CLUSTERING ORDER BY (ck ASC)'),
    ('mv_static_base_no_static', MV + 'st_v AS SELECT pk, ck, v FROM udfv_mv_a.st WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_dttl_base', MV + 'dttl_v AS SELECT * FROM udfv_mv_a.dttl WHERE pk IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk)'),
    ('mv_quoted', MV + '"QView" AS SELECT "Pk", "Ck", "Val" FROM udfv_mv_a."Quoted" WHERE "Pk" IS NOT NULL AND "Ck" IS NOT NULL AND "Val" IS NOT NULL PRIMARY KEY ("Val", "Pk", "Ck")'),
    ('mv_rn', MV + 'rn_v AS SELECT pk, ck, v FROM udfv_mv_a.rn WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL AND pk > 0 PRIMARY KEY (v, ck, pk)'),
    ('mv_options', MV + "opts AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH gc_grace_seconds = 5 AND compaction = {'class': 'LeveledCompactionStrategy'} AND caching = {'keys': 'NONE'} AND crc_check_chance = 0.5"),
    ('mv_options_id', MV + 'with_id AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH id = 5a1c395e-b41f-11e5-9f22-ba0be0483c18'),
    ('mv_token_filter', MV + 'tok AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND token(pk) > 0 PRIMARY KEY (ck, pk)'),
    ('mv_exists', MV + 'by_v AS SELECT * FROM udfv_mv_a.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_exists_if_not_exists', 'CREATE MATERIALIZED VIEW IF NOT EXISTS udfv_mv_a.by_v AS SELECT * FROM udfv_mv_a.b WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_exists_if_not_exists_other_def', 'CREATE MATERIALIZED VIEW IF NOT EXISTS udfv_mv_a.by_v AS SELECT pk, ck FROM udfv_mv_a.one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_named_like_table', MV + 'one AS SELECT * FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_named_like_table_if_not_exists', 'CREATE MATERIALIZED VIEW IF NOT EXISTS udfv_mv_a.one AS SELECT * FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    # Errors.
    ('mv_unknown_base', MV + 'x AS SELECT * FROM udfv_mv_a.nope WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_unknown_keyspace', 'CREATE MATERIALIZED VIEW udfv_mv_nope.x AS SELECT * FROM udfv_mv_nope.b WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_other_keyspace', 'CREATE MATERIALIZED VIEW udfv_mv_other.x AS SELECT * FROM udfv_mv_a.one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_no_keyspace', 'CREATE MATERIALIZED VIEW x AS SELECT * FROM one WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_system_keyspace', 'CREATE MATERIALIZED VIEW system.x AS SELECT * FROM system.local WHERE key IS NOT NULL PRIMARY KEY (key)'),
    ('mv_counter', MV + 'x AS SELECT * FROM udfv_mv_a.cnt WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_on_view', MV + 'x AS SELECT * FROM udfv_mv_a.by_v WHERE v IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (pk, v, ck)'),
    ('mv_gc_zero', MV + 'x AS SELECT * FROM udfv_mv_a.gc0 WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_static_star', MV + 'x AS SELECT * FROM udfv_mv_a.st WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_static_explicit', MV + 'x AS SELECT pk, ck, s FROM udfv_mv_a.st WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_static_in_pk', MV + 'x AS SELECT pk, ck, s FROM udfv_mv_a.st WHERE pk IS NOT NULL AND ck IS NOT NULL AND s IS NOT NULL PRIMARY KEY (s, pk, ck)'),
    ('mv_unknown_column', MV + 'x AS SELECT pk, ck, nope FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_alias', MV + 'x AS SELECT pk AS p, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_function', MV + 'x AS SELECT pk, ck, ttl(v) FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_parenthesized', MV + 'x AS SELECT (pk), ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_literal', MV + "x AS SELECT pk, ck, 'a' FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
    ('mv_field', MV + 'x AS SELECT pk, ck, fu.a FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_count', MV + 'x AS SELECT count(*) FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_use_unqualified', 'USE udfv_mv_a'),
    ('mv_unqualified', 'CREATE MATERIALIZED VIEW uq AS SELECT pk, ck FROM rev WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_unqualified_base', 'CREATE MATERIALIZED VIEW udfv_mv_a.uq2 AS SELECT pk, ck FROM rev WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_use_other', 'USE udfv_mv_other'),
    ('mv_unqualified_view_other_ks', 'CREATE MATERIALIZED VIEW uq3 AS SELECT pk, ck FROM udfv_mv_a.rev WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_qualified_view_unqualified_base', 'CREATE MATERIALIZED VIEW udfv_mv_a.uq4 AS SELECT pk, ck FROM rev WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_cast', MV + 'x AS SELECT pk, ck, CAST(v AS text) FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_pk_not_selected', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_pk_unknown', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, nope)'),
    ('mv_pk_duplicate', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk, ck)'),
    ('mv_pk_duplicate_partition', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY ((ck, ck), pk)'),
    ('mv_missing_base_pk', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk)'),
    ('mv_missing_base_pk_two', MV + 'x AS SELECT * FROM udfv_mv_a.k WHERE a IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, a)'),
    ('mv_two_regular', MV + 'x AS SELECT * FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL AND x IS NOT NULL PRIMARY KEY (v, x, pk, ck)'),
    ('mv_three_regular', MV + 'x AS SELECT * FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL AND x IS NOT NULL AND w IS NOT NULL PRIMARY KEY ((v, x), w, pk, ck)'),
    ('mv_collection_pk', MV + 'x AS SELECT pk, ck, l FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND l IS NOT NULL PRIMARY KEY (l, pk, ck)'),
    ('mv_set_pk', MV + 'x AS SELECT pk, ck, st FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND st IS NOT NULL PRIMARY KEY (pk, ck, st)'),
    ('mv_udt_pk', MV + 'x AS SELECT pk, ck, u FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND u IS NOT NULL PRIMARY KEY (u, pk, ck)'),
    ('mv_duration_pk', MV + 'x AS SELECT pk, ck, du FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND du IS NOT NULL PRIMARY KEY (du, pk, ck)'),
    ('mv_missing_not_null', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (v, pk, ck)'),
    ('mv_missing_not_null_pk', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_missing_not_null_all', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b PRIMARY KEY (v, pk, ck)'),
    ('mv_missing_not_null_two', MV + 'x AS SELECT * FROM udfv_mv_a.k WHERE a IS NOT NULL AND b IS NOT NULL PRIMARY KEY (d, a, b, c)'),
    ('mv_filter_regular', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v = 1 PRIMARY KEY (ck, pk)'),
    ('mv_filter_two_regular', MV + "x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v = 1 AND w = 'a' PRIMARY KEY (ck, pk)"),
    ('mv_filter_regular_range', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND x > 1 PRIMARY KEY (ck, pk)'),
    ('mv_filter_contains', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND l CONTAINS 1 PRIMARY KEY (ck, pk)'),
    ('mv_filter_neq', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND ck != 1 PRIMARY KEY (ck, pk)'),
    ('mv_filter_like', MV + "x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND w LIKE 'a%' PRIMARY KEY (ck, pk)"),
    ('mv_regular_not_null_not_pk', MV + 'nnr AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_not_null_unselected', MV + 'nnu AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND x IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_not_null_unknown', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND nope IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_not_null_twice', MV + 'nn2 AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_bind_marker', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck = ? PRIMARY KEY (ck, pk)'),
    ('mv_clustering_order_unknown', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (nope DESC)'),
    ('mv_clustering_order_wrong', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck) WITH CLUSTERING ORDER BY (ck DESC)'),
    ('mv_clustering_order_partial', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck) WITH CLUSTERING ORDER BY (pk DESC)'),
    ('mv_clustering_order_swapped', MV + 'x AS SELECT pk, ck, v FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND v IS NOT NULL PRIMARY KEY (v, pk, ck) WITH CLUSTERING ORDER BY (ck DESC, pk ASC)'),
    ('mv_clustering_order_partition', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (ck DESC)'),
    ('mv_clustering_order_dup', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH CLUSTERING ORDER BY (pk DESC, pk DESC)'),
    ('mv_ttl', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH default_time_to_live = 5'),
    ('mv_ttl_zero', MV + 'ttl0 AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH default_time_to_live = 0'),
    ('mv_unknown_option', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH nope = 1'),
    ('mv_gc_zero_option', MV + 'gcz AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH gc_grace_seconds = 0'),
    ('mv_compact_storage', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH COMPACT STORAGE'),
    ('mv_custom_expression', MV + "x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND expr(i, 'x') PRIMARY KEY (ck, pk)"),
    ('mv_no_partition_key', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY ((), pk, ck)'),
    ('mv_name_invalid', 'CREATE MATERIALIZED VIEW udfv_mv_a."bad name" AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_name_long', MV + 'v' * 230 + ' AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_select_json', MV + 'x AS SELECT JSON pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
    ('mv_select_distinct', MV + 'x AS SELECT DISTINCT pk FROM udfv_mv_a.b WHERE pk IS NOT NULL PRIMARY KEY (pk)'),
    ('mv_limit', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) LIMIT 1'),
    ('mv_allow_filtering', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL ALLOW FILTERING PRIMARY KEY (ck, pk)'),
    ('mv_where_unknown_column', MV + 'x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND nope = 1 PRIMARY KEY (ck, pk)'),
    ('mv_where_bad_type', MV + "x AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk = 'a' AND ck IS NOT NULL PRIMARY KEY (ck, pk)"),
])
wait(K)
cases([
    ('mv_rows_views', "SELECT keyspace_name, view_name, base_table_name, include_all_columns, where_clause, comment, gc_grace_seconds, default_time_to_live, crc_check_chance, caching, compaction FROM system_schema.views WHERE keyspace_name = 'udfv_mv_a'"),
    ('mv_rows_ids', "SELECT view_name, id FROM system_schema.views WHERE keyspace_name = 'udfv_mv_a' AND view_name = 'with_id'"),
    ('mv_rows_columns', "SELECT table_name, column_name, clustering_order, kind, position, type FROM system_schema.columns WHERE keyspace_name = 'udfv_mv_a' AND table_name IN ('by_v', 'by_ck', 'comp', 'same_v', 'keys_only', 'k_by_d', 'k_by_v', 'rev_v', 'rev_asc', 'QView', 'by_fu')"),
    ('mv_rows_tables', "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'udfv_mv_a'"),
    ('mv_rows_built', "SELECT * FROM system.built_views WHERE keyspace_name = 'udfv_mv_a'"),
    ('mv_rows_build_status', "SELECT keyspace_name, view_name, status FROM system_distributed.view_build_status WHERE keyspace_name = 'udfv_mv_a' AND view_name = 'by_v'"),
    ('mv_rows_in_progress', "SELECT * FROM system.view_builds_in_progress WHERE keyspace_name = 'udfv_mv_a'"),
])
# Backfill of the pre-existing rows (deleted and key-less rows excluded), and reads.
cases([
    ('mvr_by_v', 'SELECT * FROM udfv_mv_a.by_v'),
    ('mvr_by_v_where', 'SELECT pk, ck, w FROM udfv_mv_a.by_v WHERE v = 10'),
    ('mvr_by_v_writetime', 'SELECT v, pk, ck, writetime(w) FROM udfv_mv_a.by_v'),
    ('mvr_by_ck', 'SELECT * FROM udfv_mv_a.by_ck'),
    ('mvr_pk1', 'SELECT * FROM udfv_mv_a.pk1'),
    ('mvr_ck_gt', 'SELECT * FROM udfv_mv_a.ck_gt'),
    ('mvr_ck_in', 'SELECT * FROM udfv_mv_a.ck_in'),
    ('mvr_pk_in', 'SELECT * FROM udfv_mv_a.pk_in'),
    ('mvr_multi', 'SELECT * FROM udfv_mv_a.multi'),
    ('mvr_v10', 'SELECT * FROM udfv_mv_a.v10'),
    ('mvr_vgt', 'SELECT * FROM udfv_mv_a.vgt'),
    ('mvr_comp', 'SELECT v, pk, ck, w FROM udfv_mv_a.comp WHERE v = 10 AND pk = 1'),
    ('mvr_by_fm', 'SELECT * FROM udfv_mv_a.by_fm'),
    ('mvr_tok', 'SELECT * FROM udfv_mv_a.tok'),
    ('mvr_rev_v', 'SELECT * FROM udfv_mv_a.rev_v'),
    ('mvr_rev_asc', 'SELECT * FROM udfv_mv_a.rev_asc'),
    ('mvr_dttl', 'SELECT pk, v FROM udfv_mv_a.dttl_v'),
    ('mvr_quoted', 'SELECT * FROM udfv_mv_a."QView"'),
    ('mvr_nn2', 'SELECT * FROM udfv_mv_a.nn2'),
    ('mvr_filtering', "SELECT * FROM udfv_mv_a.by_v WHERE w = 'a' ALLOW FILTERING"),
    ('mvr_filtering_needed', "SELECT * FROM udfv_mv_a.by_v WHERE w = 'a'"),
    ('mvr_order_by', 'SELECT pk, ck FROM udfv_mv_a.by_v WHERE v = 10 ORDER BY pk DESC'),
    ('mvr_limit', 'SELECT v, pk, ck FROM udfv_mv_a.by_v LIMIT 2'),
    ('mvr_per_partition_limit', 'SELECT v, pk, ck FROM udfv_mv_a.by_v PER PARTITION LIMIT 1'),
    ('mvr_count', 'SELECT count(*) FROM udfv_mv_a.by_v'),
    ('mvr_group_by', 'SELECT v, count(*) FROM udfv_mv_a.by_v GROUP BY v'),
    ('mvr_token', 'SELECT v, pk, ck FROM udfv_mv_a.by_v WHERE token(v) > 0'),
    ('mvr_json', 'SELECT JSON v, pk, ck FROM udfv_mv_a.by_v WHERE v = 10'),
    ('mvr_unknown_column', 'SELECT x FROM udfv_mv_a.by_ck'),
    ('mvr_distinct', 'SELECT DISTINCT v FROM udfv_mv_a.by_v'),
    ('mvr_reversed', 'SELECT pk, ck FROM udfv_mv_a.by_ck WHERE ck = 1 ORDER BY pk ASC'),
    ('mvr_rev_slice', 'SELECT * FROM udfv_mv_a.rev_v WHERE pk = 1 AND ck < 3'),
    ('mvr_uq', 'SELECT * FROM udfv_mv_a.uq2'),
    ('mvr_in_view_key', 'SELECT pk, ck FROM udfv_mv_a.by_v WHERE v IN (10, 20)'),
    ('mvr_ttl_writetime', 'SELECT v, pk, ck, ttl(w), writetime(v) FROM udfv_mv_a.by_v WHERE v = 10'),
])
q('mvr_by_v_paged', 'SELECT v, pk, ck FROM udfv_mv_a.by_v', page=1)
q('mvr_by_ck_paged', 'SELECT * FROM udfv_mv_a.by_ck', page=2)
# Statements on views.
cases([
    ('mvs_insert', 'INSERT INTO udfv_mv_a.by_v (v, pk, ck) VALUES (1, 1, 1)'),
    ('mvs_update', "UPDATE udfv_mv_a.by_v SET w = 'x' WHERE v = 1 AND pk = 1 AND ck = 1"),
    ('mvs_delete', 'DELETE FROM udfv_mv_a.by_v WHERE v = 1'),
    ('mvs_batch', 'BEGIN BATCH INSERT INTO udfv_mv_a.by_v (v, pk, ck) VALUES (1, 1, 1); APPLY BATCH'),
    ('mvs_lwt', 'INSERT INTO udfv_mv_a.by_v (v, pk, ck) VALUES (1, 1, 1) IF NOT EXISTS'),
    ('mvs_truncate', 'TRUNCATE udfv_mv_a.by_v'),
    ('mvs_alter_table', "ALTER TABLE udfv_mv_a.by_v WITH comment = 'x'"),
    ('mvs_alter_table_add', 'ALTER TABLE udfv_mv_a.by_v ADD z int'),
    ('mvs_drop_table', 'DROP TABLE udfv_mv_a.by_v'),
    ('mvs_index', 'CREATE INDEX ON udfv_mv_a.by_v (w)'),
    ('mvs_sasi', "CREATE CUSTOM INDEX ON udfv_mv_a.by_v (w) USING 'org.apache.cassandra.index.sasi.SASIIndex'"),
    ('mvs_trigger', "CREATE TRIGGER trig ON udfv_mv_a.by_v USING 'org.example.Nope'"),
    ('mvs_select_serial', 'SELECT * FROM udfv_mv_a.by_v WHERE v = 10 AND pk = 1 AND ck = 1'),
])
# ALTER / DROP of base tables with views.
cases([
    ('mvb_drop_view_key_column', 'ALTER TABLE udfv_mv_a.b DROP v'),
    ('mvb_drop_selected_column', 'ALTER TABLE udfv_mv_a.b DROP w'),
    ('mvb_drop_unselected_column', 'ALTER TABLE udfv_mv_a.one DROP w'),
    ('mvb_drop_star_column', 'ALTER TABLE udfv_mv_a.k DROP v'),
    ('mvb_rename_pk', 'ALTER TABLE udfv_mv_a.rn RENAME pk TO pk2'),
    ('mvb_rename_ck', 'ALTER TABLE udfv_mv_a.rn RENAME ck TO ck2'),
    ('mvb_rename_regular', 'ALTER TABLE udfv_mv_a.rn RENAME v TO v2'),
    ('mvb_add_column', 'ALTER TABLE udfv_mv_a.b ADD y int'),
    ('mvb_add_static', 'ALTER TABLE udfv_mv_a.b ADD ys int static'),
    ('mvb_alter_options', "ALTER TABLE udfv_mv_a.b WITH comment = 'base'"),
    ('mvb_alter_gc_zero', 'ALTER TABLE udfv_mv_a.b WITH gc_grace_seconds = 0'),
    ('mvb_alter_default_ttl', 'ALTER TABLE udfv_mv_a.one WITH default_time_to_live = 100'),
    ('mvb_drop_base', 'DROP TABLE udfv_mv_a.b'),
    ('mvb_drop_base_if_exists', 'DROP TABLE IF EXISTS udfv_mv_a.b'),
])
cases([
    ('mvb_by_v_after_add', 'SELECT * FROM udfv_mv_a.by_v'),
    ('mvb_columns_after_add', "SELECT table_name, column_name, kind, type FROM system_schema.columns WHERE keyspace_name = 'udfv_mv_a' AND table_name IN ('by_v', 'by_ck', 'same', 'same_v', 'k_by_d')"),
    ('mvb_rn_after_rename', 'SELECT * FROM udfv_mv_a.rn_v'),
    ('mvb_rn_columns', "SELECT table_name, column_name, kind FROM system_schema.columns WHERE keyspace_name = 'udfv_mv_a' AND table_name IN ('rn', 'rn_v')"),
    ('mvb_rn_views', "SELECT view_name, where_clause FROM system_schema.views WHERE keyspace_name = 'udfv_mv_a' AND view_name = 'rn_v'"),
    ('mvb_same_after_drop', 'SELECT * FROM udfv_mv_a.same'),
    ('mvb_views_after', "SELECT view_name, gc_grace_seconds, default_time_to_live FROM system_schema.views WHERE keyspace_name = 'udfv_mv_a' AND view_name IN ('by_v', 'same')"),
])

# ==== View maintenance ===============================================================================
for cid, w in [
    ('mvw_update_key_column', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2000 SET v = 11 WHERE pk = 1 AND ck = 1'),
    ('mvw_update_regular', "UPDATE udfv_mv_a.b USING TIMESTAMP 2000 SET w = 'bb' WHERE pk = 1 AND ck = 2"),
    ('mvw_set_key_null', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2000 SET v = null WHERE pk = 3 AND ck = 1'),
    ('mvw_set_key_on_unset', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2000 SET v = 40 WHERE pk = 2 AND ck = 1'),
    ('mvw_older_timestamp', 'UPDATE udfv_mv_a.b USING TIMESTAMP 1500 SET v = 99 WHERE pk = 1 AND ck = 1'),
    ('mvw_same_value', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2001 SET v = 11 WHERE pk = 1 AND ck = 1'),
    ('mvw_insert_new', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (5, 1, 10, 'e') USING TIMESTAMP 2000"),
    ('mvw_delete_row', 'DELETE FROM udfv_mv_a.b USING TIMESTAMP 2001 WHERE pk = 5 AND ck = 1'),
    ('mvw_reinsert_older', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (5, 1, 10, 'e') USING TIMESTAMP 2000"),
    ('mvw_reinsert_newer', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (5, 1, 12, 'f') USING TIMESTAMP 2002"),
    ('mvw_delete_cell', 'DELETE w FROM udfv_mv_a.b USING TIMESTAMP 2003 WHERE pk = 5 AND ck = 1'),
    ('mvw_delete_key_cell', 'DELETE v FROM udfv_mv_a.b USING TIMESTAMP 2003 WHERE pk = 1 AND ck = 2'),
    ('mvw_list_append', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2003 SET l = l + [5] WHERE pk = 1 AND ck = 1'),
    ('mvw_set_add', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2003 SET st = st + {7, 8} WHERE pk = 1 AND ck = 1'),
    ('mvw_map_put', "UPDATE udfv_mv_a.b USING TIMESTAMP 2003 SET m[1] = 'one' WHERE pk = 1 AND ck = 1"),
    ('mvw_map_remove', 'DELETE m[1] FROM udfv_mv_a.b USING TIMESTAMP 2004 WHERE pk = 1 AND ck = 1'),
    ('mvw_frozen_key', 'UPDATE udfv_mv_a.b USING TIMESTAMP 2004 SET fm = {2: 2} WHERE pk = 1 AND ck = 1'),
    ('mvw_insert_json', 'INSERT INTO udfv_mv_a.b JSON \'{"pk": 6, "ck": 1, "v": 60, "w": "json"}\' USING TIMESTAMP 2005'),
    ('mvw_range_delete', 'DELETE FROM udfv_mv_a.b USING TIMESTAMP 3000 WHERE pk = 6 AND ck < 2'),
    ('mvw_partition_delete', 'DELETE FROM udfv_mv_a.b USING TIMESTAMP 3000 WHERE pk = 2'),
    ('mvw_batch_logged', 'BEGIN BATCH USING TIMESTAMP 4000 INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (7, 1, 70); UPDATE udfv_mv_a.b SET v = 71 WHERE pk = 7 AND ck = 1; INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (8, 1, 80); APPLY BATCH'),
    ('mvw_batch_unlogged', 'BEGIN UNLOGGED BATCH USING TIMESTAMP 4001 INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (9, 1, 90); DELETE FROM udfv_mv_a.b WHERE pk = 8 AND ck = 1; APPLY BATCH'),
    ('mvw_batch_move_twice', 'BEGIN BATCH UPDATE udfv_mv_a.b USING TIMESTAMP 4002 SET v = 72 WHERE pk = 7 AND ck = 1; UPDATE udfv_mv_a.b USING TIMESTAMP 4003 SET v = 73 WHERE pk = 7 AND ck = 1; APPLY BATCH'),
    ('mvw_lwt_insert', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (10, 1, 100, 'lwt') IF NOT EXISTS"),
    ('mvw_lwt_insert_again', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (10, 1, 101, 'lwt2') IF NOT EXISTS"),
    ('mvw_lwt_update', 'UPDATE udfv_mv_a.b SET v = 102 WHERE pk = 10 AND ck = 1 IF v = 100'),
    ('mvw_lwt_update_fails', 'UPDATE udfv_mv_a.b SET v = 103 WHERE pk = 10 AND ck = 1 IF v = 100'),
    ('mvw_lwt_delete', 'DELETE FROM udfv_mv_a.b WHERE pk = 10 AND ck = 1 IF EXISTS'),
    ('mvw_lwt_batch', 'BEGIN BATCH INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (11, 1, 110) IF NOT EXISTS; INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (11, 2, 111); APPLY BATCH'),
    ('mvw_same_pk_insert', 'INSERT INTO udfv_mv_a.one (pk, v) VALUES (1, 1) USING TIMESTAMP 1000'),
    ('mvw_same_pk_update_only_key', 'UPDATE udfv_mv_a.one USING TIMESTAMP 1000 SET v = 2 WHERE pk = 2'),
    ('mvw_same_pk_delete_v', 'DELETE v FROM udfv_mv_a.one USING TIMESTAMP 1001 WHERE pk = 2'),
    ('mvw_same_pk_insert_keys_only', 'INSERT INTO udfv_mv_a.one (pk) VALUES (3) USING TIMESTAMP 1000'),
    ('mvw_static_write', 'INSERT INTO udfv_mv_a.st (pk, s) VALUES (1, 5)'),
    ('mvw_static_row', 'INSERT INTO udfv_mv_a.st (pk, ck, s, v) VALUES (1, 1, 6, 1)'),
    ('mvw_static_update', 'UPDATE udfv_mv_a.st SET s = 7 WHERE pk = 1'),
    ('mvw_static_row_only_static', 'INSERT INTO udfv_mv_a.st (pk, ck, s) VALUES (2, 1, 8)'),
    ('mvw_static_partition_delete', 'DELETE FROM udfv_mv_a.st WHERE pk = 2'),
    ('mvw_k_insert', 'INSERT INTO udfv_mv_a.k (a, b, c, d, v) VALUES (1, 1, 1, 1, 1)'),
    ('mvw_k_insert_2', 'INSERT INTO udfv_mv_a.k (a, b, c, d, v) VALUES (1, 1, 1, 2, 2)'),
    ('mvw_k_range_delete', 'DELETE FROM udfv_mv_a.k WHERE a = 1 AND b = 1 AND c = 1 AND d > 1'),
    ('mvw_ttl', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (12, 1, 120, 'ttl') USING TTL 100000"),
    ('mvw_ttl_short_row', "INSERT INTO udfv_mv_a.b (pk, ck, v, w) VALUES (13, 1, 130, 'short') USING TTL 2"),
    ('mvw_ttl_short_key', 'UPDATE udfv_mv_a.b USING TTL 2 SET v = 140 WHERE pk = 14 AND ck = 1'),
    ('mvw_ttl_short_regular', "INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (15, 1, 150)"),
    ('mvw_ttl_short_regular_2', "UPDATE udfv_mv_a.b USING TTL 2 SET w = 'gone' WHERE pk = 15 AND ck = 1"),
    ('mvw_ttl_dttl', 'INSERT INTO udfv_mv_a.dttl (pk, v) VALUES (2, 2)'),
]:
    q(cid, w)
cases([
    ('mva_by_v', 'SELECT v, pk, ck, w, x FROM udfv_mv_a.by_v'),
    ('mva_by_v_writetime', 'SELECT v, pk, ck, writetime(w) FROM udfv_mv_a.by_v WHERE v = 11'),
    ('mva_by_v_collections', 'SELECT pk, ck, l, st, m, fm FROM udfv_mv_a.by_v WHERE v = 11'),
    ('mva_by_ck', 'SELECT * FROM udfv_mv_a.by_ck'),
    ('mva_pk1', 'SELECT * FROM udfv_mv_a.pk1'),
    ('mva_ck_gt', 'SELECT * FROM udfv_mv_a.ck_gt'),
    ('mva_v10', 'SELECT * FROM udfv_mv_a.v10'),
    ('mva_vgt', 'SELECT * FROM udfv_mv_a.vgt'),
    ('mva_comp', 'SELECT v, pk, ck, w FROM udfv_mv_a.comp'),
    ('mva_by_fm', 'SELECT * FROM udfv_mv_a.by_fm'),
    ('mva_same', 'SELECT * FROM udfv_mv_a.same'),
    ('mva_same_v', 'SELECT * FROM udfv_mv_a.same_v'),
    ('mva_keys_only', 'SELECT * FROM udfv_mv_a.keys_only'),
    ('mva_st_v', 'SELECT * FROM udfv_mv_a.st_v'),
    ('mva_k_by_d', 'SELECT * FROM udfv_mv_a.k_by_d'),
    ('mva_k_by_v', 'SELECT * FROM udfv_mv_a.k_by_v'),
    ('mva_ttl_present', 'SELECT pk, ck, w FROM udfv_mv_a.by_v WHERE v = 120'),
    ('mva_short_present', 'SELECT v, pk, ck, w FROM udfv_mv_a.by_v WHERE v IN (130, 140, 150)'),
    ('mva_dttl', 'SELECT pk, v FROM udfv_mv_a.dttl_v'),
])
sleep(3)
cases([
    ('mva_short_expired', 'SELECT v, pk, ck, w FROM udfv_mv_a.by_v WHERE v IN (130, 140, 150)'),
    ('mva_short_base', 'SELECT pk, ck, v, w FROM udfv_mv_a.b WHERE pk IN (13, 14, 15)'),
])
q('mva_by_v_paged', 'SELECT v, pk, ck FROM udfv_mv_a.by_v', page=3)
# TRUNCATE of the base empties its views.
cases([
    ('mvt_truncate_base', 'TRUNCATE udfv_mv_a.k'),
    ('mvt_k_by_d', 'SELECT * FROM udfv_mv_a.k_by_d'),
    ('mvt_k_insert', 'INSERT INTO udfv_mv_a.k (a, b, c, d, v) VALUES (2, 2, 2, 2, 2)'),
    ('mvt_k_by_d_after', 'SELECT * FROM udfv_mv_a.k_by_d'),
])
# ALTER / DROP MATERIALIZED VIEW.
cases([
    ('amv_comment', "ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH comment = 'altered'"),
    ('amv_several', "ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH gc_grace_seconds = 100 AND compaction = {'class': 'LeveledCompactionStrategy'}"),
    ('amv_ttl', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH default_time_to_live = 10'),
    ('amv_ttl_zero', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH default_time_to_live = 0'),
    ('amv_unknown_option', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH nope = 1'),
    ('amv_bad_value', "ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH gc_grace_seconds = 'x'"),
    ('amv_on_table', "ALTER MATERIALIZED VIEW udfv_mv_a.one WITH comment = 'x'"),
    ('amv_unknown', "ALTER MATERIALIZED VIEW udfv_mv_a.nope WITH comment = 'x'"),
    ('amv_unknown_keyspace', "ALTER MATERIALIZED VIEW udfv_mv_nope.nope WITH comment = 'x'"),
    ('amv_if_exists', "ALTER MATERIALIZED VIEW IF EXISTS udfv_mv_a.nope WITH comment = 'x'"),
    ('amv_gc_zero', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH gc_grace_seconds = 0'),
    ('amv_id', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH id = 5a1c395e-b41f-11e5-9f22-ba0be0483c18'),
    ('amv_cdc', 'ALTER MATERIALIZED VIEW udfv_mv_a.by_v WITH cdc = true'),
    ('amv_rows', "SELECT view_name, comment, gc_grace_seconds, compaction FROM system_schema.views WHERE keyspace_name = 'udfv_mv_a' AND view_name = 'by_v'"),
    ('dmv_on_table', 'DROP MATERIALIZED VIEW udfv_mv_a.one'),
    ('dmv_unknown', 'DROP MATERIALIZED VIEW udfv_mv_a.nope'),
    ('dmv_unknown_keyspace', 'DROP MATERIALIZED VIEW udfv_mv_nope.nope'),
    ('dmv_unknown_if_exists', 'DROP MATERIALIZED VIEW IF EXISTS udfv_mv_a.nope'),
    ('dmv_existing', 'DROP MATERIALIZED VIEW udfv_mv_a.pk_in'),
    ('dmv_select_after', 'SELECT * FROM udfv_mv_a.pk_in'),
    ('dmv_base_write_after', 'INSERT INTO udfv_mv_a.b (pk, ck, v) VALUES (20, 1, 200)'),
    ('dmv_base_still_has_views', 'DROP TABLE udfv_mv_a.one'),
    ('dmv_rest_one', 'DROP MATERIALIZED VIEW udfv_mv_a.same'),
    ('dmv_rest_two', 'DROP MATERIALIZED VIEW udfv_mv_a.same_v'),
    ('dmv_rest_three', 'DROP MATERIALIZED VIEW udfv_mv_a.keys_only'),
    ('dmv_base_free', 'DROP TABLE udfv_mv_a.one'),
    ('dmv_built_after', "SELECT view_name FROM system.built_views WHERE keyspace_name = 'udfv_mv_a' AND view_name IN ('pk_in', 'same', 'by_v')"),
    ('dmv_recreate', MV + 'pk_in AS SELECT pk, ck FROM udfv_mv_a.b WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)'),
])
wait(K)
q('dmv_recreated_rows', 'SELECT * FROM udfv_mv_a.pk_in WHERE ck = 1', sort=True)
q('dmv_drop_keyspace', 'DROP KEYSPACE udfv_mv_other')

# ==== SASI ===========================================================================================
SK = 'udfv_mv_s'
SASI = "USING 'org.apache.cassandra.index.sasi.SASIIndex'"
setup(
    'DROP KEYSPACE IF EXISTS udfv_mv_s',
    'CREATE KEYSPACE udfv_mv_s WITH replication = ' + NTS,
    'CREATE TABLE udfv_mv_s.t (pk int, ck text, v int, txt text, asci ascii, ci text, lo text, up text, std text, dl text, big bigint, d double, ts timestamp, u uuid, b boolean, bl blob, l list<text>, fl frozen<list<int>>, s int static, PRIMARY KEY (pk, ck))',
    'CREATE TABLE udfv_mv_s.e (pk int PRIMARY KEY, v int, txt text)',
    'CREATE TABLE udfv_mv_s.plain (pk int PRIMARY KEY, txt text)',
    'CREATE TABLE udfv_mv_s.dl (pk int PRIMARY KEY, dl text)',
    'CREATE TABLE udfv_mv_s.an (pk int PRIMARY KEY, a text, n int, s2 text, nu text, x text)',
    "INSERT INTO udfv_mv_s.an (pk, x) VALUES (4, 'a b')",
    "INSERT INTO udfv_mv_s.plain (pk, txt) VALUES (1, 'abc')",
    "INSERT INTO udfv_mv_s.e (pk, v, txt) VALUES (1, 1, 'abc')",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt, asci, ci, lo, up, std, dl, big, d, b) VALUES (1, 'a', 1, 'apple', 'apple', 'Apple', 'Apple', 'Apple', 'The quick brown fox', 'a,b,c', 10, 1.5, true)",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt, asci, ci, lo, up, std, dl, big, d, b) VALUES (1, 'b', 2, 'banana', 'banana', 'BANANA', 'BANANA', 'banana', 'jumps over the lazy dog', 'b,d', 20, 2.5, false)",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt, asci, ci, lo, up, std, dl, big, d, b) VALUES (2, 'c', 3, 'pineapple', 'pineapple', 'PineApple', 'PineApple', 'PineApple', 'Quick thinking', 'x', 30, 3.5, true)",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt, asci, ci, lo, up, std, dl, big, d) VALUES (3, 'd', 4, 'apricot', 'apricot', 'aPRICOT', 'aPRICOT', 'aPRICOT', 'brown bear', 'c,a', 40, 4.5)",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt) VALUES (4, 'e', 5, 'grape')",
    "INSERT INTO udfv_mv_s.t (pk, ck, v, txt) VALUES (5, 'ab', 6, 'a%b')",
    "INSERT INTO udfv_mv_s.t (pk, ck, v) VALUES (6, 'f', 7)",
)
cases([
    ('sasi_default', 'CREATE CUSTOM INDEX t_txt ON udfv_mv_s.t (txt) ' + SASI),
    ('sasi_dup', 'CREATE CUSTOM INDEX t_txt_dup ON udfv_mv_s.t (txt) ' + SASI),
    ('sasi_dup_if_not_exists', 'CREATE CUSTOM INDEX IF NOT EXISTS t_txt_dup ON udfv_mv_s.t (txt) ' + SASI),
    ('sasi_name_exists', 'CREATE CUSTOM INDEX t_txt ON udfv_mv_s.t (v) ' + SASI),
    ('sasi_unnamed', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (v) ' + SASI),
    ('sasi_prefix', "CREATE CUSTOM INDEX t_asc ON udfv_mv_s.t (asci) " + SASI + " WITH OPTIONS = {'mode': 'PREFIX'}"),
    ('sasi_contains', "CREATE CUSTOM INDEX t_ci ON udfv_mv_s.t (ci) " + SASI + " WITH OPTIONS = {'mode': 'CONTAINS', 'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer', 'case_sensitive': 'false'}"),
    ('sasi_lower', "CREATE CUSTOM INDEX t_lo ON udfv_mv_s.t (lo) " + SASI + " WITH OPTIONS = {'mode': 'CONTAINS', 'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer', 'normalize_lowercase': 'true'}"),
    ('sasi_upper', "CREATE CUSTOM INDEX t_up ON udfv_mv_s.t (up) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer', 'normalize_uppercase': 'true'}"),
    ('sasi_standard', "CREATE CUSTOM INDEX t_std ON udfv_mv_s.t (std) " + SASI + " WITH OPTIONS = {'mode': 'CONTAINS', 'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.StandardAnalyzer', 'tokenization_normalize_lowercase': 'true'}"),
    ('sasi_sparse', "CREATE CUSTOM INDEX t_big ON udfv_mv_s.t (big) " + SASI + " WITH OPTIONS = {'mode': 'SPARSE'}"),
    ('sasi_double', 'CREATE CUSTOM INDEX t_d ON udfv_mv_s.t (d) ' + SASI),
    ('sasi_clustering', 'CREATE CUSTOM INDEX t_ck ON udfv_mv_s.t (ck) ' + SASI + " WITH OPTIONS = {'mode': 'CONTAINS'}"),
    ('sasi_static', 'CREATE CUSTOM INDEX t_s ON udfv_mv_s.t (s) ' + SASI),
    ('sasi_boolean', 'CREATE CUSTOM INDEX t_b ON udfv_mv_s.t (b) ' + SASI),
    ('sasi_lowercase_mode', "CREATE CUSTOM INDEX t_ts ON udfv_mv_s.t (ts) " + SASI + " WITH OPTIONS = {'mode': 'sparse'}"),
    # Errors.
    ('sasi_partition_key', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (pk) ' + SASI),
    ('sasi_collection', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (l) ' + SASI),
    ('sasi_collection_values', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (VALUES(l)) ' + SASI),
    ('sasi_frozen_full', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (FULL(fl)) ' + SASI),
    ('sasi_frozen', 'CREATE CUSTOM INDEX ON udfv_mv_s.t (fl) ' + SASI),
    ('sasi_bad_mode', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'mode': 'bogus'}"),
    ('sasi_contains_int', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'mode': 'CONTAINS'}"),
    ('sasi_sparse_text', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'mode': 'SPARSE'}"),
    ('sasi_sparse_analyzed', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'mode': 'SPARSE', 'analyzed': 'true'}"),
    ('sasi_sparse_literal', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'mode': 'SPARSE', 'is_literal': 'true'}"),
    ('sasi_unknown_analyzer', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'org.example.Nope'}"),
    ('sasi_not_analyzer', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'java.lang.String'}"),
    ('sasi_analyzer_int', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer'}"),
    ('sasi_standard_int', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.StandardAnalyzer'}"),
    ('sasi_case_and_normalize', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer', 'case_sensitive': 'false', 'normalize_lowercase': 'true'}"),
    ('sasi_unknown_option', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'nope': 'x'}"),
    ('sasi_target_option', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'target': 'v'}"),
    ('sasi_class_option', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'class_name': 'x'}"),
    ('sasi_not_custom', 'CREATE INDEX ON udfv_mv_s.e (txt) ' + SASI),
    ('sasi_short_name', "CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) USING 'SASIIndex'"),
    ('sasi_lowercase_class', "CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) USING 'org.apache.cassandra.index.sasi.sasiindex'"),
    ('sasi_delimiter_int', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.DelimiterAnalyzer'}"),
    ('sasi_bad_number', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'mode': 'PREFIX', 'max_compaction_flush_memory_in_mb': 'x'}"),
    ('sasi_bad_number_one', 'CREATE CUSTOM INDEX ON udfv_mv_s.e (txt) ' + SASI + " WITH OPTIONS = {'max_compaction_flush_memory_in_mb': 'x'}"),
    ('sasi_on_view_base_ok', 'CREATE CUSTOM INDEX e_v ON udfv_mv_s.e (v) ' + SASI + " WITH OPTIONS = {'is_literal': 'false', 'max_compaction_flush_memory_in_mb': '10'}"),
])
wait(SK)
cases([
    ('sasi_delimiter', 'CREATE CUSTOM INDEX dl_dl ON udfv_mv_s.dl (dl) ' + SASI + " WITH OPTIONS = {'mode': 'CONTAINS', 'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.DelimiterAnalyzer', 'delimiter': ','}"),
    ('sasi_delimiter_rows', "SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = 'udfv_mv_s' AND table_name = 'dl'"),
])
wait(SK)
setup(
    "INSERT INTO udfv_mv_s.dl (pk, dl) VALUES (1, 'a,b,c')",
    "INSERT INTO udfv_mv_s.dl (pk, dl) VALUES (2, 'b,d')",
    "INSERT INTO udfv_mv_s.dl (pk, dl) VALUES (3, 'xa,,ya')",
    "INSERT INTO udfv_mv_s.an (pk, a, n, s2, nu) VALUES (1, 'Hello World', 1, 'One Two', 'Mixed Case')",
    "INSERT INTO udfv_mv_s.an (pk, a, n, s2, nu) VALUES (2, 'hello there', 2, 'two three', 'mixed')",
    "INSERT INTO udfv_mv_s.an (pk, a, n, s2, nu) VALUES (3, 'it''s 3.14, ok', 3, 'ONE', 'MIXED CASE')",
)
cases([
    ('sq_dl_word', "SELECT pk FROM udfv_mv_s.dl WHERE dl LIKE 'b'"),
    ('sq_dl_contains', "SELECT pk FROM udfv_mv_s.dl WHERE dl LIKE '%a%'"),
    ('sq_dl_suffix', "SELECT pk FROM udfv_mv_s.dl WHERE dl LIKE '%a'"),
    ('sq_dl_prefix', "SELECT pk FROM udfv_mv_s.dl WHERE dl LIKE 'x%'"),
    ('sq_dl_eq', "SELECT pk FROM udfv_mv_s.dl WHERE dl = 'b'"),
    ('sq_dl_two', "SELECT pk FROM udfv_mv_s.dl WHERE dl LIKE 'b,d'"),
    ('sasi_delimiter_drop_table', 'DROP TABLE udfv_mv_s.dl'),
    ('sasi_analyzed_default', "CREATE CUSTOM INDEX an_a ON udfv_mv_s.an (a) " + SASI + " WITH OPTIONS = {'mode': 'CONTAINS', 'analyzed': 'true'}"),
    ('sasi_noop_int', "CREATE CUSTOM INDEX an_n ON udfv_mv_s.an (n) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NoOpAnalyzer'}"),
    ('sasi_standard_upper', "CREATE CUSTOM INDEX an_s2 ON udfv_mv_s.an (s2) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.StandardAnalyzer', 'tokenization_normalize_uppercase': 'true'}"),
    ('sasi_nontokenizing_upper_ci', "CREATE CUSTOM INDEX an_nu ON udfv_mv_s.an (nu) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.NonTokenizingAnalyzer', 'case_sensitive': 'false'}"),
    ('sasi_abstract_analyzer', "CREATE CUSTOM INDEX ON udfv_mv_s.an (nu) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.AbstractAnalyzer'}"),
    ('sasi_analyzer_short', "CREATE CUSTOM INDEX ON udfv_mv_s.an (nu) " + SASI + " WITH OPTIONS = {'analyzer_class': 'StandardAnalyzer'}"),
    ('sasi_analyzed_false', "CREATE CUSTOM INDEX an_x ON udfv_mv_s.an (x) " + SASI + " WITH OPTIONS = {'analyzer_class': 'org.apache.cassandra.index.sasi.analyzer.StandardAnalyzer', 'analyzed': 'false'}"),
])
wait(SK)
cases([
    ('sq_an_word', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE 'hello'"),
    ('sq_an_word_case', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE 'Hello'"),
    ('sq_an_contains', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE '%orl%'"),
    ('sq_an_apostrophe', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE 'it''s'"),
    ('sq_an_number', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE '3.14'"),
    ('sq_an_number_part', "SELECT pk FROM udfv_mv_s.an WHERE a LIKE '3'"),
    ('sq_an_eq', "SELECT pk FROM udfv_mv_s.an WHERE a = 'hello'"),
    ('sq_noop_eq', 'SELECT pk FROM udfv_mv_s.an WHERE n = 2'),
    ('sq_noop_range', 'SELECT pk FROM udfv_mv_s.an WHERE n > 1'),
    ('sq_s2_word', "SELECT pk FROM udfv_mv_s.an WHERE s2 LIKE 'one'"),
    ('sq_s2_prefix', "SELECT pk FROM udfv_mv_s.an WHERE s2 LIKE 'tw%'"),
    ('sq_nu_eq', "SELECT pk FROM udfv_mv_s.an WHERE nu = 'mixed case'"),
    ('sq_nu_prefix', "SELECT pk FROM udfv_mv_s.an WHERE nu LIKE 'MIX%'"),
    ('sq_nu_contains', "SELECT pk FROM udfv_mv_s.an WHERE nu LIKE '%case'"),
    ('sq_x_eq', "SELECT pk FROM udfv_mv_s.an WHERE x = 'a b'"),
    ('sq_x_like', "SELECT pk FROM udfv_mv_s.an WHERE x LIKE 'a%'"),
    ('sq_ci_with_pk', "SELECT pk, ck FROM udfv_mv_s.t WHERE pk = 1 AND ci LIKE 'BAN%'"),
    ('sq_ci_with_pk_eq', "SELECT pk, ck FROM udfv_mv_s.t WHERE pk = 1 AND ci = 'banana'"),
    ('sq_ci_with_token', "SELECT pk, ck FROM udfv_mv_s.t WHERE token(pk) > 0 AND ci LIKE '%a%'"),
])
q('sq_ci_paged', "SELECT pk, ck FROM udfv_mv_s.t WHERE ci LIKE '%a%'", page=2)
cases([
    ('sasi_rows_indexes', "SELECT table_name, index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = 'udfv_mv_s'"),
    ('sasi_rows_index_info', "SELECT * FROM system.\"IndexInfo\" WHERE table_name = 'udfv_mv_s'"),
], sort=True)
cases([
    # Text, default (PREFIX, literal, not analyzed).
    ('sq_eq', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt = 'apple'"),
    ('sq_eq_case', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt = 'Apple'"),
    ('sq_like_prefix', "SELECT pk, ck, txt FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sq_like_exact', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'apple'"),
    ('sq_like_percent_inside', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'a%b'"),
    ('sq_like_underscore', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'appl_'"),
    ('sq_like_suffix_prefix_mode', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE '%ple'"),
    ('sq_like_contains_prefix_mode', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE '%pl%'"),
    ('sq_like_only_percent', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE '%'"),
    ('sq_like_two_percent', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE '%%'"),
    ('sq_like_empty', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE ''"),
    ('sq_like_three_percent', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE '%%%'"),
    ('sq_range_text', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt > 'b'"),
    ('sq_range_text_filtering', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt > 'b' ALLOW FILTERING"),
    ('sq_neq_text', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt != 'apple'"),
    ('sq_in_text', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt IN ('apple', 'grape')"),
    ('sq_like_twice', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%' AND txt LIKE 'a%'"),
    ('sq_like_and_eq', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%' AND txt = 'apple'"),
    ('sq_like_with_pk', "SELECT pk, ck FROM udfv_mv_s.t WHERE pk = 1 AND txt LIKE 'ba%'"),
    ('sq_like_with_other', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%' AND v > 1"),
    ('sq_like_with_other_filtering', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%' AND v > 1 ALLOW FILTERING"),
    ('sq_like_unindexed', "SELECT pk FROM udfv_mv_s.plain WHERE txt LIKE 'a%'"),
    ('sq_like_unindexed_filtering', "SELECT pk FROM udfv_mv_s.plain WHERE txt LIKE 'a%' ALLOW FILTERING"),
    ('sq_like_on_pk', "SELECT pk FROM udfv_mv_s.plain WHERE pk LIKE 'a%'"),
    ('sq_like_on_e_sasi', "SELECT pk FROM udfv_mv_s.e WHERE txt LIKE 'a%'"),
    ('sq_like_int', "SELECT pk FROM udfv_mv_s.t WHERE v LIKE '1%'"),
    ('sq_like_bigint_sparse', "SELECT pk FROM udfv_mv_s.t WHERE big LIKE '1%'"),
    # ASCII PREFIX.
    ('sq_ascii_prefix', "SELECT pk, ck FROM udfv_mv_s.t WHERE asci LIKE 'ban%'"),
    ('sq_ascii_contains', "SELECT pk, ck FROM udfv_mv_s.t WHERE asci LIKE '%an%'"),
    # CONTAINS, case insensitive.
    ('sq_ci_eq', "SELECT pk, ck, ci FROM udfv_mv_s.t WHERE ci = 'apple'"),
    ('sq_ci_prefix', "SELECT pk, ck, ci FROM udfv_mv_s.t WHERE ci LIKE 'AP%'"),
    ('sq_ci_suffix', "SELECT pk, ck, ci FROM udfv_mv_s.t WHERE ci LIKE '%APPLE'"),
    ('sq_ci_contains', "SELECT pk, ck, ci FROM udfv_mv_s.t WHERE ci LIKE '%An%'"),
    ('sq_ci_exact', "SELECT pk, ck, ci FROM udfv_mv_s.t WHERE ci LIKE 'BaNaNa'"),
    ('sq_ci_range', "SELECT pk, ck FROM udfv_mv_s.t WHERE ci > 'a'"),
    ('sq_lower_eq', "SELECT pk, ck, lo FROM udfv_mv_s.t WHERE lo = 'apple'"),
    ('sq_lower_eq_upper', "SELECT pk, ck, lo FROM udfv_mv_s.t WHERE lo = 'APPLE'"),
    ('sq_lower_contains', "SELECT pk, ck, lo FROM udfv_mv_s.t WHERE lo LIKE '%pp%'"),
    ('sq_upper_eq', "SELECT pk, ck, up FROM udfv_mv_s.t WHERE up = 'apple'"),
    ('sq_upper_prefix', "SELECT pk, ck, up FROM udfv_mv_s.t WHERE up LIKE 'PINE%'"),
    # StandardAnalyzer (tokenized).
    ('sq_std_word', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE 'quick'"),
    ('sq_std_word_case', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE 'QUICK'"),
    ('sq_std_prefix', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE 'bro%'"),
    ('sq_std_contains', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE '%ump%'"),
    ('sq_std_suffix', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE '%own'"),
    ('sq_std_two_words', "SELECT pk, ck FROM udfv_mv_s.t WHERE std LIKE 'quick brown'"),
    ('sq_std_eq', "SELECT pk, ck FROM udfv_mv_s.t WHERE std = 'quick'"),
    # Numeric PREFIX / SPARSE.
    ('sq_int_eq', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v = 3'),
    ('sq_int_range', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v > 2 AND v <= 5'),
    ('sq_int_range_open', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v >= 6'),
    ('sq_int_neq', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v != 3'),
    ('sq_int_in', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v IN (1, 3)'),
    ('sq_sparse_eq', 'SELECT pk, ck FROM udfv_mv_s.t WHERE big = 20'),
    ('sq_sparse_range', 'SELECT pk, ck FROM udfv_mv_s.t WHERE big >= 20 AND big < 40'),
    ('sq_double_range', 'SELECT pk, ck FROM udfv_mv_s.t WHERE d > 2.0'),
    ('sq_bool_eq', 'SELECT pk, ck FROM udfv_mv_s.t WHERE b = true'),
    ('sq_two_indexes', "SELECT pk, ck FROM udfv_mv_s.t WHERE v > 1 AND txt LIKE 'ap%'"),
    ('sq_three_indexes', "SELECT pk, ck FROM udfv_mv_s.t WHERE v > 0 AND txt LIKE 'ap%' AND big < 40"),
    # Clustering and static columns.
    ('sq_ck_like', "SELECT pk, ck FROM udfv_mv_s.t WHERE ck LIKE '%b%'"),
    ('sq_ck_eq', "SELECT pk, ck FROM udfv_mv_s.t WHERE ck = 'b'"),
    ('sq_ck_range', "SELECT pk, ck FROM udfv_mv_s.t WHERE ck > 'c'"),
    ('sq_static', 'SELECT pk, ck FROM udfv_mv_s.t WHERE s = 1'),
    # Aggregates, limits, order.
    ('sq_count', "SELECT count(*) FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sq_limit', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%' LIMIT 1"),
    ('sq_order_by', "SELECT pk, ck FROM udfv_mv_s.t WHERE pk = 1 AND txt LIKE '%' ORDER BY ck DESC"),
    ('sq_order_by_like', "SELECT pk, ck FROM udfv_mv_s.t WHERE pk = 1 AND txt LIKE 'b%' ORDER BY ck DESC"),
    ('sq_distinct', "SELECT DISTINCT pk FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
])
q('sq_paged', 'SELECT pk, ck, v FROM udfv_mv_s.t WHERE v > 0', page=2)
q('sq_paged_like', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%'", page=1)
# SASI follows writes.
cases([
    ('sw_update', "UPDATE udfv_mv_s.t SET txt = 'apex' WHERE pk = 4 AND ck = 'e'"),
    ('sw_delete', "DELETE FROM udfv_mv_s.t WHERE pk = 1 AND ck = 'a'"),
    ('sw_delete_cell', "DELETE txt FROM udfv_mv_s.t WHERE pk = 3 AND ck = 'd'"),
    ('sw_insert', "INSERT INTO udfv_mv_s.t (pk, ck, v, txt) VALUES (7, 'g', 8, 'apology')"),
    ('sw_ttl', "INSERT INTO udfv_mv_s.t (pk, ck, v, txt) VALUES (8, 'h', 9, 'apart') USING TTL 2"),
    ('sw_after', "SELECT pk, ck, txt FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sw_after_int', 'SELECT pk, ck FROM udfv_mv_s.t WHERE v >= 8'),
])
sleep(3)
cases([
    ('sw_after_ttl', "SELECT pk, ck, txt FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sw_truncate', 'TRUNCATE udfv_mv_s.t'),
    ('sw_after_truncate', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sasi_drop', 'DROP INDEX udfv_mv_s.t_txt'),
    ('sq_after_drop', "SELECT pk, ck FROM udfv_mv_s.t WHERE txt LIKE 'ap%'"),
    ('sasi_drop_unknown', 'DROP INDEX udfv_mv_s.nope'),
    ('sasi_drop_column_indexed', 'ALTER TABLE udfv_mv_s.t DROP asci'),
])
setup('DROP KEYSPACE IF EXISTS udfv_mv_a', 'DROP KEYSPACE IF EXISTS udfv_mv_s', 'DROP KEYSPACE IF EXISTS udfv_mv_other')

# ---- client and runner ---------------------------------------------------------------------------
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from auth_live import C, rs  # noqa: E402

TYPES = {1: 'ascii', 2: 'bigint', 3: 'blob', 4: 'boolean', 5: 'counter', 6: 'decimal', 7: 'double', 8: 'float', 9: 'int',
         11: 'timestamp', 12: 'uuid', 13: 'text', 14: 'varint', 15: 'timeuuid', 16: 'inet', 17: 'date', 18: 'time',
         19: 'smallint', 20: 'tinyint', 21: 'duration'}


def rtype(b, o):
    t = struct.unpack_from('>H', b, o)[0]; o += 2
    if t == 0:
        s, o = rs(b, o); return 'custom(%s)' % s, o
    if t in (32, 34):
        e, o = rtype(b, o); return '%s<%s>' % ('list' if t == 32 else 'set', e), o
    if t == 33:
        k, o = rtype(b, o); v, o = rtype(b, o); return 'map<%s, %s>' % (k, v), o
    if t == 48:
        _, o = rs(b, o); name, o = rs(b, o); n = struct.unpack_from('>H', b, o)[0]; o += 2
        for _ in range(n):
            _, o = rs(b, o); _, o = rtype(b, o)
        return 'udt(%s)' % name, o
    if t == 49:
        n = struct.unpack_from('>H', b, o)[0]; o += 2; items = []
        for _ in range(n):
            x, o = rtype(b, o); items.append(x)
        return 'tuple<%s>' % ', '.join(items), o
    return TYPES.get(t, str(t)), o


INTS = {'int': 4, 'bigint': 8, 'smallint': 2, 'tinyint': 1, 'counter': 8}


def fmt(ty, v):
    if v is None:
        return 'null'
    if ty in ('text', 'ascii'):
        return v.decode('utf8', 'replace')
    if ty == 'boolean':
        return 'True' if v[0] else 'False'
    if ty in INTS and len(v) == INTS[ty]:
        return str(int.from_bytes(v, 'big', signed=True))
    if ty.startswith('set<') or ty.startswith('list<'):
        inner = ty[ty.index('<') + 1:-1]; n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            l = struct.unpack_from('>i', v, o)[0]; o += 4; items.append(fmt1(inner, v[o:o + l])); o += l
        return ('{%s}' if ty.startswith('set') else '[%s]') % ', '.join(items)
    if ty.startswith('map<'):
        k, val = ty[4:-1].split(', ', 1); n = struct.unpack_from('>i', v, 0)[0]; o = 4; items = []
        for _ in range(n):
            l = struct.unpack_from('>i', v, o)[0]; o += 4; a = fmt1(k, v[o:o + l]); o += l
            l = struct.unpack_from('>i', v, o)[0]; o += 4; b = fmt1(val, v[o:o + l]); o += l
            items.append('%s: %s' % (a, b))
        return '{%s}' % ', '.join(items)
    return '0x' + v.hex()


def fmt1(ty, v):
    s = fmt(ty, v)
    return "'%s'" % s if ty in ('text', 'ascii') else s


def query(c, text, page_size=None, paging_state=None):
    flags = 0; extra = b''
    if page_size:
        flags |= 0x04; extra += struct.pack('>i', page_size)
    if paging_state is not None:
        flags |= 0x08; extra += struct.pack('>i', len(paging_state)) + paging_state
    body = text.encode()
    op, fl, b = c.req(7, struct.pack('>i', len(body)) + body + struct.pack('>HB', 1, flags) + extra)
    o = 0; out = {}
    if fl & 8:
        n = struct.unpack_from('>H', b, o)[0]; o += 2; w = []
        for _ in range(n):
            x, o = rs(b, o); w.append(x)
        out['warnings'] = w
    b = b[o:]
    if op == 0:
        code = struct.unpack_from('>i', b, 0)[0]; m, _ = rs(b, 4)
        out.update({'code': '0x%04X' % code, 'message': m}); return out, None
    kind = struct.unpack_from('>i', b, 0)[0]
    out['kind'] = {1: 'Void', 2: 'Rows', 3: 'SetKeyspace', 4: 'Prepared', 5: 'SchemaChange'}[kind]
    if kind == 5:
        o = 4; parts = []
        for _ in range(3):
            x, o = rs(b, o); parts.append(x)
        if parts[1] != 'KEYSPACE':
            x, o = rs(b, o); parts.append(x)
        out['change'] = ' '.join(parts)
    if kind != 2:
        return out, None
    o = 4; mf, n = struct.unpack_from('>ii', b, o); o += 8; state = None
    if mf & 2:
        l = struct.unpack_from('>i', b, o)[0]; o += 4; state = b[o:o + l]; o += l
    if mf & 1:
        _, o = rs(b, o); _, o = rs(b, o)
    cols = []
    for _ in range(n):
        if not mf & 1:
            _, o = rs(b, o); _, o = rs(b, o)
        name, o = rs(b, o); ty, o = rtype(b, o); cols.append([name, ty])
    out['cols'] = cols
    rows = []
    nr = struct.unpack_from('>i', b, o)[0]; o += 4
    for _ in range(nr):
        r = []
        for ci in range(n):
            l = struct.unpack_from('>i', b, o)[0]; o += 4
            r.append(fmt(cols[ci][1], None if l < 0 else b[o:o + l])); o += max(l, 0)
        rows.append(r)
    out['rows'] = rows
    return out, state


def wait_built(c, ks):
    for _ in range(240):
        want = {r[0] for r in query(c, "SELECT view_name FROM system_schema.views WHERE keyspace_name = '%s'" % ks)[0].get('rows', [])}
        built = {r[0] for r in query(c, "SELECT view_name FROM system.built_views WHERE keyspace_name = '%s'" % ks)[0].get('rows', [])}
        idx = {r[0] for r in query(c, "SELECT index_name FROM system_schema.indexes WHERE keyspace_name = '%s'" % ks)[0].get('rows', [])}
        ibuilt = {r[0] for r in query(c, "SELECT index_name FROM system.\"IndexInfo\" WHERE table_name = '%s'" % ks)[0].get('rows', [])}
        if want <= built and idx <= ibuilt:
            break
        time.sleep(0.5)
    time.sleep(1)


def run(port):
    c = C(port, 'cassandra', 'cassandra')
    out = []
    for cid, cql, o in S:
        if cid == 'wait':
            wait_built(c, cql); out.append({'id': 'wait', 'cql': cql}); continue
        if cid == 'sleep':
            time.sleep(float(cql)); out.append({'id': 'sleep', 'cql': cql}); continue
        try:
            if o.get('page'):
                pages = []; state = None
                while True:
                    r, state = query(c, cql, o['page'], state)
                    pages.append(r)
                    if state is None or 'code' in r or len(pages) >= 50:
                        break
                r = {'pages': pages}
            else:
                r, _ = query(c, cql)
        except Exception as e:
            r = {'client': repr(e)}
            c = C(port, 'cassandra', 'cassandra')
        if cid == 'setup':
            out.append({'id': 'setup', 'cql': cql})
            continue
        if o.get('sort'):
            for p in r.get('pages', [r]):
                if 'rows' in p:
                    p['rows'].sort()
        out.append({'id': cid, 'cql': cql, 'page': o.get('page'), 'sort': bool(o.get('sort')), 'result': r})
    return out


if __name__ == '__main__':
    import json
    port, path = int(sys.argv[1]), sys.argv[2]
    json.dump(run(port), open(path, 'w'), indent=1)
    print(len(S))
