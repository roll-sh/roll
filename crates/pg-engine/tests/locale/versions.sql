-- Collation versions: recorded with the database (COLLATION_VERSION, or the provider's),
-- checked at connection (CheckMyDatabase) and when copying a template, refreshed by
-- ALTER DATABASE ... REFRESH COLLATION VERSION; a default collation that can't open.
CREATE DATABASE locale_v_icu LOCALE_PROVIDER icu ICU_LOCALE 'en' COLLATION_VERSION '42' TEMPLATE template0;
CREATE DATABASE locale_v_libc COLLATION_VERSION '42' TEMPLATE template0;
CREATE DATABASE locale_v_builtin LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' COLLATION_VERSION '0' TEMPLATE template0;
CREATE DATABASE locale_v_rules LOCALE_PROVIDER icu ICU_LOCALE 'en' ICU_RULES 'garbage &&' TEMPLATE template0;
SELECT datname, datcollversion, pg_database_collation_actual_version(oid) FROM pg_database WHERE datname LIKE 'locale%' ORDER BY datname;
CREATE DATABASE locale_v_copy TEMPLATE locale_v_icu;
CREATE DATABASE locale_v_copy2 TEMPLATE locale_v_icu COLLATION_VERSION '43';
CREATE DATABASE locale_v_copy3 TEMPLATE locale_v_libc;
\c locale_v_icu
SELECT 1;
\c locale_v_libc
SELECT 1;
\c locale_v_builtin
SELECT pg_collation_actual_version(100);
\c locale_v_rules
\c locale_v_copy2
SELECT datcollversion FROM pg_database WHERE datname = current_database();
\c postgres
ALTER DATABASE locale_v_icu REFRESH COLLATION VERSION;
ALTER DATABASE locale_v_icu REFRESH COLLATION VERSION;
ALTER DATABASE locale_v_libc REFRESH COLLATION VERSION;
ALTER DATABASE locale_v_builtin REFRESH COLLATION VERSION;
ALTER DATABASE postgres REFRESH COLLATION VERSION;
ALTER DATABASE locale_nosuch REFRESH COLLATION VERSION;
SELECT datname, datcollversion FROM pg_database WHERE datname LIKE 'locale%' ORDER BY datname;
CREATE DATABASE locale_v_copy4 TEMPLATE locale_v_icu;
\c locale_v_icu
SELECT datcollversion FROM pg_database WHERE datname = current_database();
\c locale_v_builtin
SELECT 1;
\c postgres
CREATE ROLE locale_r;
SET ROLE locale_r;
ALTER DATABASE locale_v_icu REFRESH COLLATION VERSION;
RESET ROLE;
DROP ROLE locale_r;
