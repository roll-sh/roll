-- Roles are cluster-wide in Postgres: clean up first and at the end.
DROP ROLE IF EXISTS gp_admin, gp_alice, gp_bob, gp_carol, gp_group, gp_sub, gp_x, gp_y;
CREATE ROLE gp_admin CREATEROLE;
CREATE ROLE gp_alice LOGIN PASSWORD 'secret' CONNECTION LIMIT 5 VALID UNTIL '2030-01-01 00:00:00+00';
CREATE USER gp_bob NOINHERIT;
CREATE ROLE gp_group NOLOGIN;
CREATE ROLE gp_alice;
CREATE ROLE public;
CREATE ROLE pg_foo;
CREATE ROLE gp_x SUPERUSER SUPERUSER;
CREATE ROLE gp_x WITH FOO;
SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication, rolconnlimit, rolpassword, rolvaliduntil, rolbypassrls, rolconfig
FROM pg_roles WHERE rolname LIKE 'gp\_%' ORDER BY 1;
SELECT usename, usecreatedb, usesuper FROM pg_user WHERE usename LIKE 'gp\_%' ORDER BY 1;
GRANT gp_group TO gp_alice;
GRANT gp_group TO gp_alice;
GRANT gp_group TO gp_bob WITH ADMIN OPTION, INHERIT TRUE;
GRANT gp_alice TO gp_group;
GRANT gp_group TO gp_group;
GRANT pg_database_owner TO gp_alice;
GRANT gp_group TO gp_carol;
GRANT gp_nobody TO gp_alice;
GRANT gp_group TO gp_alice WITH FOO TRUE;
SELECT r.rolname AS role, m.rolname AS member, g.rolname AS grantor, admin_option, inherit_option, set_option
FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member JOIN pg_roles g ON g.oid = am.grantor
WHERE r.rolname LIKE 'gp\_%' ORDER BY 1, 2, 3;
SELECT pg_has_role('gp_alice', 'gp_group', 'MEMBER'), pg_has_role('gp_alice', 'gp_group', 'USAGE'), pg_has_role('gp_bob', 'gp_group', 'USAGE'),
       pg_has_role('gp_bob', 'gp_group', 'MEMBER WITH ADMIN OPTION'), pg_has_role('gp_alice', 'gp_group', 'SET'), pg_has_role('gp_alice', 'gp_bob', 'MEMBER');
SELECT pg_has_role('gp_alice', 'gp_group', 'FOO');
SELECT pg_has_role('nobody', 'gp_group', 'MEMBER');
-- SET ROLE / SESSION AUTHORIZATION
SET ROLE gp_alice;
SELECT current_user, session_user, current_role;
SHOW role;
SHOW is_superuser;
SET ROLE gp_group;
SELECT current_user;
SET ROLE gp_bob;
CREATE ROLE gp_y;
RESET ROLE;
SELECT current_user;
SET SESSION AUTHORIZATION gp_bob;
SELECT current_user, session_user;
SHOW session_authorization;
SET ROLE gp_group;
SELECT current_user, session_user;
SET ROLE gp_alice;
SET SESSION AUTHORIZATION gp_alice;
RESET SESSION AUTHORIZATION;
SELECT current_user, session_user;
SHOW role;
BEGIN;
SET LOCAL ROLE gp_alice;
SELECT current_user;
COMMIT;
SELECT current_user;
BEGIN;
SET ROLE gp_alice;
ROLLBACK;
SELECT current_user;
SET ROLE nobody;
-- CREATEROLE users administer the roles they create
SET ROLE gp_admin;
CREATE ROLE gp_sub;
CREATE ROLE gp_y SUPERUSER;
CREATE ROLE gp_y CREATEDB;
GRANT gp_sub TO gp_alice;
GRANT gp_group TO gp_sub;
ALTER ROLE gp_sub CONNECTION LIMIT 3;
ALTER ROLE gp_alice CONNECTION LIMIT 3;
RESET ROLE;
SELECT r.rolname AS role, m.rolname AS member, g.rolname AS grantor, admin_option, inherit_option, set_option
FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member JOIN pg_roles g ON g.oid = am.grantor
WHERE r.rolname LIKE 'gp\_%' ORDER BY 1, 2, 3;
SELECT rolname, rolconnlimit FROM pg_roles WHERE rolname = 'gp_sub';
-- REVOKE role
REVOKE gp_group FROM gp_carol;
REVOKE gp_sub FROM gp_bob;
REVOKE ADMIN OPTION FOR gp_group FROM gp_bob;
SELECT r.rolname AS role, m.rolname AS member, admin_option FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member
WHERE r.rolname = 'gp_group' ORDER BY 1, 2;
GRANT gp_group TO gp_bob WITH ADMIN OPTION;
SET ROLE gp_bob;
GRANT gp_group TO gp_sub;
RESET ROLE;
REVOKE ADMIN OPTION FOR gp_group FROM gp_bob;
REVOKE ADMIN OPTION FOR gp_group FROM gp_bob CASCADE;
SELECT r.rolname AS role, m.rolname AS member, g.rolname AS grantor, admin_option FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member JOIN pg_roles g ON g.oid = am.grantor
WHERE r.rolname = 'gp_group' ORDER BY 1, 2, 3;
-- ALTER ROLE
ALTER ROLE gp_alice NOLOGIN CREATEDB;
ALTER ROLE gp_alice SET search_path = a, b;
ALTER ROLE gp_alice SET work_mem = '64MB';
ALTER ROLE gp_alice SET no_such_setting = 1;
ALTER ROLE gp_alice IN DATABASE postgres SET work_mem = '1MB';
SELECT rolname, rolcanlogin, rolcreatedb, rolconfig FROM pg_roles WHERE rolname = 'gp_alice';
ALTER ROLE gp_alice RESET work_mem;
SELECT rolconfig FROM pg_roles WHERE rolname = 'gp_alice';
ALTER ROLE gp_alice RESET ALL;
SELECT rolconfig FROM pg_roles WHERE rolname = 'gp_alice';
ALTER ROLE nobody LOGIN;
-- DROP ROLE
DROP ROLE gp_nobody;
DROP ROLE IF EXISTS gp_nobody;
DROP ROLE postgres;
DROP ROLE pg_monitor;
SET ROLE gp_alice;
DROP ROLE gp_bob;
RESET ROLE;
DROP ROLE gp_sub, gp_carol;
DROP ROLE IF EXISTS gp_sub, gp_carol;
DROP ROLE gp_admin, gp_alice, gp_bob, gp_group;
DROP ROLE IF EXISTS gp_admin, gp_alice, gp_bob, gp_group, gp_sub, gp_x, gp_y;
SELECT count(*) FROM pg_roles WHERE rolname LIKE 'gp\_%';
SELECT count(*) FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid WHERE r.rolname LIKE 'gp\_%';
