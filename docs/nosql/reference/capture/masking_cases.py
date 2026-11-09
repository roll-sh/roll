# Dynamic data masking cases: (id, who, statement) run in order after CLEAN; who is a session in
# CREDS. `!sleep s` waits out Cassandra's 2 s auth caches, `!page n <cql>` reads every page of n
# rows, `!prepared <cql>` prepares and executes. Keyspaces and roles use the udfv_mask_ prefix.

KS = "CREATE KEYSPACE %s WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"
K = 'udfv_mask_ks'
K2 = 'udfv_mask_ks2'
ROLES = ['udfv_mask_owner', 'udfv_mask_r', 'udfv_mask_sm', 'udfv_mask_um', 'udfv_mask_ksum',
         'udfv_mask_allum', 'udfv_mask_all', 'udfv_mask_parent', 'udfv_mask_child', 'udfv_mask_w',
         'udfv_mask_kssm', 'udfv_mask_mv']
CREDS = {'su': ('cassandra', 'cassandra')}
for r in ROLES:
    CREDS[r[len('udfv_mask_'):]] = (r, 'pw')
CLEAN = ['DROP KEYSPACE IF EXISTS %s' % k for k in (K, K2)] + ['DROP ROLE IF EXISTS %s' % r for r in ROLES]


def ddl():
    C = []
    def c(cid, q, who='su'): C.append((cid, who, q))
    c('setup_ks', KS % K)
    c('setup_ks2', KS % K2)
    c('setup_udt', 'CREATE TYPE %s.addr (street text, zip int)' % K)
    masks = "SELECT * FROM system_schema.column_masks WHERE keyspace_name = '%s'" % K
    # Every masking function and argument form.
    c('create_forms', 'CREATE TABLE %s.forms (pk int PRIMARY KEY, a text MASKED WITH DEFAULT, b int MASKED WITH mask_default(), '
      'c text MASKED WITH mask_null(), d text MASKED WITH mask_replace(\'redacted\'), e int MASKED WITH mask_replace(-1), '
      'f text MASKED WITH mask_inner(1, 2), g text MASKED WITH mask_inner(1, null, \'#\'), h text MASKED WITH mask_outer(null, 1), '
      'i ascii MASKED WITH mask_outer(2, 2, \'-\'), j blob MASKED WITH mask_hash(), k blob MASKED WITH mask_hash(\'SHA-512\'), '
      'l text MASKED WITH system.mask_inner(0, 0), m varchar MASKED WITH MASK_INNER(3, 3), n text MASKED WITH "mask_default"(), '
      'o text MASKED WITH mask_replace(null), p blob MASKED WITH mask_hash(null))' % K)
    c('masks_forms', masks)
    c('describe_forms', 'DESCRIBE TABLE %s.forms' % K)
    # Masks on every kind of column and type.
    c('create_types', 'CREATE TABLE %s.types (pk int, ck text MASKED WITH DEFAULT, s int STATIC MASKED WITH mask_replace(7), '
      'bi bigint MASKED WITH DEFAULT, si smallint MASKED WITH DEFAULT, ti tinyint MASKED WITH DEFAULT, vi varint MASKED WITH DEFAULT, '
      'de decimal MASKED WITH DEFAULT, db double MASKED WITH DEFAULT, fl float MASKED WITH DEFAULT, bo boolean MASKED WITH DEFAULT, '
      'ts timestamp MASKED WITH DEFAULT, da date MASKED WITH DEFAULT, tm time MASKED WITH DEFAULT, u uuid MASKED WITH DEFAULT, '
      'tu timeuuid MASKED WITH DEFAULT, ip inet MASKED WITH DEFAULT, du duration MASKED WITH DEFAULT, bl blob MASKED WITH DEFAULT, '
      'as1 ascii MASKED WITH DEFAULT, l list<int> MASKED WITH DEFAULT, st set<text> MASKED WITH DEFAULT, '
      'mp map<text, int> MASKED WITH DEFAULT, fli frozen<list<int>> MASKED WITH DEFAULT, ad addr MASKED WITH DEFAULT, '
      'fad frozen<addr> MASKED WITH DEFAULT, tp tuple<int, text> MASKED WITH DEFAULT, vec vector<float, 2> MASKED WITH DEFAULT, '
      'PRIMARY KEY (pk, ck))' % K)
    c('masks_types', masks.replace("'%s'" % K, "'%s' AND table_name = 'types'" % K))
    c('describe_types', 'DESCRIBE TABLE %s.types' % K)
    c('create_coll_replace', 'CREATE TABLE %s.coll (pk int PRIMARY KEY, l list<int> MASKED WITH mask_replace([1, 2]), '
      'st set<text> MASKED WITH mask_replace({\'x\'}), mp map<text, int> MASKED WITH mask_replace({\'k\': 0}), '
      'ad frozen<addr> MASKED WITH mask_replace({street: \'x\', zip: 0}), tp tuple<int, text> MASKED WITH mask_replace((0, \'x\')), '
      'ln list<int> MASKED WITH mask_null(), ts timestamp MASKED WITH mask_replace(\'2020-01-01 00:00:00+0000\'), '
      'u uuid MASKED WITH mask_replace(00000000-0000-0000-0000-000000000000), bo boolean MASKED WITH mask_replace(true), '
      'de decimal MASKED WITH mask_replace(1.5), bl blob MASKED WITH mask_replace(0xcafe), du duration MASKED WITH mask_replace(1h))' % K)
    c('masks_coll', masks.replace("'%s'" % K, "'%s' AND table_name = 'coll'" % K))
    c('describe_coll', 'DESCRIBE TABLE %s.coll' % K)
    c('create_pk_masked', 'CREATE TABLE %s.keys (p1 int MASKED WITH DEFAULT, p2 text MASKED WITH mask_inner(1, 1), c1 int MASKED WITH mask_replace(0), c2 text, v int, PRIMARY KEY ((p1, p2), c1, c2)) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC)' % K)
    c('masks_keys', masks.replace("'%s'" % K, "'%s' AND table_name = 'keys'" % K))
    c('describe_keys', 'DESCRIBE TABLE %s.keys' % K)
    c('create_counter', 'CREATE TABLE %s.cnt (pk int PRIMARY KEY, n counter MASKED WITH DEFAULT)' % K)
    c('create_counter_replace', 'CREATE TABLE %s.cnt2 (pk int PRIMARY KEY, n counter MASKED WITH mask_replace(1))' % K)
    c('create_counter_null', 'CREATE TABLE %s.cnt3 (pk int PRIMARY KEY, n counter MASKED WITH mask_null())' % K)
    c('masks_counter', "SELECT table_name, column_name, function_name FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name IN ('cnt', 'cnt2', 'cnt3')" % K)
    # What every function and type masks to, for a role without UNMASK.
    c('role_r', "CREATE ROLE udfv_mask_r WITH PASSWORD = 'pw' AND LOGIN = true")
    c('grant_r', 'GRANT SELECT ON KEYSPACE %s TO udfv_mask_r' % K)
    c('insert_forms', "INSERT INTO %s.forms (pk, a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p) VALUES (1, 'alpha', 42, 'gamma', 'delta', 7, "
      "'abcdefgh', 'abcdefgh', 'abcdefgh', 'abcdefgh', 0xcafe, 0xcafe, 'lm', 'abcdefgh', 'n', 'o', 0x01)" % K)
    c('insert_forms_empty', "INSERT INTO %s.forms (pk, a, f, h, j) VALUES (2, '', '', 'x', 0x)" % K)
    c('insert_forms_nulls', 'INSERT INTO %s.forms (pk) VALUES (3)' % K)
    c('insert_types', "INSERT INTO %s.types (pk, ck, s, bi, si, ti, vi, de, db, fl, bo, ts, da, tm, u, tu, ip, du, bl, as1, l, st, mp, fli, ad, fad, tp, vec) "
      "VALUES (1, 'c', 5, 9, 9, 9, 9, 9.5, 9.5, 9.5, true, '2021-02-03 04:05:06+0000', '2021-02-03', '04:05:06', "
      "11111111-2222-3333-4444-555555555555, 50554d6e-29bb-11e5-b345-feff819cdc9f, '10.0.0.1', 3d, 0xff, 'asc', [1, 2], {'a', 'b'}, "
      "{'k': 1}, [3], {street: 's', zip: 1}, {street: 'f', zip: 2}, (1, 'one'), [1.5, 2.5])" % K)
    c('insert_coll', "INSERT INTO %s.coll (pk, l, st, mp, ad, tp, ln, ts, u, bo, de, bl, du) VALUES (1, [5], {'z'}, {'q': 9}, "
      "{street: 'real', zip: 9}, (9, 'nine'), [7], '2022-01-01 00:00:00+0000', 11111111-2222-3333-4444-555555555555, false, 9.9, 0x99, 2d)" % K)
    c('insert_keys', "INSERT INTO %s.keys (p1, p2, c1, c2, v) VALUES (1, 'secret', 5, 'c', 9)" % K)
    c('insert_keys2', "INSERT INTO %s.keys (p1, p2, c1, c2, v) VALUES (1, 'secret', 6, 'd', 10)" % K)
    c('update_cnt', 'UPDATE %s.cnt SET n = n + 5 WHERE pk = 1' % K)
    c('update_cnt2', 'UPDATE %s.cnt2 SET n = n + 5 WHERE pk = 1' % K)
    c('update_cnt3', 'UPDATE %s.cnt3 SET n = n + 5 WHERE pk = 1' % K)
    c('sleep', '!sleep 2.5', '-')
    for t in ['forms', 'types', 'coll', 'keys', 'cnt', 'cnt2', 'cnt3']:
        c('read_%s_r' % t, 'SELECT * FROM %s.%s' % (K, t), 'r')
        c('read_%s_su' % t, 'SELECT * FROM %s.%s' % (K, t))
    c('read_types_json_r', 'SELECT JSON * FROM %s.types' % K, 'r')
    c('read_keys_token_r', 'SELECT token(p1, p2), p1, p2, c1 FROM %s.keys' % K, 'r')
    c('read_keys_order_r', 'SELECT c1, c2 FROM %s.keys WHERE p1 = 1 AND p2 = \'secret\'' % K, 'r')
    c('read_types_static_r', 'SELECT DISTINCT pk, s FROM %s.types' % K, 'r')
    c('cnt_update_masked', 'UPDATE %s.cnt SET n = n + 1 WHERE pk = 1' % K, 'r')
    # Errors.
    for cid, col in [
        ('err_unknown_fn', 'v int MASKED WITH nope()'),
        ('err_unknown_fn_args', 'v int MASKED WITH nope(1, \'a\')'),
        ('err_unknown_fn_ks', 'v int MASKED WITH %s.nope()' % K),
        ('err_unknown_ks_fn', 'v int MASKED WITH udfv_mask_nope.nope()'),
        ('err_native_in_ks', 'v text MASKED WITH %s.mask_inner(1, 1)' % K),
        ('err_not_masking', 'v int MASKED WITH abs()'),
        ('err_not_masking_now', 'v timeuuid MASKED WITH now()'),
        ('err_not_masking_totimestamp', 'v timestamp MASKED WITH totimestamp()'),
        ('err_not_masking_blobastext', 'v blob MASKED WITH textasblob()'),
        ('err_aggregate_sum', 'v int MASKED WITH sum()'),
        ('err_aggregate_count', 'v int MASKED WITH count()'),
        ('err_aggregate_max', 'v text MASKED WITH max()'),
        ('err_hash_text', 'v text MASKED WITH mask_hash()'),
        ('err_hash_int_arg', 'v blob MASKED WITH mask_hash(1)'),
        ('err_hash_algo', 'v blob MASKED WITH mask_hash(\'nope\')'),
        ('err_inner_int', 'v int MASKED WITH mask_inner(1, 1)'),
        ('err_inner_args', 'v text MASKED WITH mask_inner(1)'),
        ('err_inner_too_many', 'v text MASKED WITH mask_inner(1, 1, \'#\', 1)'),
        ('err_inner_pad', 'v text MASKED WITH mask_inner(1, 1, \'##\')'),
        ('err_inner_negative', 'v text MASKED WITH mask_inner(-1, 1)'),
        ('err_inner_text_arg', 'v text MASKED WITH mask_inner(\'a\', 1)'),
        ('err_outer_pad_empty', 'v text MASKED WITH mask_outer(1, 1, \'\')'),
        ('err_replace_type', 'v int MASKED WITH mask_replace(\'x\')'),
        ('err_replace_none', 'v int MASKED WITH mask_replace()'),
        ('err_replace_two', 'v int MASKED WITH mask_replace(1, 2)'),
        ('err_replace_bigint_int', 'v bigint MASKED WITH mask_replace(1)'),
        ('err_replace_list_type', 'v list<int> MASKED WITH mask_replace([\'a\'])'),
        ('err_null_arg', 'v int MASKED WITH mask_null(1)'),
        ('err_default_arg', 'v int MASKED WITH mask_default(1)'),
        ('err_bind_marker', 'v int MASKED WITH mask_replace(?)'),
        ('err_named_bind', 'v int MASKED WITH mask_replace(:x)'),
        ('err_fn_arg', 'v int MASKED WITH mask_replace(abs(1))'),
        ('err_cast_arg', 'v int MASKED WITH mask_replace((int) 1)'),
        ('err_inner_varchar_ascii', 'v ascii MASKED WITH mask_inner(1, 1, \'é\')'),
        ('err_mask_frozen_hash', 'v frozen<list<int>> MASKED WITH mask_hash()'),
    ]:
        c(cid, 'CREATE TABLE %s.e (pk int PRIMARY KEY, %s)' % (K, col))
    c('err_unknown_ks_table', 'CREATE TABLE udfv_mask_nope.e (pk int PRIMARY KEY, v int MASKED WITH DEFAULT)')
    c('err_masked_static_no_ck', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v int STATIC MASKED WITH DEFAULT)' % K)
    # User functions.
    c('udf_redact', "CREATE FUNCTION %s.redact(v text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE java AS 'return \"redacted\";'" % K)
    c('udf_suffix', "CREATE FUNCTION %s.suffix(v text, s text) CALLED ON NULL INPUT RETURNS text LANGUAGE java AS 'return s;'" % K)
    c('udf_int', "CREATE FUNCTION %s.toint(v text) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return 1;'" % K)
    c('udf_other_ks', "CREATE FUNCTION %s.redact(v text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE java AS 'return \"other\";'" % K2)
    c('udf_state', "CREATE FUNCTION %s.st(a text, v text) CALLED ON NULL INPUT RETURNS text LANGUAGE java AS 'return v;'" % K)
    c('uda', "CREATE AGGREGATE %s.agg(text) SFUNC st STYPE text" % K)
    c('create_udf_masked', 'CREATE TABLE %s.u (pk int PRIMARY KEY, a text MASKED WITH redact(), b text MASKED WITH %s.suffix(\'!\'), c text MASKED WITH suffix(null))' % (K, K))
    c('masks_udf', masks.replace("'%s'" % K, "'%s' AND table_name = 'u'" % K))
    c('insert_udf_masked', "INSERT INTO %s.u (pk, a, b, c) VALUES (1, 'aa', 'bb', 'cc')" % K)
    c('read_udf_masked_r', 'SELECT * FROM %s.u' % K, 'r')
    c('read_udf_masked_su', 'SELECT * FROM %s.u' % K)
    c('describe_udf', 'DESCRIBE TABLE %s.u' % K)
    c('err_udf_return', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v text MASKED WITH toint())' % K)
    c('err_udf_other_ks', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v text MASKED WITH %s.redact())' % (K, K2))
    c('err_uda', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v text MASKED WITH agg())' % K)
    c('err_udf_args', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v text MASKED WITH redact(1))' % K)
    c('err_udf_arg_type', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v int MASKED WITH redact())' % K)
    c('err_udf_ks_from_other', 'CREATE TABLE %s.e (pk int PRIMARY KEY, v text MASKED WITH redact())' % K2)
    c('udf_masked_view', 'CREATE MATERIALIZED VIEW %s.uv AS SELECT pk, a FROM %s.u WHERE pk IS NOT NULL AND a IS NOT NULL PRIMARY KEY (a, pk)' % (K, K))
    c('masks_udf_view', "SELECT table_name, column_name, function_keyspace, function_name FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name = 'uv'" % K)
    c('read_udf_view_r', 'SELECT * FROM %s.uv' % K, 'r')
    c('drop_udf_in_use', 'DROP FUNCTION %s.redact' % K)
    c('drop_udf_in_use_sig', 'DROP FUNCTION %s.suffix(text, text)' % K)
    c('replace_udf_in_use', "CREATE OR REPLACE FUNCTION %s.redact(v text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE java AS 'return \"again\";'" % K)
    # ALTER TABLE forms.
    c('setup_alt', 'CREATE TABLE %s.alt (pk int, ck int, s text STATIC, v text, w int, PRIMARY KEY (pk, ck))' % K)
    for cid, q in [
        ('alter_mask', 'ALTER TABLE %s.alt ALTER v MASKED WITH mask_inner(1, 1)'),
        ('alter_mask_same', 'ALTER TABLE %s.alt ALTER v MASKED WITH mask_inner(1, 1)'),
        ('alter_mask_same_qualified', 'ALTER TABLE %s.alt ALTER v MASKED WITH system.mask_inner(1, 1)'),
        ('alter_mask_change_args', 'ALTER TABLE %s.alt ALTER v MASKED WITH mask_inner(2, 1)'),
        ('alter_mask_change_fn', 'ALTER TABLE %s.alt ALTER v MASKED WITH DEFAULT'),
        ('alter_mask_default_again', 'ALTER TABLE %s.alt ALTER v MASKED WITH mask_default()'),
        ('alter_mask_pk', 'ALTER TABLE %s.alt ALTER pk MASKED WITH DEFAULT'),
        ('alter_mask_ck', 'ALTER TABLE %s.alt ALTER ck MASKED WITH mask_replace(0)'),
        ('alter_mask_static', 'ALTER TABLE %s.alt ALTER s MASKED WITH mask_null()'),
        ('alter_mask_unknown_col', 'ALTER TABLE %s.alt ALTER nope MASKED WITH DEFAULT'),
        ('alter_mask_if_exists_col', 'ALTER TABLE %s.alt ALTER IF EXISTS nope MASKED WITH DEFAULT'),
        ('alter_mask_unknown_table', 'ALTER TABLE %s.nope ALTER v MASKED WITH DEFAULT'),
        ('alter_mask_if_exists_table', 'ALTER TABLE IF EXISTS %s.nope ALTER v MASKED WITH DEFAULT'),
        ('alter_mask_bad_fn', 'ALTER TABLE %s.alt ALTER w MASKED WITH mask_inner(1, 1)'),
        ('alter_mask_udf', 'ALTER TABLE %s.alt ALTER v MASKED WITH redact()'),
        ('alter_drop_mask', 'ALTER TABLE %s.alt ALTER v DROP MASKED'),
        ('alter_drop_mask_again', 'ALTER TABLE %s.alt ALTER v DROP MASKED'),
        ('alter_drop_mask_unmasked', 'ALTER TABLE %s.alt ALTER w DROP MASKED'),
        ('alter_drop_mask_unknown', 'ALTER TABLE %s.alt ALTER nope DROP MASKED'),
        ('alter_drop_mask_if_exists', 'ALTER TABLE %s.alt ALTER IF EXISTS nope DROP MASKED'),
        ('alter_add_masked', 'ALTER TABLE %s.alt ADD x text MASKED WITH mask_outer(1, 1)'),
        ('alter_add_masked_multi', 'ALTER TABLE %s.alt ADD (y int MASKED WITH mask_replace(5), z text)'),
        ('alter_add_masked_static', 'ALTER TABLE %s.alt ADD s2 int STATIC MASKED WITH DEFAULT'),
        ('alter_add_masked_bad', 'ALTER TABLE %s.alt ADD q int MASKED WITH mask_inner(1, 1)'),
        ('alter_add_ifne_existing', 'ALTER TABLE %s.alt ADD IF NOT EXISTS x text MASKED WITH DEFAULT'),
        ('alter_rename_masked_ck', 'ALTER TABLE %s.alt RENAME ck TO ck2'),
        ('alter_drop_masked_col', 'ALTER TABLE %s.alt DROP x'),
        ('alter_type_masked', 'ALTER TABLE %s.alt ALTER y TYPE bigint'),
    ]:
        c(cid, q % K)
    c('masks_alt', masks.replace("'%s'" % K, "'%s' AND table_name = 'alt'" % K))
    c('describe_alt', 'DESCRIBE TABLE %s.alt' % K)
    c('alter_readd_dropped', 'ALTER TABLE %s.alt ADD x text' % K)
    c('masks_alt_readd', masks.replace("'%s'" % K, "'%s' AND table_name = 'alt' AND column_name = 'x'" % K))
    # Materialized views.
    c('setup_mvbase', 'CREATE TABLE %s.mvbase (pk int, ck int MASKED WITH mask_replace(0), v text MASKED WITH mask_inner(1, 1), w int, PRIMARY KEY (pk, ck))' % K)
    c('create_mv', 'CREATE MATERIALIZED VIEW %s.mv AS SELECT * FROM %s.mvbase WHERE pk IS NOT NULL AND ck IS NOT NULL AND w IS NOT NULL PRIMARY KEY (w, pk, ck)' % (K, K))
    c('create_mv_some', 'CREATE MATERIALIZED VIEW %s.mv2 AS SELECT pk, ck, w FROM %s.mvbase WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)' % (K, K))
    c('masks_mv', "SELECT * FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name IN ('mvbase', 'mv', 'mv2')" % K)
    c('describe_mv', 'DESCRIBE MATERIALIZED VIEW %s.mv' % K)
    c('describe_mvbase', 'DESCRIBE TABLE %s.mvbase' % K)
    c('alter_base_mask_w', 'ALTER TABLE %s.mvbase ALTER w MASKED WITH mask_replace(9)' % K)
    c('alter_base_drop_v', 'ALTER TABLE %s.mvbase ALTER v DROP MASKED' % K)
    c('masks_mv_after', "SELECT * FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name IN ('mvbase', 'mv', 'mv2')" % K)
    c('describe_mv_after', 'DESCRIBE MATERIALIZED VIEW %s.mv' % K)
    c('alter_view_mask', 'ALTER TABLE %s.mv ALTER v MASKED WITH DEFAULT' % K)
    c('alter_view_drop_mask', 'ALTER TABLE %s.mv ALTER v DROP MASKED' % K)
    c('alter_mv_masked', 'ALTER MATERIALIZED VIEW %s.mv ALTER v MASKED WITH DEFAULT' % K)
    c('drop_mv', 'DROP MATERIALIZED VIEW %s.mv2' % K)
    c('masks_mv_dropped', "SELECT table_name, column_name FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name IN ('mvbase', 'mv', 'mv2')" % K)
    # Drops.
    c('drop_table', 'DROP TABLE %s.forms' % K)
    c('masks_after_drop', "SELECT table_name FROM system_schema.column_masks WHERE keyspace_name = '%s' AND table_name = 'forms'" % K)
    c('drop_ks', 'DROP KEYSPACE %s' % K)
    c('masks_after_drop_ks', masks)
    return CLEAN, C



