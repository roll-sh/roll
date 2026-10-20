-- Role administration: CREATEROLE / ADMIN OPTION checks, memberships, GRANTED BY,
-- revoking with dependent grants, passwords. Roles are cluster-wide: clean up first and last.
DROP ROLE IF EXISTS gd_ra_super, gd_ra_admin, gd_ra_limited, gd_ra_normal, gd_ra_made,
  gd_ra_made2, gd_ra_x, gd_ra_y, gd_ra_z, gd_ra_w, gd_ra_md5, gd_ra_scram, gd_ra_empty,
  gd_ra_renamed, gd_ra_group;
CREATE ROLE gd_ra_super SUPERUSER;
CREATE ROLE gd_ra_admin CREATEROLE CREATEDB;
CREATE ROLE gd_ra_limited CREATEROLE;
CREATE ROLE gd_ra_normal;
SET SESSION AUTHORIZATION gd_ra_limited;
-- needs SUPERUSER to touch SUPERUSER, attributes it lacks, ADMIN to alter others
CREATE ROLE gd_ra_x SUPERUSER;
CREATE ROLE gd_ra_x CREATEDB;
ALTER ROLE gd_ra_super NOLOGIN;
ALTER ROLE gd_ra_normal NOSUPERUSER;
ALTER ROLE gd_ra_normal NOINHERIT;
ALTER ROLE gd_ra_normal PASSWORD 'x';
COMMENT ON ROLE gd_ra_normal IS 'nope';
COMMENT ON ROLE gd_ra_super IS 'nope';
ALTER ROLE gd_ra_normal RENAME TO gd_ra_renamed;
DROP ROLE gd_ra_normal;
DROP ROLE gd_ra_super;
-- memberships named in CREATE ROLE are made once the role exists
CREATE ROLE gd_ra_x IN ROLE gd_ra_super;
CREATE ROLE gd_ra_x IN ROLE pg_database_owner;
CREATE ROLE gd_ra_x IN ROLE gd_ra_normal;
CREATE ROLE gd_ra_y ROLE gd_ra_y;
CREATE ROLE gd_ra_y ADMIN gd_ra_y;
-- a created role is administered by its creator (granted by the bootstrap superuser)
CREATE ROLE gd_ra_made;
SELECT r.rolname, m.rolname AS member, m2.rolname = current_user AS by_me, a.admin_option, a.inherit_option, a.set_option
  FROM pg_auth_members a JOIN pg_roles r ON r.oid = a.roleid JOIN pg_roles m ON m.oid = a.member
  JOIN pg_roles m2 ON m2.oid = a.grantor
 WHERE r.rolname = 'gd_ra_made' ORDER BY 2, 3;
COMMENT ON ROLE gd_ra_made IS 'mine';
ALTER ROLE gd_ra_made CONNECTION LIMIT 3;
ALTER ROLE gd_ra_made CREATEDB;
SET createrole_self_grant = 'set, inherit';
CREATE ROLE gd_ra_made2;
SELECT r.rolname, a.admin_option, a.inherit_option, a.set_option, g.rolname = 'gd_ra_limited' AS self_granted
  FROM pg_auth_members a JOIN pg_roles r ON r.oid = a.roleid JOIN pg_roles g ON g.oid = a.grantor
 WHERE r.rolname = 'gd_ra_made2' ORDER BY 2, 5;
RESET createrole_self_grant;
-- ALTER GROUP needs ADMIN OPTION on the group
ALTER GROUP gd_ra_made ADD USER gd_ra_made2;
ALTER GROUP gd_ra_made ADD USER gd_ra_made2;
ALTER GROUP gd_ra_normal ADD USER gd_ra_made2;
ALTER GROUP gd_ra_made DROP USER gd_ra_made2;
ALTER GROUP gd_ra_made DROP USER gd_ra_made2;
ALTER ROLE gd_ra_made RENAME TO gd_ra_renamed;
ALTER ROLE gd_ra_renamed RENAME TO gd_ra_made;
DROP ROLE gd_ra_made2, gd_ra_made2;
RESET SESSION AUTHORIZATION;
-- GRANTED BY and circular ADMIN OPTION grants
CREATE ROLE gd_ra_x;
CREATE ROLE gd_ra_y;
CREATE ROLE gd_ra_z;
CREATE ROLE gd_ra_w;
GRANT gd_ra_x TO gd_ra_y WITH ADMIN OPTION;
GRANT gd_ra_x TO gd_ra_z WITH ADMIN OPTION GRANTED BY gd_ra_y;
GRANT gd_ra_x TO gd_ra_y WITH ADMIN OPTION GRANTED BY gd_ra_z;
GRANT gd_ra_x TO gd_ra_w GRANTED BY gd_ra_w;
GRANT gd_ra_x TO gd_ra_w GRANTED BY gd_ra_super;
REVOKE ADMIN OPTION FOR gd_ra_x FROM gd_ra_y;
REVOKE gd_ra_x FROM gd_ra_y;
SELECT m.rolname AS member, g.rolname AS grantor, a.admin_option
  FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.grantor
 WHERE a.roleid = 'gd_ra_x'::regrole ORDER BY 1, 2;
