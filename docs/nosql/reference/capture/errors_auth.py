# Auth errors on a node with PasswordAuthenticator + CassandraAuthorizer (superuser cassandra/cassandra).
import sys, json, time, os
sys.path.insert(0, '/cap')
from common import err
from raw import Raw
host = os.environ.get('CAP_HOST', '127.0.0.1'); out = sys.argv[1]
# Shared auth-enabled references: every role, keyspace and service level is prefixed refs_ and
# dropped at the end; the cassandra role is only used to log in.
RES = []
su = Raw(host, user='cassandra', pw='cassandra')
def run(r, q): 
    try: return r.query(q)
    except Exception as e: return {'ok': False, 'error': {'code': None, 'message': 'client: ' + repr(e)}}
for q in ["DROP ROLE IF EXISTS refs_admin", "DROP ROLE IF EXISTS refs_alice", "DROP ROLE IF EXISTS refs_nologin", "DROP ROLE IF EXISTS refs_ra", "DROP ROLE IF EXISTS refs_legacy", "DROP ROLE IF EXISTS refs_r10",
          "DROP KEYSPACE IF EXISTS refs_ea",
          "CREATE KEYSPACE refs_ea WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}",
          "CREATE TABLE refs_ea.t (pk int PRIMARY KEY, v int)",
          "CREATE ROLE refs_alice WITH PASSWORD = 'refs_alice' AND LOGIN = true",
          "CREATE ROLE refs_nologin WITH PASSWORD = 'x' AND LOGIN = false",
          "CREATE ROLE refs_admin WITH PASSWORD = 'refs_admin' AND LOGIN = true AND SUPERUSER = true"]:
    RES.append(['setup', 'setup', q, run(su, q)])
time.sleep(2)
def login(cid, u, pw, label):
    try:
        r = Raw(host, user=u, pw=pw)
        st = {'ok': True, 'startup': r.startup[0]}
        if hasattr(r, 'auth'):
            op, e = r.auth
            st = {'ok': e is None, 'opcode': op}
            if e: st['error'] = e
        RES.append([cid, 'auth', label, st])
    except Exception as e:
        RES.append([cid, 'auth', label, {'ok': False, 'error': {'code': None, 'message': 'client: ' + repr(e)}}])
login('login_wrong_password', 'refs_alice', 'wrong', "SASL PLAIN refs_alice / wrong")
login('login_unknown_user', 'nobody', 'x', "SASL PLAIN nobody / x")
login('login_not_permitted', 'refs_nologin', 'x', "SASL PLAIN refs_nologin / x (LOGIN = false)")
login('login_empty_user', '', '', "SASL PLAIN empty user and password")
login('login_ok', 'refs_alice', 'refs_alice', "SASL PLAIN refs_alice / refs_alice")
# no credentials: STARTUP then a QUERY without AUTH_RESPONSE
try:
    r = Raw.__new__(Raw); import socket, struct
    from raw import smap
    r.v = 4; r.sock = socket.create_connection((host, 9042), timeout=30); r.stream = 0
    op, fl, body = r.req(1, smap({'CQL_VERSION': '3.0.0'}))
    from raw import rs
    RES.append(['startup_authenticate', 'auth', 'STARTUP → AUTHENTICATE body', {'ok': op == 3, 'opcode': op, 'authenticator': rs(body, 0)[0] if op == 3 else None}])
    RES.append(['query_before_auth', 'auth', 'QUERY SELECT key FROM system.local before AUTH_RESPONSE', r.query("SELECT key FROM system.local")])
    op, fl, body = r.req(0x0f, struct.pack('>i', 3) + b'abc')
    from raw import decode_error
    RES.append(['auth_response_garbage', 'auth', 'AUTH_RESPONSE with token "abc"', {'ok': op != 0, 'opcode': op, 'error': decode_error(body) if op == 0 else None}])
except Exception as e:
    RES.append(['startup_authenticate', 'auth', 'raw', {'ok': False, 'error': {'code': None, 'message': repr(e)}}])
