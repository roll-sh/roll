-- Client suites, round 5 (agent 100): numerics kept in decimal digits, information_schema
-- views generated from PostgreSQL's definitions kept per catalog version, and the planner's
-- shortcuts for expression equality.
-- Big numerics: text output and input, scaling, rounding, division, roots, the digits'
-- edges (base-10^9 limbs around 9, 18, 27 digits) and the on-disk boundary past i128.
SELECT length(repeat('9', 100000)::numeric::text), md5(repeat('12345', 20000)::numeric::text);
SELECT md5((repeat('7', 100000)::numeric * 3)::text), md5((repeat('7', 20000)::numeric * repeat('3', 15000)::numeric)::text);
SELECT md5((repeat('3', 20000)::numeric / repeat('7', 10000)::numeric)::text);
SELECT md5(factorial(3000)::text), factorial(0), factorial(1), factorial(33);
SELECT 999999999::numeric + 1, 999999999999999999::numeric + 1, 999999999999999999999999999::numeric + 1;
SELECT -1000000000::numeric - 1, 1e18::numeric - 1, 1e27 / 1e9, 1e36 % 999999999;
SELECT 170141183460469231731687303715884105727::numeric + 1, -170141183460469231731687303715884105728::numeric - 1;
SELECT 9223372036854775807::numeric * 9223372036854775807, (-9223372036854775808)::numeric * 2;
SELECT round(123456789012345678901234567890.123456789, 5), round(-0.5), round(2.5), round(-123456789.987654321, -4);
SELECT trunc(-123456789012345678901.999, 2), floor(-1e30 - 0.5), ceil(1e30 + 0.5), floor(-0.000000001), ceil(0.000000001);
SELECT trim_scale(1000000000.000000000000), trim_scale(0.000000000), min_scale(1e-20), scale(12.300000000000);
SELECT 1::numeric / 3, 2::numeric / 3, 1e30::numeric / 7, -1e-30 / 3, 10::numeric / 4, 1.5 * 7 / 7;
SELECT div(1e40, 7), mod(-1e40, 7), div(-17, 5), mod(17.5, -5), 1e40::numeric % 1e20;
SELECT sqrt(2::numeric), sqrt(1e100), sqrt(123456789012345678901234567890.5), sqrt(0.0000000002);
SELECT exp(1::numeric), ln(1e100), log(2, 1e50), power(2::numeric, 200), power(1.0001, 10000.5);
SELECT gcd(1e40::numeric, 1e30 + 10), lcm(123456789012345678901234567890, 987654321);
SELECT x::numeric(40, 10) FROM (VALUES ('12345678901234567890.0123456789012'), ('-0.00000000005')) v(x);
SELECT '1e131071'::numeric > 0, length('-1e-16383'::numeric::text), '0.0000000001e10'::numeric;
SELECT (repeat('1', 50) || '.' || repeat('2', 50))::numeric::float8, 1e300::numeric::float8, 123456789012345678901234567890::numeric::float4;
SELECT sum(x), avg(x), stddev(x)::numeric(30, 20) FROM (SELECT (g::numeric * 1e25 + 0.5) AS x FROM generate_series(1, 1000) g) s;
SELECT sum(x::numeric * 1.5 / 7) FROM generate_series(1, 2000) x;
SELECT to_char(123456789012345678901234567890.12345::numeric, 'FM999,999,999,999,999,999,999,999,999,999.99999');
SELECT hash_numeric(1e40) = hash_numeric(10000000000000000000000000000000000000000.000), hash_numeric(-1e-40), hash_numeric_extended(123456789012345678901234567890, 7);
-- Values past i128 stored and read back, indexed and hashed.
CREATE TABLE a100_big (k numeric PRIMARY KEY, v numeric);
INSERT INTO a100_big SELECT g * 1e40 + g, -(g * 1e45) FROM generate_series(1, 50) g;
INSERT INTO a100_big VALUES (repeat('9', 1000)::numeric, 1), (-1e100, 0.000000000000000000000000000000000000000001);
SELECT count(*), sum(k), min(v), max(k) - min(k) FROM a100_big;
SELECT k, v FROM a100_big WHERE k = 7e40 + 7;
SELECT count(DISTINCT v), count(*) FILTER (WHERE k > 1e41) FROM a100_big;
SELECT v FROM a100_big ORDER BY v LIMIT 3;
DROP TABLE a100_big;

-- information_schema.routines / parameters (PostgreSQL's view definitions) follow DDL and
-- the role reading them.
CREATE SCHEMA a100_s;
CREATE FUNCTION a100_s.f(a int, OUT b text) LANGUAGE sql AS 'SELECT a::text';
SELECT routine_name, data_type FROM information_schema.routines WHERE routine_schema = 'a100_s';
SELECT parameter_name, data_type, parameter_mode, ordinal_position FROM information_schema.parameters
  WHERE specific_schema = 'a100_s' ORDER BY ordinal_position;
CREATE OR REPLACE FUNCTION a100_s.f(a int, OUT b text, OUT c date) LANGUAGE sql AS 'SELECT a::text, current_date';
SELECT parameter_name, data_type, parameter_mode FROM information_schema.parameters
  WHERE specific_schema = 'a100_s' ORDER BY ordinal_position;
CREATE ROLE a100_r;
REVOKE EXECUTE ON FUNCTION a100_s.f(int) FROM PUBLIC;
GRANT USAGE ON SCHEMA a100_s TO a100_r;
SET ROLE a100_r;
SELECT count(*) FROM information_schema.routines WHERE routine_schema = 'a100_s';
RESET ROLE;
SELECT count(*) FROM information_schema.routines WHERE routine_schema = 'a100_s';
BEGIN;
DROP FUNCTION a100_s.f(int);
SELECT count(*) FROM information_schema.parameters WHERE specific_schema = 'a100_s';
ROLLBACK;
SELECT count(*) FROM information_schema.parameters WHERE specific_schema = 'a100_s';
DROP SCHEMA a100_s CASCADE;
DROP ROLE a100_r;

-- Catalog joins whose clauses equate subscripts (pgjdbc's getImportedKeys).
CREATE TABLE a100_p (a int, b int, PRIMARY KEY (a, b));
CREATE TABLE a100_c (x int, y int, z int, FOREIGN KEY (y, z) REFERENCES a100_p);
SELECT pkc.relname, pka.attname, fkc.relname, fka.attname, pos.n
  FROM pg_namespace pkn, pg_class pkc, pg_attribute pka, pg_namespace fkn, pg_class fkc,
    pg_attribute fka, pg_constraint con, generate_series(1, 32) pos(n), pg_class pkic
  WHERE pkn.oid = pkc.relnamespace AND pkc.oid = pka.attrelid AND pka.attnum = con.confkey[pos.n]
    AND con.confrelid = pkc.oid AND fkn.oid = fkc.relnamespace AND fkc.oid = fka.attrelid
    AND fka.attnum = con.conkey[pos.n] AND con.conrelid = fkc.oid AND con.contype = 'f'
    AND (pkic.relkind = 'i' OR pkic.relkind = 'I') AND pkic.oid = con.conindid
    AND fkc.relname LIKE 'a100%'
  ORDER BY pkn.nspname, pkc.relname, con.conname, pos.n;
DROP TABLE a100_c, a100_p;
