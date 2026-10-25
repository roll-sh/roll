-- Round 17 (agent 37), what psql's backslash commands read: ALTER DATABASE ... SET is kept
-- (pg_db_role_setting, \drds) and sessions there start with it, pg_tablespace_size and
-- pg_database_size(oid) exist, format_type(0) is '-' (\do of a shell operator), a domain
-- CHECK's VALUE subscripted prints as (VALUE)[1] (\dD), a range type's constructors have
-- arguments and a result (\df), pg_get_ruledef prints conditions with subqueries and
-- actions writing views.
-- (ALTER DATABASE ... SET itself: tests/wire.rs database_settings, CREATE DATABASE needing a
-- cluster.)
ALTER DATABASE a37_nosuchdb SET work_mem = '1MB';
SELECT pg_tablespace_size('pg_default') > 0, pg_tablespace_size('pg_global') >= 0,
  pg_tablespace_size(1663) > 0;
SELECT pg_tablespace_size(12345);
SELECT pg_tablespace_size('a37_nosuch');
SELECT pg_database_size(current_database()) > 0,
  pg_database_size((SELECT oid FROM pg_database WHERE datname = current_database())) > 0,
  true;
SELECT pg_database_size(12345);
SELECT pg_database_size('a37_nosuchdb');
SELECT format_type(0, NULL), format_type(0, -1), format_type(99999, NULL);
CREATE DOMAIN a37_pair AS int[] CHECK (VALUE[1] < VALUE[2]);
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE contypid = 'a37_pair'::regtype;
DROP DOMAIN a37_pair;
CREATE TYPE a37_ar AS RANGE (subtype = int4[]);
SELECT proname, pg_get_function_arguments(p.oid), pg_get_function_identity_arguments(p.oid),
  pg_get_function_result(p.oid) FROM pg_proc p WHERE proname IN ('a37_ar', 'a37_armultirange')
  ORDER BY 1, 2;
DROP TYPE a37_ar;
-- pg_get_ruledef: a condition with a subquery, an action writing a view.
CREATE TABLE a37_sl (sl_name char(10), sl_avail int);
CREATE VIEW a37_slv AS SELECT * FROM a37_sl;
CREATE TABLE a37_ok (ok_name char(10), ok_quant int);
CREATE RULE a37_r1 AS ON INSERT TO a37_ok DO INSTEAD
  UPDATE a37_slv SET sl_avail = sl_avail + new.ok_quant WHERE sl_name = new.ok_name;
CREATE TABLE a37_t3 (id3a int, data text);
CREATE RULE a37_r3 AS ON INSERT TO a37_t3
  WHERE (EXISTS (SELECT 1 FROM a37_t3 WHERE a37_t3.id3a = new.id3a))
  DO INSTEAD UPDATE a37_t3 SET data = new.data WHERE a37_t3.id3a = new.id3a;
SELECT rulename, pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename LIKE 'a37_r%' ORDER BY 1;
SELECT rulename, pg_get_ruledef(oid, true) FROM pg_rewrite WHERE rulename LIKE 'a37_r%' ORDER BY 1;
DROP TABLE a37_ok, a37_t3;
DROP VIEW a37_slv;
DROP TABLE a37_sl;
-- A domain-typed column without COLLATE has the domain's collation (pg_dump adds no COLLATE).
CREATE DOMAIN a37_cd AS text COLLATE "POSIX";
CREATE TABLE a37_dc (c information_schema.character_data, y information_schema.yes_or_no,
  d a37_cd, e a37_cd COLLATE "C", n information_schema.cardinal_number);
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 'a37_dc'::regclass AND attnum > 0 ORDER BY attnum;
DROP TABLE a37_dc;
DROP DOMAIN a37_cd;