al = Raw(host, user='refs_alice', pw='refs_alice')
for cid, q in [('select_no_permission', "SELECT * FROM refs_ea.t"), ('insert_no_permission', "INSERT INTO refs_ea.t (pk, v) VALUES (1, 1)"),
               ('create_ks_no_permission', "CREATE KEYSPACE refs_ea2 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}"),
               ('create_table_no_permission', "CREATE TABLE refs_ea.t2 (pk int PRIMARY KEY)"),
               ('drop_table_no_permission', "DROP TABLE refs_ea.t"), ('alter_table_no_permission', "ALTER TABLE refs_ea.t ADD x int"),
               ('truncate_no_permission', "TRUNCATE refs_ea.t"), ('create_index_no_permission', "CREATE INDEX ON refs_ea.t (v)"),
               ('grant_no_permission', "GRANT SELECT ON ALL KEYSPACES TO refs_alice"), ('create_role_no_permission', "CREATE ROLE refs_bob"),
               ('drop_role_no_permission', "DROP ROLE refs_admin"), ('alter_other_role', "ALTER ROLE refs_admin WITH PASSWORD = 'x'"),
               ('alter_own_superuser', "ALTER ROLE refs_alice WITH SUPERUSER = true"), ('alter_own_password', "ALTER ROLE refs_alice WITH PASSWORD = 'refs_alice'"),
               ('list_roles_nonsuper', "LIST ROLES"), ('list_all_permissions_nonsuper', "LIST ALL PERMISSIONS"),
               ('list_own_permissions', "LIST ALL PERMISSIONS OF refs_alice"), ('list_other_permissions', "LIST ALL PERMISSIONS OF refs_admin"),
               ('read_system_auth_roles', "SELECT * FROM system_auth.roles"), ('read_system_roles', "SELECT * FROM system.roles"),
               ('describe_passwords_nonsuper', "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS"), ('describe_schema_nonsuper', "DESCRIBE SCHEMA"),
               ('select_system_local_nonsuper', "SELECT cluster_name FROM system.local"), ('select_system_schema_nonsuper', "SELECT keyspace_name FROM system_schema.keyspaces"),
               ('use_keyspace_nonsuper', "USE refs_ea"), ('create_function_nonsuper', "CREATE FUNCTION refs_ea.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'"),
               ('service_level_nonsuper', "CREATE SERVICE LEVEL refs_sl_alice"), ('list_service_levels_nonsuper', "LIST ALL SERVICE LEVELS")]:
    RES.append([cid, 'auth', 'as refs_alice: ' + q, run(al, q)])
for cid, q in [('grant_select', "GRANT SELECT ON refs_ea.t TO refs_alice"), ('grant_modify_keyspace', "GRANT MODIFY ON KEYSPACE refs_ea TO refs_alice")]:
    RES.append([cid, 'auth', q, run(su, q)])
time.sleep(1)
for cid, q in [('select_after_grant', "SELECT * FROM refs_ea.t"), ('insert_after_grant', "INSERT INTO refs_ea.t (pk, v) VALUES (1, 1)"),
               ('alter_after_grant_select_only', "ALTER TABLE refs_ea.t ADD y int")]:
    RES.append([cid, 'auth', 'as refs_alice: ' + q, run(al, q)])
