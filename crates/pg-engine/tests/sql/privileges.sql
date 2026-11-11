-- Roles are cluster-wide in Postgres: clean up first and at the end.
DROP ROLE IF EXISTS pv_owner, pv_reader, pv_writer, pv_group, pv_other;
CREATE ROLE pv_owner;
CREATE ROLE pv_reader;
CREATE ROLE pv_writer;
CREATE ROLE pv_group;
CREATE ROLE pv_other;
GRANT pv_group TO pv_reader;
CREATE TABLE pv_t (id int PRIMARY KEY, secret text, note text);
INSERT INTO pv_t VALUES (1, 's1', 'n1'), (2, 's2', 'n2');
CREATE SEQUENCE pv_seq;
CREATE FUNCTION pv_f(int) RETURNS int LANGUAGE sql AS 'SELECT $1 + 1';
CREATE VIEW pv_v AS SELECT id, note FROM pv_t;
-- ACL columns start NULL (the default)
SELECT relname, relacl FROM pg_class WHERE relname IN ('pv_t', 'pv_seq', 'pv_v') ORDER BY 1;
SELECT proacl FROM pg_proc WHERE proname = 'pv_f';
-- GRANT / REVOKE on tables
GRANT SELECT ON pv_t TO pv_reader;
GRANT INSERT, UPDATE ON TABLE pv_t TO pv_writer WITH GRANT OPTION;
GRANT SELECT (id, note), UPDATE (note) ON pv_t TO pv_other;
GRANT ALL ON pv_v TO pv_group;
SELECT relname, relacl FROM pg_class WHERE relname IN ('pv_t', 'pv_v') ORDER BY 1;
SELECT attname, attacl FROM pg_attribute WHERE attrelid = 'pv_t'::regclass AND attnum > 0 ORDER BY attnum;
SELECT x.grantor::regrole, x.grantee::regrole, x.privilege_type, x.is_grantable FROM (SELECT (aclexplode(relacl)).* FROM pg_class WHERE relname = 'pv_t') x ORDER BY 2, 3;
SELECT (aclexplode(relacl)).privilege_type FROM pg_class WHERE relname = 'pv_v' ORDER BY 1;
SELECT a.grantee::regrole, a.privilege_type FROM pg_class c, aclexplode(c.relacl) a WHERE c.relname = 'pv_v' AND a.grantee <> c.relowner ORDER BY 2;
GRANT USAGE ON pv_t TO pv_reader;
GRANT SELECT ON pv_nope TO pv_reader;
GRANT SELECT ON pv_t TO pv_nobody;
GRANT SELECT (nope) ON pv_t TO pv_reader;
-- has_*_privilege
SELECT has_table_privilege('pv_reader', 'pv_t', 'SELECT'), has_table_privilege('pv_reader', 'pv_t', 'INSERT'),
       has_table_privilege('pv_writer', 'pv_t', 'INSERT WITH GRANT OPTION'), has_table_privilege('pv_other', 'pv_t', 'SELECT');
SELECT has_column_privilege('pv_other', 'pv_t', 'note', 'SELECT'), has_column_privilege('pv_other', 'pv_t', 'secret', 'SELECT'),
       has_column_privilege('pv_other', 'pv_t', 'note', 'UPDATE'), has_any_column_privilege('pv_other', 'pv_t', 'SELECT');
SELECT has_sequence_privilege('pv_reader', 'pv_seq', 'USAGE'), has_function_privilege('pv_reader', 'pv_f(int)', 'EXECUTE'),
       has_schema_privilege('pv_reader', 'public', 'USAGE'), has_schema_privilege('pv_reader', 'public', 'CREATE'),
       has_database_privilege('pv_reader', current_database(), 'CONNECT'), has_database_privilege('pv_reader', current_database(), 'CREATE');