def reads():
    C = []
    def c(cid, q, who='su'): C.append((cid, who, q))
    c('setup_ks', KS % K)
    c('setup_udt', 'CREATE TYPE %s.addr (street text, zip int)' % K)
    c('setup_t', 'CREATE TABLE %s.t (pk int, ck int MASKED WITH mask_replace(0), s text STATIC MASKED WITH mask_inner(0, 1), '
      'v text MASKED WITH mask_inner(1, 1), n int MASKED WITH DEFAULT, l list<int> MASKED WITH DEFAULT, '
      'm map<text, int> MASKED WITH mask_null(), u frozen<addr> MASKED WITH DEFAULT, b blob MASKED WITH mask_hash(), plain text, '
      'PRIMARY KEY (pk, ck))' % K)
    c('setup_k', 'CREATE TABLE %s.k (p int MASKED WITH mask_replace(100), q int, v text MASKED WITH mask_outer(1, 0), PRIMARY KEY (p))' % K)
    c('setup_t_index_n', 'CREATE INDEX t_n ON %s.t (n)' % K)
    c('setup_t_index_plain', 'CREATE INDEX t_plain ON %s.t (plain)' % K)
    for pk, ck, v, n in [(1, 1, 'alice@example.com', 10), (1, 2, 'bob@example.com', 20), (1, 3, 'carol', 30), (2, 1, 'dave', 40), (2, 5, 'eve', 50), (3, 7, 'x', 60)]:
        c('insert_%d_%d' % (pk, ck), "INSERT INTO %s.t (pk, ck, s, v, n, l, m, u, b, plain) VALUES (%d, %d, 'static%d', '%s', %d, [%d, 1], {'a': %d}, {street: 'main %d', zip: %d}, 0x%02x, 'p%d') USING TIMESTAMP %d"
          % (K, pk, ck, pk, v, n, n, n, n, n, n, ck, 1000 + n))
    for p, q, v in [(1, 10, 'one'), (2, 20, 'two'), (3, 30, 'three'), (4, 40, 'four')]:
        c('insert_k_%d' % p, "INSERT INTO %s.k (p, q, v) VALUES (%d, %d, '%s')" % (K, p, q, v))
    # Roles and grants.
    for r in ROLES:
        login = 'false' if r == 'udfv_mask_parent' else 'true'
        c('role_' + r, "CREATE ROLE %s WITH PASSWORD = 'pw' AND LOGIN = %s" % (r, login))
    for cid, q in [
        ('grant_owner_create', 'GRANT CREATE ON KEYSPACE %s TO udfv_mask_owner'),
        ('grant_r_t', 'GRANT SELECT ON %s.t TO udfv_mask_r'),
        ('grant_r_k', 'GRANT SELECT ON %s.k TO udfv_mask_r'),
        ('grant_sm_t', 'GRANT SELECT ON %s.t TO udfv_mask_sm'),
        ('grant_sm_k', 'GRANT SELECT ON %s.k TO udfv_mask_sm'),
        ('grant_sm_t_masked', 'GRANT SELECT_MASKED ON %s.t TO udfv_mask_sm'),
        ('grant_sm_k_masked', 'GRANT SELECT_MASKED ON TABLE %s.k TO udfv_mask_sm'),
        ('grant_um_t', 'GRANT SELECT ON %s.t TO udfv_mask_um'),
        ('grant_um_k', 'GRANT SELECT ON %s.k TO udfv_mask_um'),
        ('grant_um_t_unmask', 'GRANT UNMASK ON %s.t TO udfv_mask_um'),
        ('grant_um_k_unmask', 'GRANT UNMASK ON %s.k TO udfv_mask_um'),
        ('grant_ksum_select', 'GRANT SELECT ON KEYSPACE %s TO udfv_mask_ksum'),
        ('grant_ksum_unmask', 'GRANT UNMASK ON KEYSPACE %s TO udfv_mask_ksum'),
        ('grant_kssm_select', 'GRANT SELECT ON KEYSPACE %s TO udfv_mask_kssm'),
        ('grant_kssm_masked', 'GRANT SELECT_MASKED ON KEYSPACE %s TO udfv_mask_kssm'),
        ('grant_allum_select', 'GRANT SELECT ON ALL KEYSPACES TO udfv_mask_allum'),
        ('grant_allum_unmask', 'GRANT UNMASK ON ALL KEYSPACES TO udfv_mask_allum'),
        ('grant_all_t', 'GRANT ALL PERMISSIONS ON %s.t TO udfv_mask_all'),
        ('grant_all_k', 'GRANT ALL ON %s.k TO udfv_mask_all'),
        ('grant_parent_select', 'GRANT SELECT ON %s.t TO udfv_mask_parent'),
        ('grant_parent_unmask', 'GRANT UNMASK ON %s.t TO udfv_mask_parent'),
        ('grant_child', 'GRANT udfv_mask_parent TO udfv_mask_child'),
        ('grant_w_select', 'GRANT SELECT ON %s.t TO udfv_mask_w'),
        ('grant_w_modify', 'GRANT MODIFY ON %s.t TO udfv_mask_w'),
        ('grant_unmask_function', 'GRANT UNMASK ON ALL FUNCTIONS TO udfv_mask_r'),
        ('grant_unmask_role', 'GRANT UNMASK ON ROLE udfv_mask_r TO udfv_mask_sm'),
        ('grant_select_masked_mbean', 'GRANT SELECT_MASKED ON ALL MBEANS TO udfv_mask_r'),
        ('list_um', 'LIST ALL PERMISSIONS OF udfv_mask_um'),
        ('list_unmask_on_t', 'LIST UNMASK ON %s.t'),
        ('list_select_masked', 'LIST SELECT_MASKED OF udfv_mask_kssm'),
        ('perm_rows', "SELECT role, resource, permissions FROM system_auth.role_permissions WHERE role = 'udfv_mask_sm'"),
    ]:
        c(cid, q.replace('%s', K))
    c('sleep', '!sleep 2.5', '-')
    c('owner_create', 'CREATE TABLE %s.own (pk int PRIMARY KEY, v text MASKED WITH DEFAULT)' % K, 'owner')
    c('owner_insert', "INSERT INTO %s.own (pk, v) VALUES (1, 'mine')" % K, 'owner')
    c('owner_select', 'SELECT * FROM %s.own' % K, 'owner')
    c('owner_select_t', 'SELECT * FROM %s.t' % K, 'owner')
    c('owner_perms', 'LIST ALL PERMISSIONS ON %s.own OF udfv_mask_owner' % K)
    c('owner_where', "SELECT * FROM %s.own WHERE v = 'mine' ALLOW FILTERING" % K, 'owner')
    c('r_select_own', 'SELECT * FROM %s.own' % K, 'r')
    c('su_grant_r_own', 'GRANT SELECT ON %s.own TO udfv_mask_r' % K)
    c('sleep', '!sleep 2.5', '-')
    c('r_select_own_granted', 'SELECT * FROM %s.own' % K, 'r')
    c('insert_static_only', "INSERT INTO %s.t (pk, s) VALUES (4, 'lonely')" % K)
    c('static_only_r', 'SELECT pk, ck, s, v FROM %s.t WHERE pk = 4' % K, 'r')
    star = 'SELECT * FROM %s.t WHERE pk = 1' % K
    for who in ['su', 'r', 'sm', 'um', 'ksum', 'kssm', 'allum', 'all', 'child', 'w']:
        c('star_' + who, star, who)
        c('k_' + who, 'SELECT * FROM %s.k' % K, who)
    # Selectors over masked values.
    for cid, q in [
        ('sel_functions', "SELECT ck, blobAsText(textAsBlob(v)), toJson(v), CAST(n AS text), n + 1, writetime(v), ttl(v), writetime(s), maxwritetime(l), token(pk) FROM %s.t WHERE pk = 1"),
        ('sel_alias', 'SELECT v AS x, ck AS "C" FROM %s.t WHERE pk = 2'),
        ('sel_aggregates', 'SELECT count(v), max(v), min(n), sum(n), avg(n), count(*), max(ck), min(s) FROM %s.t'),
        ('sel_aggregates_pk', 'SELECT count(*), max(ck), sum(n) FROM %s.t WHERE pk = 1'),
        ('sel_json_star', 'SELECT JSON * FROM %s.t WHERE pk = 2'),
        ('sel_json_cols', 'SELECT JSON ck, v, plain FROM %s.t WHERE pk = 2'),
        ('sel_distinct', 'SELECT DISTINCT pk, s FROM %s.t'),
        ('sel_static_only', 'SELECT s FROM %s.t WHERE pk = 1'),
        ('sel_in_order', 'SELECT pk, ck, v FROM %s.t WHERE pk IN (2, 1) ORDER BY ck DESC'),
        ('sel_in_order_unselected', 'SELECT v FROM %s.t WHERE pk IN (2, 1) ORDER BY ck DESC'),
        ('sel_in_order_json', 'SELECT JSON v, ck FROM %s.t WHERE pk IN (2, 1) ORDER BY ck ASC'),
        ('sel_order_desc', 'SELECT ck, v FROM %s.t WHERE pk = 1 ORDER BY ck DESC'),
        ('sel_group_pk', 'SELECT pk, ck, count(*), max(v) FROM %s.t GROUP BY pk'),
        ('sel_group_ck', 'SELECT pk, ck, v FROM %s.t WHERE pk = 1 GROUP BY pk, ck'),
        ('sel_udt_field', 'SELECT u, u.street, u.zip FROM %s.t WHERE pk = 1'),
        ('sel_elements', "SELECT l, l[0], m, m['a'], l[0..1] FROM %s.t WHERE pk = 1"),
        ('sel_blob', 'SELECT b, mask_hash(b), blobAsInt(b) FROM %s.t WHERE pk = 3'),
        ('sel_mask_fn_on_masked', 'SELECT mask_inner(v, 0, 0), mask_default(n) FROM %s.t WHERE pk = 3'),
        ('sel_limit', 'SELECT ck, v FROM %s.t LIMIT 2'),
        ('sel_per_partition_limit', 'SELECT pk, ck FROM %s.t PER PARTITION LIMIT 1'),
        ('sel_k_token', 'SELECT p, token(p), q FROM %s.k'),
        ('sel_k_json', 'SELECT JSON p, v FROM %s.k'),
        ('page_t', '!page 2 SELECT pk, ck, v FROM %s.t'),
        ('page_t_partition', '!page 1 SELECT ck, v FROM %s.t WHERE pk = 1'),
        ('page_group', '!page 1 SELECT pk, max(v) FROM %s.t GROUP BY pk'),
        ('prepared', '!prepared SELECT ck, v FROM %s.t WHERE pk = 2'),
        ('plain_filter', "SELECT ck, v FROM %s.t WHERE plain = 'p10'"),
        ('plain_filter_af', "SELECT ck, v FROM %s.t WHERE pk = 1 AND plain = 'p20' ALLOW FILTERING"),
    ]:
        for who in ['r', 'um']:
            c('%s_%s' % (cid, who), q.replace('%s', K), who)
    c('prepared_su', '!prepared SELECT ck, v FROM %s.t WHERE pk = 2' % K)
    # Paging over statics and groups without masks (both dialects).
    c('setup_pg', 'CREATE TABLE %s.pg (pk int, ck int, s int STATIC, v text, PRIMARY KEY (pk, ck))' % K)
    for pk, ck in [(1, 1), (1, 2), (1, 3), (2, 1), (2, 2), (3, 1)]:
        c('insert_pg_%d_%d' % (pk, ck), "INSERT INTO %s.pg (pk, ck, s, v) VALUES (%d, %d, %d, 'v%d%d')" % (K, pk, ck, pk, pk, ck))
    c('insert_pg_static_only', 'INSERT INTO %s.pg (pk, s) VALUES (4, 4)' % K)
    for cid, q in [
        ('pg_all_2', '!page 2 SELECT * FROM %s.pg'),
        ('pg_all_3', '!page 3 SELECT pk, ck, s FROM %s.pg'),
        ('pg_partition_1', '!page 1 SELECT ck, v FROM %s.pg WHERE pk = 1'),
        ('pg_partition_3', '!page 3 SELECT ck, s FROM %s.pg WHERE pk = 1'),
        ('pg_group_1', '!page 1 SELECT pk, max(v) FROM %s.pg GROUP BY pk'),
        ('pg_group_2', '!page 2 SELECT pk, count(*) FROM %s.pg GROUP BY pk'),
        ('pg_group_4', '!page 4 SELECT pk, count(*) FROM %s.pg GROUP BY pk'),
        ('pg_group_5', '!page 5 SELECT pk, count(*) FROM %s.pg GROUP BY pk'),
        ('pg_group_ck_2', '!page 2 SELECT pk, ck, count(*) FROM %s.pg WHERE pk = 1 GROUP BY pk, ck'),
        ('pg_agg', '!page 1 SELECT count(*) FROM %s.pg'),
        # The masking functions and permissions on a table without masks (ScyllaDB has neither).
        ('pg_mask_functions', 'SELECT mask_inner(v, 1, 1), mask_default(ck), mask_null(v), mask_replace(ck, 0), mask_hash(v) FROM %s.pg WHERE pk = 1'),
        ('pg_mask_function_qualified', 'SELECT system.mask_outer(v, 1, 1) FROM %s.pg WHERE pk = 1'),
        ('pg_grant_unmask', 'GRANT UNMASK ON %s.pg TO udfv_mask_r'),
        ('pg_grant_select_masked', 'GRANT SELECT_MASKED ON %s.pg TO udfv_mask_r'),
        ('pg_grant_unmask_ks', 'GRANT UNMASK ON KEYSPACE %s TO udfv_mask_r'),
        ('pg_revoke_unmask', 'REVOKE UNMASK ON %s.pg FROM udfv_mask_r'),
        ('pg_list_unmask', 'LIST UNMASK OF udfv_mask_r'),
        ('pg_revoke_unmask_ks', 'REVOKE UNMASK ON KEYSPACE %s FROM udfv_mask_r'),
        ('pg_revoke_select_masked', 'REVOKE SELECT_MASKED ON %s.pg FROM udfv_mask_r'),
        ('pg_alter_masked', 'ALTER TABLE %s.pg ALTER v MASKED WITH DEFAULT'),
        ('pg_alter_drop_masked', 'ALTER TABLE %s.pg ALTER v DROP MASKED'),
        ('pg_add_masked', 'ALTER TABLE %s.pg ADD w text MASKED WITH DEFAULT'),
    ]:
        c(cid, q.replace('%s', K))
    # Restrictions on masked columns.
    for cid, q in [
        ('where_ck_eq', 'SELECT * FROM %s.t WHERE pk = 1 AND ck = 1'),
        ('where_ck_slice', 'SELECT * FROM %s.t WHERE pk = 1 AND ck > 1'),
        ('where_ck_in', 'SELECT * FROM %s.t WHERE pk = 1 AND ck IN (1, 2)'),
        ('where_ck_multi', 'SELECT * FROM %s.t WHERE pk = 1 AND (ck) = (1)'),
        ('where_ck_multi_slice', 'SELECT * FROM %s.t WHERE pk = 1 AND (ck) >= (2)'),
        ('where_v_af', "SELECT * FROM %s.t WHERE v = 'carol' ALLOW FILTERING"),
        ('where_v_no_af', "SELECT * FROM %s.t WHERE v = 'carol'"),
        ('where_pk_v_af', "SELECT pk, ck FROM %s.t WHERE pk = 1 AND v = 'carol' ALLOW FILTERING"),
        ('where_s_af', "SELECT pk, ck FROM %s.t WHERE s = 'static1' ALLOW FILTERING"),
        ('where_l_contains', 'SELECT pk, ck FROM %s.t WHERE l CONTAINS 10 ALLOW FILTERING'),
        ('where_m_contains_key', "SELECT pk, ck FROM %s.t WHERE m CONTAINS KEY 'a' ALLOW FILTERING"),
        ('where_m_entry', "SELECT pk, ck FROM %s.t WHERE m['a'] = 10 ALLOW FILTERING"),
        ('where_n_index', 'SELECT pk, ck, n FROM %s.t WHERE n = 20'),
        ('where_many', "SELECT * FROM %s.t WHERE pk = 1 AND ck = 1 AND v = 'x' AND n = 10 AND s = 'y' ALLOW FILTERING"),
        ('where_many_reordered', "SELECT * FROM %s.t WHERE n = 10 AND v = 'x' AND pk = 1 AND ck = 1 ALLOW FILTERING"),
        ('where_unknown', 'SELECT * FROM %s.t WHERE nope = 1 ALLOW FILTERING'),
        ('where_k_pk', 'SELECT * FROM %s.k WHERE p = 1'),
        ('where_k_pk_in', 'SELECT p, v FROM %s.k WHERE p IN (3, 1, 2)'),
        ('where_k_token', 'SELECT p, q FROM %s.k WHERE token(p) > 0'),
        ('where_k_q_af', 'SELECT p FROM %s.k WHERE q = 20 ALLOW FILTERING'),
        ('where_k_v_af', "SELECT p FROM %s.k WHERE v = 'two' ALLOW FILTERING"),
        ('where_k_p_q', 'SELECT p FROM %s.k WHERE p = 2 AND q = 20 ALLOW FILTERING'),
        ('prepared_where', '!prepared SELECT * FROM %s.k WHERE p = 1'),
    ]:
        for who in ['r', 'sm', 'um', 'kssm', 'su']:
            c('%s_%s' % (cid, who), q.replace('%s', K), who)
    c('no_select', 'SELECT * FROM %s.t' % K, 'mv')
    c('no_select_where', 'SELECT * FROM %s.t WHERE pk = 1 AND ck = 1' % K, 'mv')
    # Lightweight transactions show existing values.
    for who in ['w', 'su']:
        for cid, q in [
            ('lwt_insert_exists', "INSERT INTO %s.t (pk, ck, v) VALUES (1, 1, 'z') IF NOT EXISTS"),
            ('lwt_update_cond', "UPDATE %s.t SET plain = 'z' WHERE pk = 1 AND ck = 2 IF v = 'nope'"),
            ('lwt_update_cond_two', "UPDATE %s.t SET plain = 'z' WHERE pk = 1 AND ck = 2 IF n = 1 AND plain = 'p20'"),
            ('lwt_update_static_cond', "UPDATE %s.t SET s = 'z' WHERE pk = 1 IF s = 'nope'"),
            ('lwt_update_collection_cond', "UPDATE %s.t SET plain = 'z' WHERE pk = 1 AND ck = 2 IF l = [1] AND m['a'] = 1"),
            ('lwt_delete_cond', "DELETE FROM %s.t WHERE pk = 1 AND ck = 3 IF v = 'nope'"),
            ('lwt_delete_in_cond', "DELETE FROM %s.t WHERE pk = 1 AND ck = 3 IF v IN ('nope', 'nah')"),
            ('lwt_update_exists_ok', "UPDATE %s.t SET plain = 'p30' WHERE pk = 1 AND ck = 3 IF EXISTS"),
            ('lwt_update_exists_missing', "UPDATE %s.t SET plain = 'z' WHERE pk = 9 AND ck = 9 IF EXISTS"),
            ('lwt_update_masked_cond_true', "UPDATE %s.t SET plain = 'p20' WHERE pk = 1 AND ck = 2 IF v = 'bob@example.com'"),
            ('lwt_batch', "BEGIN BATCH UPDATE %s.t SET plain = 'z' WHERE pk = 1 AND ck = 1 IF v = 'nope'; UPDATE %s.t SET plain = 'z' WHERE pk = 1 AND ck = 2 IF n = 0; APPLY BATCH"),
            ('lwt_batch_not_exists', "BEGIN BATCH INSERT INTO %s.t (pk, ck, v) VALUES (2, 1, 'z') IF NOT EXISTS; INSERT INTO %s.t (pk, ck, v) VALUES (2, 9, 'z') IF NOT EXISTS; APPLY BATCH"),
            ('lwt_k_insert_exists', "INSERT INTO %s.k (p, q, v) VALUES (1, 1, 'z') IF NOT EXISTS"),
            ('lwt_k_update_cond', "UPDATE %s.k SET q = 1 WHERE p = 2 IF v = 'nope'"),
        ]:
            c('%s_%s' % (cid, who), q.replace('%s', K), who)
    c('grant_w_k', 'GRANT SELECT ON %s.k TO udfv_mask_w' % K)
    c('grant_w_k_modify', 'GRANT MODIFY ON %s.k TO udfv_mask_w' % K)
    c('sleep', '!sleep 2.5', '-')
    c('lwt_k_insert_exists_w2', "INSERT INTO %s.k (p, q, v) VALUES (1, 1, 'z') IF NOT EXISTS" % K, 'w')
    c('lwt_k_update_cond_w2', "UPDATE %s.k SET q = 1 WHERE p = 2 IF v = 'nope'" % K, 'w')
    c('after_lwt', 'SELECT pk, ck, plain FROM %s.t' % K)
    # Revocation.
    c('revoke_um_unmask', 'REVOKE UNMASK ON %s.t FROM udfv_mask_um' % K)
    c('revoke_sm_masked', 'REVOKE SELECT_MASKED ON %s.t FROM udfv_mask_sm' % K)
    c('sleep', '!sleep 2.5', '-')
    c('after_revoke_um', 'SELECT ck, v FROM %s.t WHERE pk = 2' % K, 'um')
    c('after_revoke_um_where', 'SELECT ck, v FROM %s.t WHERE pk = 2 AND ck = 1' % K, 'um')
    c('after_revoke_sm_where', 'SELECT ck, v FROM %s.t WHERE pk = 2 AND ck = 1' % K, 'sm')
    # Dropping a mask shows the data to everyone.
    c('drop_mask_v', 'ALTER TABLE %s.t ALTER v DROP MASKED' % K)
    c('after_drop_mask', 'SELECT ck, v FROM %s.t WHERE pk = 2' % K, 'r')
    c('after_drop_mask_where', "SELECT ck FROM %s.t WHERE v = 'eve' ALLOW FILTERING" % K, 'r')
    c('remask_v', 'ALTER TABLE %s.t ALTER v MASKED WITH mask_replace(\'***\')' % K)
    c('after_remask', 'SELECT ck, v FROM %s.t WHERE pk = 2' % K, 'r')
    c('drop_masks_all', 'ALTER TABLE %s.t ALTER ck DROP MASKED' % K)
    c('after_drop_ck_mask', 'SELECT ck, v FROM %s.t WHERE pk = 2 AND ck = 1' % K, 'r')
    return CLEAN, C