BEGIN;
REVOKE ADMIN OPTION FOR gd_ra_x FROM gd_ra_y CASCADE;
SELECT m.rolname AS member, a.admin_option FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member
 WHERE a.roleid = 'gd_ra_x'::regrole ORDER BY 1;
ROLLBACK;
-- a role that granted a membership can't be dropped until the grant goes
DROP ROLE gd_ra_y;
REASSIGN OWNED BY gd_ra_y TO gd_ra_w;
DROP ROLE gd_ra_y;
DROP OWNED BY gd_ra_y;
SELECT count(*) FROM pg_auth_members WHERE grantor = 'gd_ra_y'::regrole;
DROP ROLE gd_ra_y;
-- inferred grantor: a role holding ADMIN OPTION that the user inherits from
CREATE ROLE gd_ra_y;
GRANT gd_ra_x TO gd_ra_y WITH ADMIN OPTION;
GRANT gd_ra_y TO gd_ra_z;
SET ROLE gd_ra_z;
GRANT gd_ra_x TO gd_ra_w;
RESET ROLE;
SELECT g.rolname AS grantor FROM pg_auth_members a JOIN pg_roles g ON g.oid = a.grantor
 WHERE a.roleid = 'gd_ra_x'::regrole AND a.member = 'gd_ra_w'::regrole ORDER BY 1;
-- passwords: password_encryption, hashes kept as they are, empty passwords
SET password_encryption = 'md5';
CREATE ROLE gd_ra_md5 PASSWORD 'pw1';
ALTER ROLE gd_ra_md5 PASSWORD 'md5e73a4b11df52a6068f8b39f90be36023';
SET password_encryption = 'scram-sha-256';
SET scram_iterations = 1024;
CREATE ROLE gd_ra_scram PASSWORD 'pw2';
CREATE ROLE gd_ra_empty PASSWORD '';
ALTER ROLE gd_ra_empty PASSWORD 'md561ca20d14261b22101b0cac7ba35336c';
SELECT rolname, regexp_replace(rolpassword, '(SCRAM-SHA-256)\$(\d+):([a-zA-Z0-9+/=]+)\$([a-zA-Z0-9+=/]+):([a-zA-Z0-9+/=]+)', '\1$\2:<salt>$<storedkey>:<serverkey>') AS pw
  FROM pg_authid WHERE rolname IN ('gd_ra_md5', 'gd_ra_scram', 'gd_ra_empty') ORDER BY 1;
ALTER ROLE gd_ra_md5 RENAME TO gd_ra_renamed;
SELECT rolpassword IS NULL FROM pg_authid WHERE rolname = 'gd_ra_renamed';
CREATE ROLE gd_ra_md5 PASSWORD 'SCRAM-SHA-256$000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004096:wNFxNSk1hAXBkgub8py3bg==$65zC6E+R0U7tiYTC9+Wtq4Thw6gUDj3eDCINij8TflU=:rC1I7tcVugrHEY2DT0iPjGyjM4aJxkMM9n8WBxtUtHU=';
RESET password_encryption;
RESET scram_iterations;
DROP ROLE gd_ra_super, gd_ra_admin, gd_ra_limited, gd_ra_normal, gd_ra_made,
  gd_ra_x, gd_ra_y, gd_ra_z, gd_ra_w, gd_ra_scram, gd_ra_empty, gd_ra_renamed;
DROP ROLE IF EXISTS gd_ra_made2;