SELECT has_table_privilege('pv_reader', 'pv_t', 'FOO');
SELECT has_table_privilege('pv_reader', 'pv_nope', 'SELECT');
-- Access checks as other roles
SET ROLE pv_reader;
SELECT current_user, session_user;
SELECT * FROM pv_t ORDER BY id;
INSERT INTO pv_t VALUES (3, 's3', 'n3');
UPDATE pv_t SET note = 'x';
DELETE FROM pv_t;
SELECT nextval('pv_seq');
SELECT pv_f(1);
SELECT * FROM pv_v ORDER BY id;
RESET ROLE;
SET ROLE pv_other;
SELECT id, note FROM pv_t ORDER BY id;
SELECT secret FROM pv_t;
SELECT * FROM pv_t;
SELECT count(*) FROM pv_t;
UPDATE pv_t SET note = 'changed' WHERE id = 1;
UPDATE pv_t SET secret = 'changed' WHERE id = 1;
UPDATE pv_t SET note = secret WHERE id = 1;
RESET ROLE;
SET ROLE pv_writer;
INSERT INTO pv_t VALUES (3, 's3', 'n3');
INSERT INTO pv_t VALUES (4, 's4', 'n4') RETURNING id;
UPDATE pv_t SET note = 'w' WHERE id = 3;
UPDATE pv_t SET note = 'w';
GRANT INSERT ON pv_t TO pv_other;
GRANT DELETE ON pv_t TO pv_other;
RESET ROLE;
-- Views run with their owner's rights
REVOKE SELECT ON pv_t FROM pv_reader;
SET ROLE pv_group;
SELECT * FROM pv_v ORDER BY id;
SELECT * FROM pv_t;
RESET ROLE;
-- REVOKE, grant options, CASCADE
SELECT relacl FROM pg_class WHERE relname = 'pv_t';
REVOKE INSERT ON pv_t FROM pv_writer;
REVOKE GRANT OPTION FOR UPDATE ON pv_t FROM pv_writer;
SELECT relacl FROM pg_class WHERE relname = 'pv_t';
REVOKE ALL ON pv_t FROM pv_other;
SELECT relacl FROM pg_class WHERE relname = 'pv_t';
SELECT attname, attacl FROM pg_attribute WHERE attrelid = 'pv_t'::regclass AND attnum > 0 ORDER BY attnum;
GRANT SELECT ON pv_t TO pv_writer WITH GRANT OPTION;
SET ROLE pv_writer;
GRANT SELECT ON pv_t TO pv_other;
RESET ROLE;
REVOKE SELECT ON pv_t FROM pv_writer;
REVOKE SELECT ON pv_t FROM pv_writer CASCADE;
SELECT relacl FROM pg_class WHERE relname = 'pv_t';
-- PUBLIC
GRANT SELECT ON pv_t TO PUBLIC;
SET ROLE pv_other;
SELECT count(*) FROM pv_t;
RESET ROLE;
REVOKE SELECT ON pv_t FROM PUBLIC;
-- Functions: EXECUTE is granted to PUBLIC by default
REVOKE EXECUTE ON FUNCTION pv_f(int) FROM PUBLIC;
SELECT proacl FROM pg_proc WHERE proname = 'pv_f';
SET ROLE pv_reader;
SELECT pv_f(1);
RESET ROLE;
GRANT EXECUTE ON FUNCTION pv_f(int) TO pv_group;
SET ROLE pv_reader;
SELECT pv_f(1);
RESET ROLE;
-- Sequences
GRANT USAGE ON SEQUENCE pv_seq TO pv_reader;
GRANT SELECT ON pv_seq TO pv_other;
GRANT EXECUTE ON SEQUENCE pv_seq TO pv_other;
SET ROLE pv_reader;
SELECT nextval('pv_seq');
SELECT setval('pv_seq', 10);
RESET ROLE;
SET ROLE pv_other;
SELECT last_value FROM pv_seq;
SELECT nextval('pv_seq');
RESET ROLE;
-- Schemas
CREATE SCHEMA pv_s;
CREATE TABLE pv_s.t (a int);
SET ROLE pv_reader;
SELECT * FROM pv_s.t;
CREATE TABLE pv_s.t2 (a int);
CREATE TABLE pv_mine (a int);
RESET ROLE;
GRANT USAGE, CREATE ON SCHEMA pv_s TO pv_reader;
SELECT nspname, nspacl FROM pg_namespace WHERE nspname IN ('pv_s', 'public') ORDER BY 1;
SET ROLE pv_reader;
SELECT * FROM pv_s.t;
CREATE TABLE pv_s.t2 (a int);
SELECT tableowner FROM pg_tables WHERE tablename = 't2';
DROP TABLE pv_s.t;
RESET ROLE;
-- ALL TABLES IN SCHEMA
GRANT SELECT ON ALL TABLES IN SCHEMA pv_s TO pv_other;
SELECT relname, relacl FROM pg_class WHERE relnamespace = 'pv_s'::regnamespace ORDER BY 1;
-- Default privileges
ALTER DEFAULT PRIVILEGES IN SCHEMA pv_s GRANT SELECT ON TABLES TO pv_group;
ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO pv_other;
ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
SELECT pg_get_userbyid(defaclrole), defaclnamespace::regnamespace, defaclobjtype, defaclacl FROM pg_default_acl ORDER BY 2, 3;
CREATE TABLE pv_s.t3 (a int);
CREATE FUNCTION pv_g() RETURNS int LANGUAGE sql AS 'SELECT 1';
SELECT relacl FROM pg_class WHERE relname = 't3';
SELECT proacl FROM pg_proc WHERE proname = 'pv_g';
ALTER DEFAULT PRIVILEGES IN SCHEMA pv_s REVOKE SELECT ON TABLES FROM pv_group;
ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM pv_other;
ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO PUBLIC;
SELECT count(*) FROM pg_default_acl;
-- Ownership
ALTER TABLE pv_t OWNER TO pv_owner;
SELECT relowner::regrole, relacl FROM pg_class WHERE relname = 'pv_t';
SET ROLE pv_reader;
ALTER TABLE pv_t ADD COLUMN x int;
DROP TABLE pv_t;
ALTER TABLE pv_t RENAME TO pv_t2;
CREATE INDEX ON pv_t (note);
TRUNCATE pv_t;
COMMENT ON TABLE pv_t IS 'no';
DROP FUNCTION pv_f(int);
DROP SCHEMA pv_s CASCADE;
DROP SEQUENCE pv_seq;
DROP VIEW pv_v;
LOCK TABLE pv_t IN ACCESS SHARE MODE;
CREATE SCHEMA pv_nope;
RESET ROLE;
GRANT pv_owner TO pv_reader;
SET ROLE pv_reader;
ALTER TABLE pv_t ADD COLUMN x int;
COMMENT ON TABLE pv_t IS 'mine';
RESET ROLE;
REVOKE pv_owner FROM pv_reader;
ALTER FUNCTION pv_f(int) OWNER TO pv_owner;
ALTER SEQUENCE pv_seq OWNER TO pv_owner;
ALTER VIEW pv_v OWNER TO pv_owner;
ALTER SCHEMA pv_s OWNER TO pv_owner;
SELECT proowner::regrole FROM pg_proc WHERE proname = 'pv_f';
SELECT relname, relowner::regrole FROM pg_class WHERE relname IN ('pv_seq', 'pv_v', 'pv_t') ORDER BY 1;
SELECT nspowner::regrole FROM pg_namespace WHERE nspname = 'pv_s';
-- information_schema
SELECT grantor, grantee, table_name, privilege_type, is_grantable, with_hierarchy FROM information_schema.table_privileges
WHERE table_schema IN ('public', 'pv_s') ORDER BY 3, 2, 4;
SELECT grantor, grantee, table_name, column_name, privilege_type, is_grantable FROM information_schema.column_privileges
WHERE table_schema = 'public' AND grantee <> 'pv_owner' ORDER BY 3, 4, 2, 5;
SELECT grantor, grantee, routine_name, privilege_type, is_grantable FROM information_schema.routine_privileges
WHERE routine_schema = 'public' ORDER BY 3, 2;
SELECT grantor, grantee, object_name, object_type, privilege_type, is_grantable FROM information_schema.usage_privileges
WHERE object_schema = 'public' ORDER BY 3, 2;
SELECT grantee, role_name, is_grantable FROM information_schema.applicable_roles WHERE role_name LIKE 'pv\_%' ORDER BY 1, 2;
SET ROLE pv_reader;
SELECT role_name FROM information_schema.enabled_roles ORDER BY 1;
SELECT grantee, table_name, privilege_type FROM information_schema.role_table_grants WHERE table_schema IN ('public', 'pv_s') ORDER BY 1, 2, 3;
RESET ROLE;
-- REASSIGN OWNED / DROP OWNED
REASSIGN OWNED BY pv_owner TO pv_writer;
SELECT relname, relowner::regrole FROM pg_class WHERE relname IN ('pv_seq', 'pv_v', 'pv_t') ORDER BY 1;
SELECT nspowner::regrole FROM pg_namespace WHERE nspname = 'pv_s';
DROP ROLE pv_writer;
DROP OWNED BY pv_writer, pv_reader, pv_other, pv_group;
SELECT count(*) FROM pg_class WHERE relname IN ('pv_seq', 'pv_v', 'pv_t');
BEGIN;
DROP OWNED BY pv_writer, pv_reader, pv_other, pv_group CASCADE;
SELECT count(*) FROM pg_class WHERE relname IN ('pv_seq', 'pv_v', 'pv_t', 't', 't2', 't3');
ROLLBACK;
SELECT count(*) FROM pg_namespace WHERE nspname = 'pv_s';
DROP TABLE IF EXISTS pv_mine;
DROP FUNCTION pv_g();
DROP ROLE pv_owner, pv_reader, pv_writer, pv_group, pv_other;
SELECT count(*) FROM pg_roles WHERE rolname LIKE 'pv\_%';
DROP OWNED BY pv_owner, pv_reader, pv_writer, pv_group, pv_other CASCADE;
DROP ROLE pv_owner, pv_reader, pv_writer, pv_group, pv_other;
SELECT count(*) FROM pg_roles WHERE rolname LIKE 'pv\_%';
