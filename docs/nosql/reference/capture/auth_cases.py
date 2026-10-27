# Auth cases: (id, who, statement) run in order after CLEAN; who is a session in auth_live.CREDS,
# "-" for logins (`!login user password`, `!token <hex SASL token>`); `!sleep s` waits out
# Cassandra's 2 s auth caches; `!prepare` sends PREPARE. Roles and keyspaces use the auth_ prefix.


def battery1():
    # Auth probe cases: (id, who, cql). who: 'su' (cassandra/cassandra), 'alice' (auth_alice/alice), 'bob', '-' (no session; logins).
    KS = "CREATE KEYSPACE auth_ks WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"
    ROLES = ['auth_alice', 'auth_bob', 'auth_nologin', 'auth_nopw', 'auth_r1', 'auth_r2', 'auth_r3', 'auth_x', 'auth_legacy', 'auth_legacy2', 'auth_hashed', 'auth_hashed2', 'auth_dc', 'auth_dc2', 'auth_cidr', 'auth_opts', 'Auth_Quoted', 'auth_s', 'auth_super', 'auth_sl_role', 'auth_ns']
    CLEAN = [('clean', 'su', 'DROP ROLE IF EXISTS "%s"' % r) for r in ROLES] + [
        ('clean', 'su', 'DROP KEYSPACE IF EXISTS auth_ks'), ('clean', 'su', 'DROP KEYSPACE IF EXISTS auth_ks2'), ('clean', 'su', 'DROP KEYSPACE IF EXISTS auth_ks3'),
        ('clean', 'su', 'DROP SERVICE LEVEL IF EXISTS auth_sl1'), ('clean', 'su', 'DROP SERVICE LEVEL IF EXISTS auth_sl2'),
        ('clean', 'su', "DROP IDENTITY IF EXISTS 'auth_id1'"), ('clean', 'su', "DROP IDENTITY IF EXISTS 'auth_id2'")]
    C = []
    def c(cid, who, q): C.append((cid, who, q))
    c('setup_ks', 'su', KS)
    c('setup_t', 'su', 'CREATE TABLE auth_ks.t (pk int PRIMARY KEY, v int)')
    c('setup_t2', 'su', 'CREATE TABLE auth_ks.t2 (pk int PRIMARY KEY, v int)')
    c('setup_alice', 'su', "CREATE ROLE auth_alice WITH PASSWORD = 'alice' AND LOGIN = true")
    c('setup_bob', 'su', "CREATE ROLE auth_bob WITH PASSWORD = 'bob' AND LOGIN = true")
    c('setup_nologin', 'su', "CREATE ROLE auth_nologin WITH PASSWORD = 'x' AND LOGIN = false")
    c('setup_nopw', 'su', "CREATE ROLE auth_nopw WITH LOGIN = true")
    # logins
    c('login_wrong_password', '-', '!login auth_alice wrong')
    c('login_unknown_user', '-', '!login auth_nobody x')
    c('login_not_permitted', '-', '!login auth_nologin x')
    c('login_not_permitted_wrong_pw', '-', '!login auth_nologin y')
    c('login_no_password_role', '-', '!login auth_nopw x')
    c('login_case', '-', '!login AUTH_ALICE alice')
    c('login_ok', '-', '!login auth_alice alice')
    c('token_empty_user_pw', '-', '!token 0000')
    c('token_empty_pw', '-', '!token 00' + 'auth_alice'.encode().hex() + '00')
    c('token_empty_user', '-', '!token 0000' + b'alice'.hex())
    c('token_no_nul', '-', '!token 616263')
    c('token_one_nul', '-', '!token 61620063')
    c('token_three_nul', '-', '!token 000000')
    c('token_authzid', '-', '!token ' + (b'zz\0auth_alice\0alice').hex())
    c('token_extra_field', '-', '!token ' + (b'a\0b\0auth_alice\0alice').hex())
    c('token_empty', '-', '!token ')
    # as alice, nothing granted
    for cid, q in [('select_no_permission', "SELECT * FROM auth_ks.t"), ('select_unknown_ks', "SELECT * FROM auth_nope.t"),
                   ('select_unknown_table', "SELECT * FROM auth_ks.nope"),
                   ('insert_no_permission', "INSERT INTO auth_ks.t (pk, v) VALUES (1, 1)"),
                   ('update_no_permission', "UPDATE auth_ks.t SET v = 1 WHERE pk = 1"),
                   ('delete_no_permission', "DELETE FROM auth_ks.t WHERE pk = 1"),
                   ('batch_no_permission', "BEGIN BATCH INSERT INTO auth_ks.t (pk, v) VALUES (1, 1); INSERT INTO auth_ks.t2 (pk, v) VALUES (1, 1); APPLY BATCH"),
                   ('create_ks_no_permission', KS.replace('auth_ks', 'auth_ks2')),
                   ('alter_ks_no_permission', "ALTER KEYSPACE auth_ks WITH durable_writes = true"),
                   ('drop_ks_no_permission', "DROP KEYSPACE auth_ks"),
                   ('drop_ks_unknown_no_permission', "DROP KEYSPACE auth_nope"),
                   ('create_table_no_permission', "CREATE TABLE auth_ks.t3 (pk int PRIMARY KEY)"),
                   ('create_table_unknown_ks', "CREATE TABLE auth_nope.t3 (pk int PRIMARY KEY)"),
                   ('drop_table_no_permission', "DROP TABLE auth_ks.t"),
                   ('drop_table_unknown', "DROP TABLE IF EXISTS auth_ks.nope"),
                   ('alter_table_no_permission', "ALTER TABLE auth_ks.t ADD x int"),
                   ('truncate_no_permission', "TRUNCATE auth_ks.t"),
                   ('create_type_no_permission', "CREATE TYPE auth_ks.ty (a int)"),
                   ('alter_type_no_permission', "ALTER TYPE auth_ks.ty ADD b int"),
                   ('drop_type_no_permission', "DROP TYPE auth_ks.ty"),
                   ('create_index_no_permission', "CREATE INDEX ON auth_ks.t (v)"),
                   ('drop_index_no_permission', "DROP INDEX auth_ks.nope"),
                   ('create_mv_no_permission', "CREATE MATERIALIZED VIEW auth_ks.mv AS SELECT * FROM auth_ks.t WHERE v IS NOT NULL AND pk IS NOT NULL PRIMARY KEY (v, pk)"),
                   ('create_function_no_permission', "CREATE FUNCTION auth_ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'"),
                   ('drop_function_no_permission', "DROP FUNCTION auth_ks.nope"),
                   ('create_trigger_no_permission', "CREATE TRIGGER tr ON auth_ks.t USING 'x'"),
                   ('grant_no_permission', "GRANT SELECT ON ALL KEYSPACES TO auth_alice"),
                   ('grant_table_no_permission', "GRANT SELECT ON auth_ks.t TO auth_bob"),
                   ('revoke_no_permission', "REVOKE SELECT ON auth_ks.t FROM auth_bob"),
                   ('create_role_no_permission', "CREATE ROLE auth_x"),
                   ('create_role_super_no_permission', "CREATE ROLE auth_x WITH SUPERUSER = true"),
                   ('drop_role_no_permission', "DROP ROLE auth_bob"),
                   ('drop_role_unknown_no_permission', "DROP ROLE auth_nope"),
                   ('drop_role_self', "DROP ROLE auth_alice"),
                   ('alter_other_role', "ALTER ROLE auth_bob WITH PASSWORD = 'x'"),
                   ('alter_own_superuser', "ALTER ROLE auth_alice WITH SUPERUSER = true"),
                   ('alter_own_login', "ALTER ROLE auth_alice WITH LOGIN = true"),
                   ('alter_own_options', "ALTER ROLE auth_alice WITH OPTIONS = {'a': 'b'}"),
                   ('alter_own_password', "ALTER ROLE auth_alice WITH PASSWORD = 'alice'"),
                   ('alter_unknown_role_nonsuper', "ALTER ROLE auth_nope WITH PASSWORD = 'x'"),
                   ('grant_role_no_permission', "GRANT auth_bob TO auth_alice"),
                   ('revoke_role_no_permission', "REVOKE auth_bob FROM auth_alice"),
                   ('list_roles_nonsuper', "LIST ROLES"),
                   ('list_roles_of_self', "LIST ROLES OF auth_alice"),
                   ('list_roles_of_other', "LIST ROLES OF auth_bob"),
                   ('list_roles_of_unknown', "LIST ROLES OF auth_nope"),
                   ('list_users_nonsuper', "LIST USERS"),
                   ('list_all_permissions_nonsuper', "LIST ALL PERMISSIONS"),
                   ('list_own_permissions', "LIST ALL PERMISSIONS OF auth_alice"),
                   ('list_other_permissions', "LIST ALL PERMISSIONS OF auth_bob"),
                   ('list_permissions_on_table_nonsuper', "LIST ALL PERMISSIONS ON auth_ks.t"),
                   ('read_system_auth_roles', "SELECT role FROM system_auth.roles"),
                   ('read_system_roles', "SELECT role FROM system.roles"),
                   ('read_system_local', "SELECT cluster_name FROM system.local"),
                   ('read_system_peers', "SELECT peer FROM system.peers"),
                   ('read_system_size_estimates', "SELECT * FROM system.size_estimates LIMIT 1"),
                   ('read_system_schema', "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'auth_ks'"),
                   ('read_system_traces', "SELECT * FROM system_traces.sessions LIMIT 1"),
                   ('read_system_views', "SELECT * FROM system_views.settings LIMIT 1"),
                   ('read_virtual_schema', "SELECT * FROM system_virtual_schema.keyspaces LIMIT 1"),
                   ('read_system_scylla_local', "SELECT * FROM system.scylla_local LIMIT 1"),
                   ('read_system_versions', "SELECT * FROM system.versions"),
                   ('read_service_levels_table', "SELECT * FROM system.service_levels_v2"),
                   ('use_keyspace_nonsuper', "USE auth_ks"),
                   ('use_unknown_keyspace', "USE auth_nope"),
                   ('create_sl_nonsuper', "CREATE SERVICE LEVEL auth_sl1"),
                   ('list_sl_nonsuper', "LIST ALL SERVICE LEVELS"),
                   ('list_one_sl_nonsuper', "LIST SERVICE LEVEL driver"),
                   ('list_attached_nonsuper', "LIST ATTACHED SERVICE LEVEL OF auth_alice"),
                   ('list_all_attached_nonsuper', "LIST ALL ATTACHED SERVICE LEVELS"),
                   ('list_effective_nonsuper', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice"),
                   ('attach_sl_nonsuper', "ATTACH SERVICE LEVEL driver TO auth_alice"),
                   ('detach_sl_nonsuper', "DETACH SERVICE LEVEL FROM auth_alice"),
                   ('add_identity_nonsuper', "ADD IDENTITY 'auth_id1' TO ROLE 'auth_alice'"),
                   ('drop_identity_nonsuper', "DROP IDENTITY 'auth_id1'"),
                   ('create_user_nonsuper', "CREATE USER auth_x WITH PASSWORD 'x'"),
                   ('alter_user_own_pw', "ALTER USER auth_alice WITH PASSWORD 'alice'"),
                   ('alter_user_own_super', "ALTER USER auth_alice SUPERUSER"),
                   ('drop_user_nonsuper', "DROP USER auth_bob"),
                   ('describe_schema_nonsuper', "DESCRIBE KEYSPACE auth_ks"),
                   ]:
        c(cid, 'alice', q)
    # grants
    c('grant_select', 'su', "GRANT SELECT ON auth_ks.t TO auth_alice")
    c('grant_select_again', 'su', "GRANT SELECT ON auth_ks.t TO auth_alice")
    c('grant_modify_keyspace', 'su', "GRANT MODIFY ON KEYSPACE auth_ks TO auth_alice")
    c('grant_all_on_t2', 'su', "GRANT ALL ON auth_ks.t2 TO auth_bob")
    c('grant_all_again', 'su', "GRANT ALL PERMISSIONS ON auth_ks.t2 TO auth_bob")
    c('grant_select_alter_t2', 'su', "GRANT SELECT ON TABLE auth_ks.t2 TO auth_bob")
    c('perm_rows_alice', 'su', "SELECT role, resource, permissions FROM system_auth.role_permissions WHERE role = 'auth_alice'")
    c('perm_rows_alice_scylla', 'su', "SELECT role, resource, permissions FROM system.role_permissions WHERE role = 'auth_alice'")
    c('perm_index', 'su', "SELECT * FROM system_auth.resource_role_permissons_index WHERE resource = 'data/auth_ks/t2'")
    c('sleep', '-', '!sleep 2.5')
    for cid, q in [('select_after_grant', "SELECT * FROM auth_ks.t"), ('insert_after_grant', "INSERT INTO auth_ks.t (pk, v) VALUES (1, 1)"),
                   ('select_t2_without', "SELECT * FROM auth_ks.t2"), ('insert_t2_keyspace_modify', "INSERT INTO auth_ks.t2 (pk, v) VALUES (1, 1)"),
                   ('batch_after_grant', "BEGIN BATCH INSERT INTO auth_ks.t (pk, v) VALUES (2, 1); INSERT INTO auth_ks.t2 (pk, v) VALUES (2, 1); APPLY BATCH"),
                   ('lwt_after_grant', "UPDATE auth_ks.t SET v = 2 WHERE pk = 1 IF v = 1"),
                   ('lwt_t2_modify_only', "UPDATE auth_ks.t2 SET v = 2 WHERE pk = 1 IF v = 1"),
                   ('lwt_insert_t2_modify_only', "INSERT INTO auth_ks.t2 (pk, v) VALUES (5, 5) IF NOT EXISTS"),
                   ('truncate_after_grant', "TRUNCATE auth_ks.t2"),
                   ('alter_after_grant_select_only', "ALTER TABLE auth_ks.t ADD y int"),
                   ('list_own_after_grant', "LIST ALL PERMISSIONS OF auth_alice"),
                   ('list_select_of_self', "LIST SELECT OF auth_alice"),
                   ('list_own_on_t', "LIST ALL PERMISSIONS ON auth_ks.t OF auth_alice"),
                   ('list_own_on_t_norec', "LIST ALL PERMISSIONS ON auth_ks.t OF auth_alice NORECURSIVE"),
                   ('list_own_on_ks', "LIST ALL PERMISSIONS ON KEYSPACE auth_ks OF auth_alice"),
                   ('list_own_modify', "LIST MODIFY OF auth_alice"),
                   ('grant_with_select_only', "GRANT SELECT ON auth_ks.t TO auth_bob")]:
        c(cid, 'alice', q)
    c('grant_authorize', 'su', "GRANT AUTHORIZE ON auth_ks.t TO auth_alice")
    c('sleep', '-', '!sleep 2.5')
    c('grant_with_authorize_select', 'alice', "GRANT SELECT ON auth_ks.t TO auth_bob")
    c('grant_with_authorize_modify', 'alice', "GRANT MODIFY ON auth_ks.t TO auth_bob")
    c('grant_with_authorize_alter', 'alice', "GRANT ALTER ON auth_ks.t TO auth_bob")
    c('revoke_with_authorize', 'alice', "REVOKE SELECT ON auth_ks.t FROM auth_bob")
    c('grant_with_authorize_unknown_role', 'alice', "GRANT SELECT ON auth_ks.t TO auth_nope")
    # role inheritance
    c('create_r1', 'su', "CREATE ROLE auth_r1")
    c('grant_r1_select_t2', 'su', "GRANT SELECT ON auth_ks.t2 TO auth_r1")
    c('grant_r1_to_alice', 'su', "GRANT auth_r1 TO auth_alice")
    c('create_r2', 'su', "CREATE ROLE auth_r2 WITH SUPERUSER = true")
    c('member_rows', 'su', "SELECT * FROM system_auth.role_members WHERE role = 'auth_r1'")
    c('member_rows_scylla', 'su', "SELECT * FROM system.role_members WHERE role = 'auth_r1'")
    c('role_rows', 'su', "SELECT role, can_login, is_superuser, member_of FROM system_auth.roles WHERE role IN ('auth_alice', 'auth_r1', 'auth_r2', 'auth_nopw')")
    c('role_rows_scylla', 'su', "SELECT role, can_login, is_superuser, member_of FROM system.roles WHERE role IN ('auth_alice', 'auth_r1', 'auth_r2', 'auth_nopw')")
    c('sleep', '-', '!sleep 2.5')
    c('select_t2_inherited', 'alice', "SELECT * FROM auth_ks.t2")
    c('list_roles_inherited', 'alice', "LIST ROLES")
    c('list_roles_of_r1_inherited', 'alice', "LIST ROLES OF auth_r1")
    c('list_perms_of_r1_inherited', 'alice', "LIST ALL PERMISSIONS OF auth_r1")
    c('list_own_inherited', 'alice', "LIST ALL PERMISSIONS OF auth_alice")
    c('list_own_inherited_norec', 'alice', "LIST ALL PERMISSIONS OF auth_alice NORECURSIVE")
    c('grant_r2_to_r1', 'su', "GRANT auth_r2 TO auth_r1")
    c('sleep', '-', '!sleep 2.5')
    c('superuser_inherited_select', 'alice', "SELECT * FROM auth_ks.t2")
    c('superuser_inherited_create_role', 'alice', "CREATE ROLE auth_x")
    c('superuser_inherited_drop_role', 'alice', "DROP ROLE auth_x")
    c('alter_inherited_superuser', 'alice', "ALTER ROLE auth_r2 WITH SUPERUSER = false")
    c('list_roles_super_inherited', 'alice', "LIST ROLES OF auth_alice")
    c('revoke_r2_from_r1', 'su', "REVOKE auth_r2 FROM auth_r1")
    c('sleep', '-', '!sleep 2.5')
    c('after_revoke_superuser', 'alice', "CREATE ROLE auth_x")
    # role management as su
    for cid, q in [('create_role_exists', "CREATE ROLE auth_alice"), ('create_role_if_not_exists', "CREATE ROLE IF NOT EXISTS auth_alice"),
                   ('create_role_empty_name', "CREATE ROLE ''"),
                   ('create_role_quoted', 'CREATE ROLE "Auth_Quoted"'),
                   ('create_role_quoted_lookup', "CREATE ROLE 'Auth_Quoted'"),
                   ('drop_role_unknown', "DROP ROLE auth_nope"), ('drop_role_if_exists_unknown', "DROP ROLE IF EXISTS auth_nope"),
                   ('drop_own_role', "DROP ROLE cassandra"),
                   ('alter_unknown_role', "ALTER ROLE auth_nope WITH PASSWORD = 'x'"),
                   ('alter_if_exists_unknown', "ALTER ROLE IF EXISTS auth_nope WITH PASSWORD = 'x'"),
                   ('alter_role_login', "ALTER ROLE auth_r1 WITH LOGIN = true AND SUPERUSER = false"),
                   ('alter_role_no_options', "ALTER ROLE auth_r1"),
                   ('alter_own_superuser_su', "ALTER ROLE cassandra WITH SUPERUSER = true"),
                   ('role_bad_option', "CREATE ROLE auth_x WITH NOPE = 1"),
                   ('role_options_map', "CREATE ROLE auth_opts WITH OPTIONS = {'a': 'b'}"),
                   ('role_dup_option', "CREATE ROLE auth_x WITH LOGIN = true AND LOGIN = false"),
                   ('role_hashed_password', "CREATE ROLE auth_hashed WITH HASHED PASSWORD = 'notahash' AND LOGIN = true"),
                   ('role_hashed_bcrypt', "CREATE ROLE auth_hashed2 WITH HASHED PASSWORD = '$2a$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS' AND LOGIN = true"),
                   ('role_hashed_sha512', "ALTER ROLE auth_hashed WITH HASHED PASSWORD = '$6$rollsalt$SB70vlMcKgHm/b626DEcCWr/ck3vuP7A07ziftzQEWGhAZFTyTrq6t4ZG8EcoCms9ps65p8W2Ei7VvEws6EjU1'"),
                   ('role_password_and_hashed', "CREATE ROLE auth_x WITH PASSWORD = 'x' AND HASHED PASSWORD = 'y'"),
                   ('role_generated_password', "CREATE ROLE auth_x WITH GENERATED PASSWORD"),
                   ('role_access_to_datacenters', "CREATE ROLE auth_dc WITH ACCESS TO DATACENTERS {'datacenter1'} AND LOGIN = true AND PASSWORD = 'dc'"),
                   ('role_access_unknown_dc', "CREATE ROLE auth_x WITH ACCESS TO DATACENTERS {'nope'}"),
                   ('role_access_all_dcs', "CREATE ROLE auth_dc2 WITH ACCESS TO ALL DATACENTERS"),
                   ('alter_access_dcs', "ALTER ROLE auth_dc2 WITH ACCESS TO DATACENTERS {'datacenter1'}"),
                   ('role_access_to_cidrs', "CREATE ROLE auth_x WITH ACCESS FROM CIDRS {'region1'}"),
                   ('role_access_all_cidrs', "CREATE ROLE auth_cidr WITH ACCESS FROM ALL CIDRS"),
                   ('alter_access_cidrs', "ALTER ROLE auth_cidr WITH ACCESS FROM CIDRS {'region1'}"),
                   ('network_rows', "SELECT * FROM system_auth.network_permissions WHERE role IN ('roles/auth_dc', 'roles/auth_dc2', 'roles/auth_alice')"),
                   ('cidr_rows', "SELECT * FROM system_auth.cidr_permissions WHERE role IN ('auth_cidr', 'auth_alice', 'auth_dc')"),
                   ('grant_role_to_self', "GRANT auth_alice TO auth_alice"),
                   ('grant_role_cycle', "GRANT auth_alice TO auth_r1"),
                   ('grant_role_twice', "GRANT auth_r1 TO auth_alice"),
                   ('grant_unknown_role', "GRANT auth_nope TO auth_alice"),
                   ('grant_role_to_unknown', "GRANT auth_r1 TO auth_nope"),
                   ('revoke_role_not_granted', "REVOKE auth_r1 FROM auth_nologin"),
                   ('revoke_unknown_role', "REVOKE auth_nope FROM auth_alice"),
                   ('create_r3', "CREATE ROLE auth_r3"),
                   ('grant_r3_r1', "GRANT auth_r3 TO auth_r1"),
                   ('grant_cycle_deep', "GRANT auth_alice TO auth_r3"),
                   ('grant_inherited_twice', "GRANT auth_r3 TO auth_alice"),
                   ('list_roles_super', "LIST ROLES"), ('list_roles_of', "LIST ROLES OF auth_alice"),
                   ('list_roles_of_norec', "LIST ROLES OF auth_alice NORECURSIVE"), ('list_roles_of_unknown_su', "LIST ROLES OF auth_nope"),
                   ('list_users_super', "LIST USERS"),
                   ('list_perms_alice', "LIST ALL PERMISSIONS OF auth_alice"),
                   ('list_perms_alice_norec', "LIST ALL PERMISSIONS OF auth_alice NORECURSIVE"),
                   ('list_perms_on_table', "LIST ALL PERMISSIONS ON auth_ks.t"),
                   ('list_perms_on_table_norec', "LIST ALL PERMISSIONS ON auth_ks.t NORECURSIVE"),
                   ('list_perms_on_ks', "LIST ALL PERMISSIONS ON KEYSPACE auth_ks"),
                   ('list_perms_select_on_t2', "LIST SELECT ON auth_ks.t2"),
                   ('list_perms_unknown_role', "LIST ALL PERMISSIONS OF auth_nope"),
                   ('list_perms_unknown_table', "LIST ALL PERMISSIONS ON auth_ks.nope"),
                   ('list_perms_unknown_ks', "LIST ALL PERMISSIONS ON KEYSPACE auth_nope"),
                   ('list_perms_none', "LIST ALL PERMISSIONS OF auth_nologin"),
                   ('list_perms_all_roles', "LIST ALL PERMISSIONS ON ALL ROLES"),
                   ('grant_unknown_table', "GRANT SELECT ON auth_ks.nope TO auth_alice"), ('grant_unknown_keyspace', "GRANT SELECT ON KEYSPACE auth_nope TO auth_alice"),
                   ('grant_unknown_role_perm', "GRANT SELECT ON KEYSPACE auth_ks TO auth_nope"),
                   ('grant_unknown_role_and_table', "GRANT SELECT ON auth_ks.nope TO auth_nope"),
                   ('grant_bad_permission', "GRANT NOPE ON KEYSPACE auth_ks TO auth_alice"),
                   ('grant_describe_on_keyspace', "GRANT DESCRIBE ON KEYSPACE auth_ks TO auth_alice"),
                   ('grant_execute_on_table', "GRANT EXECUTE ON auth_ks.t TO auth_alice"),
                   ('grant_create_on_table', "GRANT CREATE ON auth_ks.t TO auth_alice"),
                   ('grant_authorize_on_role', "GRANT AUTHORIZE ON ROLE auth_bob TO auth_alice"),
                   ('grant_describe_on_role', "GRANT DESCRIBE ON ROLE auth_bob TO auth_alice"),
                   ('grant_create_on_role', "GRANT CREATE ON ROLE auth_bob TO auth_alice"),
                   ('grant_select_on_role', "GRANT SELECT ON ROLE auth_bob TO auth_alice"),
                   ('grant_on_unknown_role_resource', "GRANT ALTER ON ROLE auth_nope TO auth_alice"),
                   ('grant_all_roles', "GRANT ALL ON ALL ROLES TO auth_r3"),
                   ('grant_describe_all_roles', "GRANT DESCRIBE ON ALL ROLES TO auth_r3"),
                   ('grant_all_functions', "GRANT EXECUTE ON ALL FUNCTIONS TO auth_r3"),
                   ('grant_all_functions_ks', "GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE auth_ks TO auth_r3"),
                   ('grant_all_functions_unknown_ks', "GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE auth_nope TO auth_r3"),
                   ('grant_unknown_function', "GRANT EXECUTE ON FUNCTION auth_ks.nope(int) TO auth_r3"),
                   ('grant_builtin_function', "GRANT EXECUTE ON FUNCTION system.now() TO auth_r3"),
                   ('grant_all_mbeans', "GRANT SELECT ON ALL MBEANS TO auth_r3"),
                   ('grant_mbean', "GRANT SELECT ON MBEAN 'org.apache.cassandra.db:type=Tables' TO auth_r3"),
                   ('grant_mbeans_pattern', "GRANT EXECUTE ON MBEANS 'org.apache.cassandra.db:type=*' TO auth_r3"),
                   ('grant_all_tables', "GRANT SELECT ON ALL TABLES IN KEYSPACE auth_ks TO auth_r3"),
                   ('grant_unmask', "GRANT UNMASK ON auth_ks.t TO auth_r3"), ('grant_select_masked', "GRANT SELECT_MASKED ON auth_ks.t TO auth_r3"),
                   ('grant_vector_search', "GRANT VECTOR_SEARCH_INDEXING ON ALL KEYSPACES TO auth_r3"),
                   ('grant_vector_search_ks', "GRANT VECTOR_SEARCH_INDEXING ON KEYSPACE auth_ks TO auth_r3"),
                   ('grant_system_keyspace_select', "GRANT SELECT ON KEYSPACE system TO auth_r3"),
                   ('grant_system_keyspace_modify', "GRANT MODIFY ON KEYSPACE system TO auth_r3"),
                   ('grant_system_table_modify', "GRANT MODIFY ON system.local TO auth_r3"),
                   ('grant_system_auth_alter', "GRANT ALTER ON KEYSPACE system_auth TO auth_r3"),
                   ('grant_system_schema_all', "GRANT ALL ON KEYSPACE system_schema TO auth_r3"),
                   ('grant_all_keyspaces_all', "GRANT ALL ON ALL KEYSPACES TO auth_r3"),
                   ('list_perms_r3', "LIST ALL PERMISSIONS OF auth_r3 NORECURSIVE"),
                   ('revoke_unknown_keyspace', "REVOKE SELECT ON KEYSPACE auth_nope FROM auth_alice"),
                   ('revoke_not_granted', "REVOKE ALTER ON KEYSPACE auth_ks FROM auth_alice"),
                   ('revoke_partial', "REVOKE ALL ON auth_ks.t2 FROM auth_r1"),
                   ('revoke_some_not_granted', "REVOKE SELECT ON auth_ks.t FROM auth_alice"),
                   ('revoke_all_roles', "REVOKE ALL ON ALL ROLES FROM auth_r3"),
                   ('revoke_unknown_role', "REVOKE SELECT ON auth_ks.t FROM auth_nope"),
                   ('perm_rows_after_revoke', "SELECT role, resource, permissions FROM system_auth.role_permissions WHERE role = 'auth_alice'"),
                   ('perm_rows_after_revoke_scylla', "SELECT role, resource, permissions FROM system.role_permissions WHERE role = 'auth_alice'"),
                   ('perm_index_after_revoke', "SELECT * FROM system_auth.resource_role_permissons_index WHERE resource = 'data/auth_ks/t'"),
                   ('create_user_legacy', "CREATE USER auth_legacy WITH PASSWORD 'x' NOSUPERUSER"),
                   ('create_user_super', "CREATE USER auth_legacy2 WITH PASSWORD 'x' SUPERUSER"),
                   ('create_user_exists', "CREATE USER auth_alice WITH PASSWORD 'x'"),
                   ('create_user_if_not_exists', "CREATE USER IF NOT EXISTS auth_alice WITH PASSWORD 'x'"),
                   ('create_user_quoted', 'CREATE USER "auth_Q" WITH PASSWORD \'x\''),
                   ('create_user_hashed', "CREATE USER auth_x WITH HASHED PASSWORD 'notahash'"),
                   ('alter_user_legacy', "ALTER USER auth_legacy WITH PASSWORD 'y'"),
                   ('alter_user_superuser', "ALTER USER auth_legacy SUPERUSER"),
                   ('alter_user_nothing', "ALTER USER auth_legacy"),
                   ('alter_user_unknown', "ALTER USER auth_nope WITH PASSWORD 'y'"),
                   ('alter_user_if_exists', "ALTER USER IF EXISTS auth_nope WITH PASSWORD 'y'"),
                   ('legacy_rows', "SELECT role, can_login, is_superuser FROM system_auth.roles WHERE role IN ('auth_legacy', 'auth_legacy2')"),
                   ('legacy_rows_scylla', "SELECT role, can_login, is_superuser FROM system.roles WHERE role IN ('auth_legacy', 'auth_legacy2')"),
                   ('list_users_super2', "LIST USERS"),
                   ('drop_user_legacy2', "DROP USER auth_legacy2"),
                   ('drop_user_unknown', "DROP USER auth_nope"),
                   ('drop_user_if_exists', "DROP USER IF EXISTS auth_nope"),
                   ('add_identity', "ADD IDENTITY 'auth_id1' TO ROLE 'auth_alice'"),
                   ('add_identity_exists', "ADD IDENTITY 'auth_id1' TO ROLE 'auth_bob'"),
                   ('add_identity_if_not_exists', "ADD IDENTITY IF NOT EXISTS 'auth_id1' TO ROLE 'auth_bob'"),
                   ('add_identity_unknown_role', "ADD IDENTITY 'auth_id2' TO ROLE 'auth_nope'"),
                   ('add_identity_super_role', "ADD IDENTITY 'auth_id2' TO ROLE 'auth_r2'"),
                   ('identity_rows', "SELECT * FROM system_auth.identity_to_role WHERE identity IN ('auth_id1', 'auth_id2')"),
                   ('drop_identity', "DROP IDENTITY 'auth_id2'"),
                   ('drop_identity_unknown', "DROP IDENTITY 'auth_nope'"),
                   ('drop_identity_if_exists', "DROP IDENTITY IF EXISTS 'auth_nope'"),
                   ('create_role_with_identity', "CREATE ROLE auth_x"),
                   ('drop_x', "DROP ROLE auth_x"),
                   ]:
        c(cid, 'su', q)
    c('alice_drop_identity_of_super', 'alice', "DROP IDENTITY 'auth_id1'")
    c('alice_add_identity_super_role', 'alice', "ADD IDENTITY 'auth_id3' TO ROLE 'auth_r2'")
    # password change rate limit
    c('pw_change_1', 'su', "ALTER ROLE auth_bob WITH PASSWORD = 'bob2'")
    c('pw_change_2', 'su', "ALTER ROLE auth_bob WITH PASSWORD = 'bob3'")
    c('pw_change_hashed', 'su', "ALTER ROLE auth_bob WITH HASHED PASSWORD = '$2a$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS'")
    c('login_after_pw_change', '-', '!login auth_bob bob2')
    c('login_after_pw_change_old', '-', '!login auth_bob bob')
    c('login_hashed_sha512', '-', '!login auth_hashed cassandra')
    c('login_hashed_bcrypt', '-', '!login auth_hashed2 cassandra')
    c('login_dc', '-', '!login auth_dc dc')
    c('dc_query', 'dc', "SELECT * FROM auth_ks.t")
    c('alter_dc_restrict', 'su', "ALTER ROLE auth_dc WITH ACCESS TO DATACENTERS {'datacenter1'}")
    # creator grants
    c('grant_create_all_ks', 'su', "GRANT CREATE ON ALL KEYSPACES TO auth_bob")
    c('sleep', '-', '!sleep 2.5')
    c('bob_create_ks', 'bob', KS.replace('auth_ks', 'auth_ks2'))
    c('bob_create_table', 'bob', "CREATE TABLE auth_ks2.t (pk int PRIMARY KEY)")
    c('bob_create_type', 'bob', "CREATE TYPE auth_ks2.ty (a int)")
    c('bob_perms_after_create', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('bob_create_ks_if_not_exists_existing', 'bob', KS.replace('KEYSPACE auth_ks', 'KEYSPACE IF NOT EXISTS auth_ks'))
    c('bob_perms_after_ifne', 'su', "LIST ALL PERMISSIONS ON KEYSPACE auth_ks OF auth_bob NORECURSIVE")
    c('bob_drop_table', 'bob', "DROP TABLE auth_ks2.t")
    c('bob_perms_after_drop_table', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('bob_drop_ks', 'bob', "DROP KEYSPACE auth_ks2")
    c('bob_perms_after_drop_ks', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('perm_rows_after_drop_ks', 'su', "SELECT role, resource FROM system_auth.role_permissions WHERE role = 'auth_bob'")
    c('perm_rows_after_drop_ks_scylla', 'su', "SELECT role, resource FROM system.role_permissions WHERE role = 'auth_bob'")
    c('su_create_ks3', 'su', KS.replace('auth_ks', 'auth_ks3'))
    c('su_ks3_table', 'su', "CREATE TABLE auth_ks3.t (pk int PRIMARY KEY)")
    c('grant_ks3', 'su', "GRANT SELECT ON auth_ks3.t TO auth_alice")
    c('drop_ks3_table', 'su', "DROP TABLE auth_ks3.t")
    c('recreate_ks3_table', 'su', "CREATE TABLE auth_ks3.t (pk int PRIMARY KEY)")
    c('perms_after_recreate', 'su', "LIST ALL PERMISSIONS ON auth_ks3.t OF auth_alice NORECURSIVE")
    c('drop_ks3', 'su', "DROP KEYSPACE auth_ks3")
    # service levels
    for cid, q in [('create_sl', "CREATE SERVICE LEVEL auth_sl1 WITH timeout = 10s AND workload_type = 'interactive'"),
                   ('create_sl2', "CREATE SERVICE LEVEL auth_sl2 WITH shares = 500 AND timeout = 2s AND workload_type = 'batch'"),
                   ('create_sl_bad_option', "CREATE SERVICE LEVEL auth_sl3 WITH nope = 1"),
                   ('attach_sl', "ATTACH SERVICE LEVEL auth_sl1 TO auth_alice"),
                   ('attach_sl_again', "ATTACH SERVICE LEVEL auth_sl2 TO auth_alice"),
                   ('attach_sl2_r1', "ATTACH SERVICE LEVEL auth_sl1 TO auth_r1"),
                   ('attach_unknown_sl', "ATTACH SERVICE LEVEL auth_nope TO auth_alice"),
                   ('attach_sl_unknown_role', "ATTACH SERVICE LEVEL auth_sl1 TO auth_nope"),
                   ('attach_default', "ATTACH SERVICE LEVEL default TO auth_bob"),
                   ('list_service_levels', "LIST ALL SERVICE LEVELS"),
                   ('list_attached', "LIST ALL ATTACHED SERVICE LEVELS"),
                   ('list_attached_of', "LIST ATTACHED SERVICE LEVEL OF auth_alice"),
                   ('list_attached_of_none', "LIST ATTACHED SERVICE LEVEL OF auth_bob"),
                   ('list_attached_of_unknown', "LIST ATTACHED SERVICE LEVEL OF auth_nope"),
                   ('list_effective', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice"),
                   ('list_effective_r1', "LIST EFFECTIVE SERVICE LEVEL OF auth_r1"),
                   ('list_effective_none', "LIST EFFECTIVE SERVICE LEVEL OF auth_nologin"),
                   ('list_effective_super', "LIST EFFECTIVE SERVICE LEVEL OF cassandra"),
                   ('role_attr_rows', "SELECT * FROM system.role_attributes WHERE role IN ('auth_alice', 'auth_r1')"),
                   ('alter_sl', "ALTER SERVICE LEVEL auth_sl2 WITH timeout = 500ms"),
                   ('list_effective_after_alter', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice"),
                   ('drop_sl_attached', "DROP SERVICE LEVEL auth_sl2"),
                   ('role_attr_after_drop_sl', "SELECT * FROM system.role_attributes WHERE role IN ('auth_alice', 'auth_r1')"),
                   ('list_attached_after_drop', "LIST ALL ATTACHED SERVICE LEVELS"),
                   ('detach_sl', "DETACH SERVICE LEVEL FROM auth_r1"),
                   ('detach_sl_none', "DETACH SERVICE LEVEL FROM auth_r1"),
                   ('detach_unknown_role', "DETACH SERVICE LEVEL FROM auth_nope"),
                   ('list_attached_after_detach', "LIST ALL ATTACHED SERVICE LEVELS"),
                   ('attach_sl_r1_again', "ATTACH SERVICE LEVEL auth_sl1 TO auth_r1"),
                   ('drop_role_with_sl', "DROP ROLE auth_r1"),
                   ('role_attr_after_drop_role', "SELECT * FROM system.role_attributes WHERE role IN ('auth_alice', 'auth_r1')"),
                   ('drop_service_level', "DROP SERVICE LEVEL auth_sl1"),
                   ('drop_service_level_unknown', "DROP SERVICE LEVEL auth_nope")]:
        c(cid, 'su', q)
    # drop cleanup
    c('drop_role_alice', 'su', "DROP ROLE auth_alice")
    c('perm_rows_after_drop_role', 'su', "SELECT role, resource FROM system_auth.role_permissions WHERE role = 'auth_alice'")
    c('perm_rows_after_drop_role_scylla', 'su', "SELECT role, resource FROM system.role_permissions WHERE role = 'auth_alice'")
    c('members_after_drop_role', 'su', "SELECT * FROM system_auth.role_members WHERE role = 'auth_r3'")
    c('members_after_drop_role_scylla', 'su', "SELECT * FROM system.role_members WHERE role = 'auth_r3'")
    c('identity_after_drop_role', 'su', "SELECT * FROM system_auth.identity_to_role WHERE identity = 'auth_id1'")
    c('perm_index_after_drop_role', 'su', "SELECT * FROM system_auth.resource_role_permissons_index WHERE resource = 'data/auth_ks/t'")
    c('sleep', '-', '!sleep 2.5')
    c('query_after_role_dropped', 'alice', "SELECT * FROM auth_ks.t")
    c('list_roles_after_role_dropped', 'alice', "LIST ROLES")
    c('drop_ks', 'su', "DROP KEYSPACE auth_ks")
    c('perm_rows_after_drop_ks_r3', 'su', "SELECT role, resource FROM system_auth.role_permissions WHERE role = 'auth_r3'")
    c('perm_rows_after_drop_ks_r3_scylla', 'su', "SELECT role, resource FROM system.role_permissions WHERE role = 'auth_r3'")

    return CLEAN, C


def battery2():
    KS = "CREATE KEYSPACE auth_ks WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"
    ROLES = ['auth_alice', 'auth_bob', 'auth_carol', 'auth_r1', 'auth_r2', 'auth_viewer']
    CLEAN = [('clean', 'su', 'DROP ROLE IF EXISTS %s' % r) for r in ROLES] + [('clean', 'su', 'DROP KEYSPACE IF EXISTS %s' % k) for k in ('auth_ks', 'auth_ks2', 'auth_ks3')]
    C = []
    def c(cid, who, q): C.append((cid, who, q))
    c('setup_ks', 'su', KS)
    c('setup_t', 'su', 'CREATE TABLE auth_ks.t (pk int PRIMARY KEY, v int)')
    c('setup_alice', 'su', "CREATE ROLE auth_alice WITH PASSWORD = 'alice' AND LOGIN = true")
    c('setup_bob', 'su', "CREATE ROLE auth_bob WITH PASSWORD = 'bob' AND LOGIN = true")
    c('setup_carol', 'su', "CREATE ROLE auth_carol WITH PASSWORD = 'carol' AND LOGIN = true")
    c('grant_create_all_ks', 'su', "GRANT CREATE ON ALL KEYSPACES TO auth_bob")
    c('sleep', '-', '!sleep 2.5')
    c('bob_create_ks', 'bob', KS.replace('auth_ks', 'auth_ks2'))
    c('bob_create_table', 'bob', "CREATE TABLE auth_ks2.t (pk int PRIMARY KEY)")
    c('bob_create_type', 'bob', "CREATE TYPE auth_ks2.ty (a int)")
    c('bob_perms_after_create', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('bob_select_own_table', 'bob', "SELECT * FROM auth_ks2.t")
    c('bob_create_ks_ifne_existing', 'bob', KS.replace('KEYSPACE auth_ks', 'KEYSPACE IF NOT EXISTS auth_ks'))
    c('bob_perms_after_ifne', 'su', "LIST ALL PERMISSIONS ON KEYSPACE auth_ks OF auth_bob NORECURSIVE")
    c('bob_create_table_ifne_existing', 'bob', "CREATE TABLE IF NOT EXISTS auth_ks2.t (pk int PRIMARY KEY)")
    c('bob_alter_own_table', 'bob', "ALTER TABLE auth_ks2.t ADD v int")
    c('bob_create_index', 'bob', "CREATE INDEX ON auth_ks2.t (v)")
    c('bob_drop_table', 'bob', "DROP TABLE auth_ks2.t")
    c('bob_perms_after_drop_table', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('bob_drop_ks', 'bob', "DROP KEYSPACE auth_ks2")
    c('bob_perms_after_drop_ks', 'su', "LIST ALL PERMISSIONS OF auth_bob NORECURSIVE")
    c('perm_rows_bob', 'su', "SELECT role, resource, permissions FROM system_auth.role_permissions WHERE role = 'auth_bob'")
    c('perm_rows_bob_scylla', 'su', "SELECT role, resource, permissions FROM system.role_permissions WHERE role = 'auth_bob'")
    # role creator grants
    c('grant_create_roles', 'su', "GRANT CREATE ON ALL ROLES TO auth_carol")
    c('sleep', '-', '!sleep 2.5')
    c('carol_create_role', 'carol', "CREATE ROLE auth_r1")
    c('carol_perms', 'su', "LIST ALL PERMISSIONS OF auth_carol NORECURSIVE")
    c('carol_alter_created', 'carol', "ALTER ROLE auth_r1 WITH LOGIN = true")
    c('carol_grant_created_role', 'carol', "GRANT auth_r1 TO auth_bob")
    c('carol_drop_created', 'carol', "DROP ROLE auth_r1")
    c('carol_perms_after_drop', 'su', "LIST ALL PERMISSIONS OF auth_carol NORECURSIVE")
    c('carol_create_super', 'carol', "CREATE ROLE auth_r2 WITH SUPERUSER = true")
    c('su_create_r2', 'su', "CREATE ROLE auth_r2 WITH SUPERUSER = true")
    c('grant_drop_r2_carol', 'su', "GRANT DROP ON ROLE auth_r2 TO auth_carol")
    c('sleep', '-', '!sleep 2.5')
    c('carol_drop_super', 'carol', "DROP ROLE auth_r2")
    c('su_perms_on_role', 'su', "LIST ALL PERMISSIONS ON ROLE auth_r2")
    c('su_perms_on_all_roles', 'su', "LIST ALL PERMISSIONS ON ALL ROLES")
    # describe permission to list roles
    c('create_viewer', 'su', "CREATE ROLE auth_viewer WITH PASSWORD = 'v' AND LOGIN = true")
    c('grant_describe_roles', 'su', "GRANT DESCRIBE ON ALL ROLES TO auth_viewer")
    c('sleep', '-', '!sleep 2.5')
    c('viewer_list_roles', 'viewer', "LIST ROLES")
    c('viewer_list_roles_of_alice', 'viewer', "LIST ROLES OF auth_alice")
    c('viewer_list_all_perms', 'viewer', "LIST ALL PERMISSIONS")
    c('viewer_list_perms_of_alice', 'viewer', "LIST ALL PERMISSIONS OF auth_alice")
    c('viewer_list_users', 'viewer', "LIST USERS")
    c('grant_describe_role_alice', 'su', "GRANT DESCRIBE ON ROLE auth_alice TO auth_carol")
    c('sleep', '-', '!sleep 2.5')
    c('carol_list_perms_of_alice', 'carol', "LIST ALL PERMISSIONS OF auth_alice")
    c('carol_list_roles_of_alice', 'carol', "LIST ROLES OF auth_alice")
    c('su_perms_r3', 'su', "LIST ALL PERMISSIONS OF auth_viewer")
    # alter as non-super with ALTER permission
    c('grant_alter_role_bob', 'su', "GRANT ALTER ON ROLE auth_bob TO auth_carol")
    c('sleep', '-', '!sleep 2.5')
    c('carol_alter_bob_pw', 'carol', "ALTER ROLE auth_bob WITH PASSWORD = 'bob'")
    c('carol_alter_bob_login', 'carol', "ALTER ROLE auth_bob WITH LOGIN = true")
    c('carol_alter_bob_super', 'carol', "ALTER ROLE auth_bob WITH SUPERUSER = true")
    c('grant_authorize_role_bob', 'su', "GRANT AUTHORIZE ON ROLE auth_bob TO auth_carol")
    c('sleep', '-', '!sleep 2.5')
    c('carol_grant_bob_to_alice', 'carol', "GRANT auth_bob TO auth_alice")
    c('carol_revoke_bob_from_alice', 'carol', "REVOKE auth_bob FROM auth_alice")
    c('carol_create_user', 'carol', "CREATE USER auth_r1 WITH PASSWORD 'x'")
    c('carol_list_users', 'carol', "LIST USERS")
    # keyspace-level grant propagates to new tables
    c('grant_select_ks_alice', 'su', "GRANT SELECT ON KEYSPACE auth_ks TO auth_alice")
    c('create_t2', 'su', "CREATE TABLE auth_ks.t2 (pk int PRIMARY KEY)")
    c('sleep', '-', '!sleep 2.5')
    c('alice_select_new_table', 'alice', "SELECT * FROM auth_ks.t2")
    c('alice_select_system_auth_with_ks_grant', 'alice', "SELECT * FROM system.local")
    c('grant_select_all_ks_alice', 'su', "GRANT SELECT ON ALL KEYSPACES TO auth_alice")
    c('sleep', '-', '!sleep 2.5')
    c('alice_select_system_roles_all_ks', 'alice', "SELECT role FROM system.roles")
    c('alice_select_system_auth_roles_all_ks', 'alice', "SELECT role FROM system_auth.roles")
    c('alice_select_system_paxos_all_ks', 'alice', "SELECT * FROM system.paxos LIMIT 1")
    c('alice_select_system_schema_all_ks', 'alice', "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'auth_ks'")
    c('alice_select_traces_all_ks', 'alice', "SELECT * FROM system_traces.events LIMIT 1")
    c('alice_select_distributed_all_ks', 'alice', "SELECT * FROM system_distributed.view_build_status LIMIT 1")
    c('alice_alter_system_auth', 'alice', "ALTER KEYSPACE system_auth WITH durable_writes = true")
    c('alice_drop_system_table', 'alice', "DROP TABLE system.local")
    c('alice_drop_system_auth_table', 'alice', "DROP TABLE system_auth.roles")
    c('alice_create_table_system_auth', 'alice', "CREATE TABLE system_auth.x (pk int PRIMARY KEY)")
    c('alice_truncate_system_auth', 'alice', "TRUNCATE system_auth.roles")
    c('alice_insert_system_auth', 'alice', "INSERT INTO system_auth.roles (role) VALUES ('auth_zz')")
    c('alice_insert_system_roles', 'alice', "INSERT INTO system.roles (role) VALUES ('auth_zz')")
    c('su_drop_system_auth_table', 'su', "DROP TABLE system_auth.roles")
    c('su_drop_system_roles_table', 'su', "DROP TABLE system.roles")
    c('su_drop_ks_system_auth', 'su', "DROP KEYSPACE system_auth")
    c('su_alter_table_system_auth', 'su', "ALTER TABLE system_auth.roles ADD x int")
    c('su_alter_table_system_roles', 'su', "ALTER TABLE system.roles ADD x int")
    # superuser LIST ALL PERMISSIONS (filtered by auth_)
    c('su_list_all', 'su', "LIST ALL PERMISSIONS")
    c('su_list_all_norec', 'su', "LIST ALL PERMISSIONS NORECURSIVE")
    c('su_list_select', 'su', "LIST SELECT")
    c('su_list_on_all_keyspaces', 'su', "LIST ALL PERMISSIONS ON ALL KEYSPACES")
    c('su_list_on_role_bob', 'su', "LIST ALL ON ROLE auth_bob")
    c('su_list_with_permission_kw', 'su', "LIST SELECT PERMISSION OF auth_alice")
    c('su_list_multi', 'su', "LIST SELECT, MODIFY OF auth_alice")
    c('su_grant_multi', 'su', "GRANT SELECT, MODIFY ON auth_ks.t TO auth_carol")
    c('su_grant_multi_perm_kw', 'su', "GRANT SELECT PERMISSION, ALTER PERMISSION ON auth_ks.t TO auth_carol")
    c('su_list_carol', 'su', "LIST ALL OF auth_carol")
    c('su_revoke_multi', 'su', "REVOKE SELECT, ALTER ON auth_ks.t FROM auth_carol")
    c('su_grant_unqualified_table', 'su', "GRANT SELECT ON t TO auth_carol")
    c('su_use', 'su', "USE auth_ks")
    c('su_grant_unqualified_table_after_use', 'su', "GRANT SELECT ON t TO auth_carol")
    c('su_list_unqualified', 'su', "LIST ALL ON t OF auth_carol")
    c('su_grant_quoted_role', 'su', "GRANT SELECT ON auth_ks.t TO \"auth_Carol\"")
    c('su_grant_string_role', 'su', "GRANT SELECT ON auth_ks.t TO 'auth_carol'")
    c('su_drop_t_cleans', 'su', "DROP TABLE auth_ks.t")
    c('su_perms_carol_after_drop', 'su', "LIST ALL OF auth_carol")

    return CLEAN, C


def battery3():
    ROLES = ['auth_alice', 'auth_r1']
    CLEAN = [('clean', 'su', 'DROP ROLE IF EXISTS %s' % r) for r in ROLES] + [('clean', 'su', 'DROP SERVICE LEVEL IF EXISTS auth_sl%d' % i) for i in (1, 2, 3)] + [('clean', 'su', 'DROP KEYSPACE IF EXISTS auth_ks')]
    C = []
    def c(cid, who, q): C.append((cid, who, q))
    c('setup_alice', 'su', "CREATE ROLE auth_alice WITH PASSWORD = 'alice' AND LOGIN = true")
    c('setup_r1', 'su', "CREATE ROLE auth_r1")
    for cid, q in [('drop_system_auth_nope', "DROP TABLE system_auth.nope"), ('alter_system_traces_nope', "ALTER TABLE system_traces.nope ADD x int"),
                   ('drop_system_distributed_nope', "DROP TABLE system_distributed.nope"), ('drop_system_traces_nope', "DROP TABLE system_traces.nope"),
                   ('drop_system_auth_nope_ifexists', "DROP TABLE IF EXISTS system_auth.nope"),
                   ('alter_system_auth_role_members', "ALTER TABLE system_auth.role_members ADD x int"),
                   ('drop_system_schema_table', "DROP TABLE system_schema.nope"),
                   ('drop_type_system_auth', "DROP TYPE system_auth.nope"),
                   ('create_type_system_traces', "CREATE TYPE system_traces.auth_ty (a int)"),
                   ('create_index_system_auth', "CREATE INDEX ON system_auth.roles (can_login)"),
                   ('drop_ks_system_traces', "DROP KEYSPACE system_traces"),
                   ('drop_ks_system', "DROP KEYSPACE system"),
                   ('alter_ks_system', "ALTER KEYSPACE system WITH durable_writes = true"),
                   ]:
        if cid != 'create_type_system_traces':  # ScyllaDB accepts it from a superuser and can't drop it
            c(cid, 'su', q)
        c(cid + '_alice', 'alice', q)
    c('sl1', 'su', "CREATE SERVICE LEVEL auth_sl1")
    c('sl2', 'su', "CREATE SERVICE LEVEL auth_sl2 WITH timeout = 1500ms")
    c('attach1', 'su', "ATTACH SERVICE LEVEL auth_sl1 TO auth_alice")
    c('effective_plain', 'su', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice")
    c('attach2', 'su', "ATTACH SERVICE LEVEL auth_sl2 TO auth_r1")
    c('grant_r1', 'su', "GRANT auth_r1 TO auth_alice")
    c('effective_merged', 'su', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice")
    c('effective_self_alice', 'alice', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice")
    c('effective_other_alice', 'alice', "LIST EFFECTIVE SERVICE LEVEL OF auth_r1")
    c('alter_sl_alice', 'alice', "ALTER SERVICE LEVEL auth_sl1 WITH shares = 5")
    c('drop_sl_alice', 'alice', "DROP SERVICE LEVEL auth_sl1")
    c('drop_sl_alice_unknown', 'alice', "DROP SERVICE LEVEL IF EXISTS auth_nope")
    c('list_sl_nope', 'su', "LIST SERVICE LEVEL auth_nope")
    c('alter_sl_shares', 'su', "ALTER SERVICE LEVEL auth_sl1 WITH shares = 10 AND workload_type = 'batch'")
    c('list_sl1', 'su', "LIST SERVICE LEVEL auth_sl1")
    c('alter_sl_empty', 'su', "ALTER SERVICE LEVEL auth_sl1 WITH timeout = 10s AND shares = 1000")
    c('alter_sl_wl_null', 'su', "ALTER SERVICE LEVEL auth_sl1 WITH workload_type = null")
    c('effective_after', 'su', "LIST EFFECTIVE SERVICE LEVEL OF auth_alice")
    c('sl_timeout_fmt', 'su', "ALTER SERVICE LEVEL auth_sl2 WITH timeout = 90061001ms")
    c('list_sl2', 'su', "LIST SERVICE LEVEL auth_sl2")
    c('effective_after2', 'su', "LIST EFFECTIVE SERVICE LEVEL OF auth_r1")
    c('sl_quoted_name', 'su', "CREATE SERVICE LEVEL 'auth_SL3'")
    c('sl_quoted_list', 'su', "LIST SERVICE LEVEL 'auth_SL3'")
    c('sl_quoted_drop', 'su', "DROP SERVICE LEVEL 'auth_SL3'")
    c('sl_timeout_str', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH timeout = '2s'")
    c('sl_list_ws', 'su', "LIST ALL SERVICE LEVELS")
    c('sl_drop3', 'su', "DROP SERVICE LEVEL IF EXISTS auth_sl3")
    c('sl_shares_str', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH shares = '7'")
    c('sl_shares_neg', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH shares = -1")
    c('sl_shares_float', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH shares = 1.5")
    c('sl_wl_bool', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH workload_type = true")
    c('sl_map', 'su', "CREATE SERVICE LEVEL auth_sl3 WITH timeout = {'a': 'b'}")
    c('describe_sl', 'su', "DESCRIBE SCHEMA WITH INTERNALS")
    c('drop_sl3', 'su', "DROP SERVICE LEVEL IF EXISTS auth_sl3")
    # misc
    c('create_ks', 'su', "CREATE KEYSPACE auth_ks WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}")
    c('create_t', 'su', "CREATE TABLE auth_ks.t (pk int PRIMARY KEY, v int)")
    c('grant_sel', 'su', "GRANT SELECT ON auth_ks.t TO auth_alice")
    c('sleep', '-', '!sleep 2.5')
    c('alice_select_json', 'alice', "SELECT JSON * FROM auth_ks.t")
    c('alice_select_count', 'alice', "SELECT count(*) FROM auth_ks.t")
    c('alice_select_fn', 'alice', "SELECT now() FROM auth_ks.t")
    c('alice_prepare_insert', 'alice', "!prepare INSERT INTO auth_ks.t (pk, v) VALUES (?, ?)")
    c('alice_prepare_select_unk', 'alice', "!prepare SELECT * FROM auth_ks.nope")
    c('alice_prepare_ddl', 'alice', "!prepare DROP TABLE auth_ks.t")

    return CLEAN, C


def battery4():
    ROLES = ['auth_alice', 'auth_sup', 'auth_child', 'auth_h']
    CLEAN = [('clean', 'su', 'DROP ROLE IF EXISTS %s' % r) for r in ROLES]
    C = []
    def c(cid, who, q): C.append((cid, who, q))
    c('setup_alice', 'su', "CREATE ROLE auth_alice WITH PASSWORD = 'alice' AND LOGIN = true")
    c('setup_sup', 'su', "CREATE ROLE auth_sup WITH SUPERUSER = true")
    c('setup_child', 'su', "CREATE ROLE auth_child WITH LOGIN = true")
    c('grant_sup_child', 'su', "GRANT auth_sup TO auth_child")
    c('grant_create_roles', 'su', "GRANT CREATE ON ALL ROLES TO auth_alice")
    c('grant_drop_child', 'su', "GRANT DROP ON ROLE auth_child TO auth_alice")
    c('list_users_inherited_super', 'su', "LIST USERS")
    c('sleep', '-', '!sleep 2.5')
    c('insert_system_traces', 'alice', "INSERT INTO system_traces.events (session_id, event_id) VALUES (uuid(), now())")
    c('insert_system_distributed', 'alice', "INSERT INTO system_distributed.view_build_status (keyspace_name, view_name, host_id) VALUES ('a', 'b', uuid())")
    c('create_hashed_nonsuper', 'alice', "CREATE ROLE auth_h WITH HASHED PASSWORD = '$6$rollsalt$SB70vlMcKgHm/b626DEcCWr/ck3vuP7A07ziftzQEWGhAZFTyTrq6t4ZG8EcoCms9ps65p8W2Ei7VvEws6EjU1'")
    c('drop_inherited_super', 'alice', "DROP ROLE auth_child")
    return CLEAN, C


BATTERIES = [('roles', *battery1()), ('creators', *battery2()), ('system', *battery3()), ('extras', *battery4())]