for cid, q in [('create_role_exists', "CREATE ROLE refs_alice"), ('create_role_if_not_exists', "CREATE ROLE IF NOT EXISTS refs_alice"),
               ('drop_role_unknown', "DROP ROLE nope"), ('drop_role_if_exists_unknown', "DROP ROLE IF EXISTS nope"),
               ('grant_unknown_table', "GRANT SELECT ON refs_ea.nope TO refs_alice"), ('grant_unknown_keyspace', "GRANT SELECT ON KEYSPACE nope TO refs_alice"),
               ('grant_unknown_role', "GRANT SELECT ON KEYSPACE refs_ea TO nope"), ('grant_bad_permission', "GRANT NOPE ON KEYSPACE refs_ea TO refs_alice"),
               ('grant_describe_on_keyspace', "GRANT DESCRIBE ON KEYSPACE refs_ea TO refs_alice"), ('grant_execute_on_table', "GRANT EXECUTE ON refs_ea.t TO refs_alice"),
               ('grant_authorize_on_role', "GRANT AUTHORIZE ON ROLE refs_alice TO refs_alice"), ('grant_select_on_role', "GRANT SELECT ON ROLE refs_alice TO refs_alice"),
               ('grant_all_functions', "GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE refs_ea TO refs_alice"), ('grant_unknown_function', "GRANT EXECUTE ON FUNCTION refs_ea.nope(int) TO refs_alice"),
               ('grant_mbean', "GRANT SELECT ON MBEAN 'org.apache.cassandra.db:type=Tables' TO refs_alice"),
               ('grant_unmask', "GRANT UNMASK ON refs_ea.t TO refs_alice"), ('grant_select_masked', "GRANT SELECT_MASKED ON refs_ea.t TO refs_alice"),
               ('grant_vector_search', "GRANT VECTOR_SEARCH_INDEXING ON ALL KEYSPACES TO refs_alice"),
               ('list_permissions_unknown_role', "LIST ALL PERMISSIONS OF nope"),
               ('alter_unknown_role', "ALTER ROLE nope WITH PASSWORD = 'x'"), ('revoke_unknown_keyspace', "REVOKE SELECT ON KEYSPACE nope FROM refs_alice"),
               ('revoke_not_granted', "REVOKE ALTER ON KEYSPACE refs_ea FROM refs_alice"),
               ('create_user_legacy', "CREATE USER refs_legacy WITH PASSWORD 'x' NOSUPERUSER"), ('create_user_exists', "CREATE USER refs_alice WITH PASSWORD 'x'"),
               ('alter_user_legacy', "ALTER USER refs_legacy WITH PASSWORD 'y'"), ('list_users_super', "LIST USERS"),
               ('role_bad_option', "CREATE ROLE refs_r9 WITH NOPE = 1"), ('role_options_map', "CREATE ROLE refs_r11 WITH OPTIONS = {'a': 'b'}"),
               ('role_hashed_password', "CREATE ROLE refs_r10 WITH HASHED PASSWORD = 'notahash' AND LOGIN = true"),
               ('role_password_and_hashed', "CREATE ROLE refs_r12 WITH PASSWORD = 'x' AND HASHED PASSWORD = 'y'"),
               ('role_generated_password', "CREATE ROLE refs_r13 WITH GENERATED PASSWORD"),
               ('role_access_to_datacenters', "CREATE ROLE refs_r14 WITH ACCESS TO DATACENTERS {'datacenter1'}"),
               ('role_access_to_cidrs', "CREATE ROLE refs_r15 WITH ACCESS FROM CIDRS {'region1'}"),
               ('grant_role_to_self', "GRANT refs_alice TO refs_alice"), ('grant_role_cycle_setup', "CREATE ROLE refs_ra"), ('grant_role_cycle_1', "GRANT refs_ra TO refs_alice"),
               ('grant_role_cycle_2', "GRANT refs_alice TO refs_ra"), ('grant_role_twice', "GRANT refs_ra TO refs_alice"), ('revoke_role_not_granted', "REVOKE refs_ra FROM refs_nologin"),
               ('list_roles_super', "LIST ROLES"), ('list_roles_of', "LIST ROLES OF refs_alice"), ('list_perms_alice', "LIST ALL PERMISSIONS OF refs_alice"),
               ('list_perms_on_table', "LIST ALL PERMISSIONS ON refs_ea.t"), ('list_perms_norecursive', "LIST ALL PERMISSIONS OF refs_alice NORECURSIVE"),
               ('describe_passwords_super', "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS"),
               ('create_service_level', "CREATE SERVICE LEVEL refs_sl1 WITH timeout = 10s AND workload_type = 'interactive'"),
               ('create_service_level_bad_option', "CREATE SERVICE LEVEL refs_sl2 WITH nope = 1"),
               ('attach_service_level', "ATTACH SERVICE LEVEL refs_sl1 TO refs_alice"), ('list_service_levels', "LIST ALL SERVICE LEVELS"),
               ('list_attached', "LIST ALL ATTACHED SERVICE LEVELS"), ('list_effective', "LIST EFFECTIVE SERVICE LEVEL OF refs_alice"),
               ('detach_service_level', "DETACH SERVICE LEVEL FROM refs_alice"), ('drop_service_level', "DROP SERVICE LEVEL refs_sl1"),
               ('drop_service_level_unknown', "DROP SERVICE LEVEL nope"),
               ('drop_role_alice', "DROP ROLE refs_alice")]:
    RES.append([cid, 'auth', q, run(su, q)])
time.sleep(1)
adm = Raw(host, user='refs_admin', pw='refs_admin')
RES.append(['drop_own_role', 'auth', 'as refs_admin (superuser): DROP ROLE refs_admin', run(adm, "DROP ROLE refs_admin")])
RES.append(['query_after_role_dropped', 'auth', 'as refs_alice (role dropped while connected): SELECT * FROM refs_ea.t', run(al, "SELECT * FROM refs_ea.t")])
for q in ["DROP ROLE IF EXISTS refs_alice", "DROP ROLE IF EXISTS refs_nologin", "DROP ROLE IF EXISTS refs_ra", "DROP ROLE IF EXISTS refs_legacy",
          "DROP ROLE IF EXISTS refs_r10", "DROP ROLE IF EXISTS refs_r11", "DROP ROLE IF EXISTS refs_r12", "DROP ROLE IF EXISTS refs_r13", "DROP ROLE IF EXISTS refs_r14",
          "DROP ROLE IF EXISTS refs_r15", "DROP SERVICE LEVEL IF EXISTS refs_sl1", "DROP KEYSPACE IF EXISTS refs_ea", "DROP ROLE IF EXISTS refs_admin"]:
    run(su, q)
json.dump(RES, open(out, 'w'), indent=1, default=str)
print('done', len(RES))