def views():
    C = []
    def c(cid, q, who='su'): C.append((cid, who, q))
    c('setup_ks', KS % K)
    c('setup_base', 'CREATE TABLE %s.b (pk int, ck int, v text MASKED WITH mask_inner(1, 1), w int MASKED WITH DEFAULT, x text, PRIMARY KEY (pk, ck))' % K)
    c('setup_mv', 'CREATE MATERIALIZED VIEW %s.bv AS SELECT * FROM %s.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND w IS NOT NULL PRIMARY KEY (w, pk, ck)' % (K, K))
    c('setup_mv2', 'CREATE MATERIALIZED VIEW %s.bv2 AS SELECT pk, ck, x FROM %s.b WHERE pk IS NOT NULL AND ck IS NOT NULL AND x IS NOT NULL PRIMARY KEY (x, pk, ck)' % (K, K))
    for pk, ck, v, w in [(1, 1, 'alpha', 10), (1, 2, 'beta', 20), (2, 1, 'gamma', 30)]:
        c('insert_%d_%d' % (pk, ck), "INSERT INTO %s.b (pk, ck, v, w, x) VALUES (%d, %d, '%s', %d, 'x%d')" % (K, pk, ck, v, w, w))
    for r in ['udfv_mask_r', 'udfv_mask_um', 'udfv_mask_ksum', 'udfv_mask_mv', 'udfv_mask_sm']:
        c('role_' + r, "CREATE ROLE %s WITH PASSWORD = 'pw' AND LOGIN = true" % r)
    for cid, q in [
        ('grant_r', 'GRANT SELECT ON %s.b TO udfv_mask_r'),
        ('grant_um', 'GRANT SELECT ON %s.b TO udfv_mask_um'),
        ('grant_um_unmask', 'GRANT UNMASK ON %s.b TO udfv_mask_um'),
        ('grant_ksum', 'GRANT SELECT ON KEYSPACE %s TO udfv_mask_ksum'),
        ('grant_ksum_unmask', 'GRANT UNMASK ON KEYSPACE %s TO udfv_mask_ksum'),
        ('grant_sm', 'GRANT SELECT ON %s.b TO udfv_mask_sm'),
        ('grant_sm_masked', 'GRANT SELECT_MASKED ON %s.b TO udfv_mask_sm'),
        ('grant_mv_view', 'GRANT SELECT ON %s.bv TO udfv_mask_mv'),
        ('grant_mv_view_unmask', 'GRANT UNMASK ON %s.bv TO udfv_mask_mv'),
        ('grant_mv_view_masked', 'GRANT SELECT_MASKED ON %s.bv TO udfv_mask_mv'),
    ]:
        c(cid, q.replace('%s', K))
    c('sleep', '!sleep 2.5', '-')
    for who in ['su', 'r', 'um', 'ksum', 'sm', 'mv']:
        c('view_star_' + who, 'SELECT * FROM %s.bv' % K, who)
        c('view2_star_' + who, 'SELECT * FROM %s.bv2' % K, who)
        c('view_where_pk_' + who, 'SELECT pk, ck, v FROM %s.bv WHERE w = 20' % K, who)
        c('view_where_v_' + who, "SELECT pk FROM %s.bv WHERE v = 'beta' ALLOW FILTERING" % K, who)
        c('view2_where_' + who, "SELECT pk, ck FROM %s.bv2 WHERE x = 'x20'" % K, who)
        c('base_star_' + who, 'SELECT * FROM %s.b' % K, who)
    c('grant_mv_base', 'GRANT SELECT ON %s.b TO udfv_mask_mv' % K)
    c('sleep', '!sleep 2.5', '-')
    c('view_star_mv_unmask', 'SELECT * FROM %s.bv' % K, 'mv')
    c('view_where_mv_unmask', 'SELECT pk, ck, v FROM %s.bv WHERE w = 20' % K, 'mv')
    c('view2_star_mv_unmask', 'SELECT * FROM %s.bv2' % K, 'mv')
    c('base_star_mv_unmask', 'SELECT * FROM %s.b' % K, 'mv')
    c('base_where_mv', 'SELECT * FROM %s.b WHERE pk = 1 AND w = 10 ALLOW FILTERING' % K, 'mv')
    c('list_mv', 'LIST ALL PERMISSIONS OF udfv_mask_mv')
    c('alter_base_mask_x', "ALTER TABLE %s.b ALTER x MASKED WITH mask_replace('?')" % K)
    c('view2_after_alter', 'SELECT * FROM %s.bv2' % K, 'r')
    c('view2_where_after_alter', "SELECT pk, ck FROM %s.bv2 WHERE x = 'x20'" % K, 'r')
    c('alter_base_add_masked', 'ALTER TABLE %s.b ADD y int MASKED WITH mask_replace(-1)' % K)
    c('insert_y', 'UPDATE %s.b SET y = 5 WHERE pk = 1 AND ck = 1' % K)
    c('view_after_add', 'SELECT pk, ck, y FROM %s.bv WHERE w = 10' % K, 'r')
    c('masks_views', "SELECT table_name, column_name, function_name FROM system_schema.column_masks WHERE keyspace_name = '%s'" % K)
    c('alter_base_drop_v', 'ALTER TABLE %s.b ALTER v DROP MASKED' % K)
    c('view_after_drop_mask', 'SELECT pk, ck, v FROM %s.bv' % K, 'r')
    c('lwt_on_base', "UPDATE %s.b SET x = 'x10' WHERE pk = 1 AND ck = 1 IF w = 0" % K)
    c('drop_view_perms', 'DROP MATERIALIZED VIEW %s.bv' % K)
    c('list_mv_after_drop', 'LIST ALL PERMISSIONS OF udfv_mask_mv')
    c('masks_after_view_drop', "SELECT table_name, column_name, function_name FROM system_schema.column_masks WHERE keyspace_name = '%s'" % K)
    return CLEAN, C


BATTERIES = [('ddl', *ddl()), ('reads', *reads()), ('views', *views())]
