-- Query semantics fixed in the regression sweep: operators over domains, CASE with a
-- user operator, target-list SRFs after ORDER BY / DISTINCT ON, JSON_VALUE RETURNING a
-- collated domain, placeholder settings, INSTEAD rules with RETURNING.
CREATE DOMAIN gsw_foodomain AS text;
CREATE FUNCTION gsw_volfoo(text) RETURNS gsw_foodomain AS 'begin return $1::gsw_foodomain; end' LANGUAGE plpgsql VOLATILE;
CREATE FUNCTION gsw_inline_eq(gsw_foodomain, gsw_foodomain) RETURNS boolean AS 'SELECT CASE $2::text WHEN $1::text THEN true ELSE false END' LANGUAGE sql;
CREATE OPERATOR = (PROCEDURE = gsw_inline_eq, LEFTARG = gsw_foodomain, RIGHTARG = gsw_foodomain);
SELECT CASE gsw_volfoo('bar') WHEN 'foo'::gsw_foodomain THEN 'is foo' ELSE 'is not foo' END;
SELECT CASE gsw_volfoo('foo') WHEN 'foo'::gsw_foodomain THEN 'is foo' ELSE 'is not foo' END;
SELECT gsw_volfoo('bar') = 'bar'::gsw_foodomain, 'a'::text = 'a'::text;
DROP OPERATOR = (gsw_foodomain, gsw_foodomain);
-- SRFs in the select list run after the sort when no key needs them
SELECT a, generate_series(1, 3) AS g FROM (VALUES (3), (1), (2)) t(a) ORDER BY a DESC;
SELECT DISTINCT ON (a) a, b, generate_series(1, 3) AS g FROM (VALUES (3, 2), (3, 1), (1, 1), (1, 4), (5, 3), (5, 1)) AS t(a, b) ORDER BY a, b DESC;
SELECT DISTINCT ON (a) a, b, generate_series(1, 3) AS g FROM (VALUES (3, 2), (3, 1), (1, 1), (1, 4), (5, 3), (5, 1)) AS t(a, b) ORDER BY a, b DESC, g DESC;
SELECT DISTINCT ON (a) a, b, generate_series(1, 3) AS g FROM (VALUES (3, 2), (3, 1), (1, 1), (1, 4), (5, 3), (5, 1)) AS t(a, b);
SELECT a, generate_series(1, 2) AS g FROM (VALUES (3), (1), (2)) t(a) ORDER BY a LIMIT 3;
SELECT a, generate_series(1, 2) AS g FROM (VALUES (3), (1), (2)) t(a) ORDER BY g, a;
SELECT a, generate_series(1, 2) AS g, random() < 2 AS r FROM (VALUES (2), (1)) t(a) ORDER BY a;
-- JSON_VALUE RETURNING a domain with a collation
CREATE COLLATION gsw_ci (provider = icu, locale = '@colStrength=secondary', deterministic = false);
CREATE DOMAIN gsw_d1 AS text COLLATE gsw_ci;
CREATE DOMAIN gsw_d2 AS text COLLATE "C";
SELECT JSON_VALUE('{"a": "A"}', '$.a' RETURNING gsw_d1) = 'a';
SELECT JSON_VALUE('{"a": "A"}', '$.a' RETURNING gsw_d1 DEFAULT 'C' ON EMPTY) = 'a';
SELECT JSON_VALUE('{"a": "A"}', '$.a' RETURNING gsw_d1 DEFAULT 'C'::gsw_d2 ON EMPTY) = 'a';
SELECT JSON_VALUE('{"a": "A"}', '$.a' RETURNING gsw_d1 DEFAULT 'C' COLLATE "C" ON EMPTY) = 'a';
SELECT JSON_VALUE('{"a": "A"}', '$.c' RETURNING gsw_d1 DEFAULT 'A' ON EMPTY) = 'a';
SELECT JSON_VALUE('{"a": "A"}', '$.c' RETURNING gsw_d1 DEFAULT ('A' COLLATE "C") COLLATE gsw_ci ON EMPTY) = 'a';
-- a placeholder setting stays once made, empty
SELECT set_config('gsw.blah', 'x', true);
SELECT current_setting('gsw.blah') = '' AS empty;
BEGIN;
SET LOCAL gsw.other = 'y';
COMMIT;
SHOW gsw.other;
SELECT current_setting('gsw.missing', true) IS NULL AS missing;
-- an aggregate's ORDER BY key whose collations conflict
CREATE TABLE gsw_ct10 (a int, x text COLLATE "C", y text COLLATE "POSIX");
SELECT array_agg(a ORDER BY x || y) FROM gsw_ct10;
-- INSTEAD rules doing another command, with RETURNING
CREATE TABLE gsw_foo (f1 int, f2 text, f3 int, f4 int);
INSERT INTO gsw_foo VALUES (4, 'x', 1, 2), (5, 'y', 3, 4);
CREATE RULE gsw_del_rule AS ON DELETE TO gsw_foo DO INSTEAD
  UPDATE gsw_foo SET f2 = f2 || ' (deleted)', f3 = -1, f4 = -1 WHERE f1 = OLD.f1 RETURNING *;
DELETE FROM gsw_foo WHERE f1 = 4 RETURNING *;
DELETE FROM gsw_foo WHERE f1 = 5 RETURNING old.*, new.*;
DELETE FROM gsw_foo WHERE f1 = 6 RETURNING old.f1;
SELECT * FROM gsw_foo ORDER BY f1;
CREATE TABLE gsw_foo3 (f1 int, f2 text);
CREATE TABLE gsw_log3 (f1 int, f2 text);
INSERT INTO gsw_foo3 VALUES (4, 'x');
CREATE RULE gsw_del_rule3 AS ON DELETE TO gsw_foo3 DO INSTEAD INSERT INTO gsw_log3 VALUES (old.f1, old.f2) RETURNING *;
DELETE FROM gsw_foo3 WHERE f1 = 4 RETURNING *;
SELECT * FROM gsw_log3;
-- a rule action with data-modifying WITH queries prints back
CREATE TABLE gsw_rs (f1 int, f2 int DEFAULT 0);
CREATE TABLE gsw_rl (f1 int, f2 int);
CREATE TABLE gsw_i4 (f1 int);
CREATE RULE gsw_r7 AS ON DELETE TO gsw_rs DO INSTEAD
  WITH wins AS (INSERT INTO gsw_i4 AS trgt VALUES (0) RETURNING *),
       wupd AS (UPDATE gsw_i4 trgt SET f1 = f1 + 1 RETURNING *),
       wdel AS (DELETE FROM gsw_i4 trgt WHERE f1 = 0 RETURNING *)
  INSERT INTO gsw_rl AS trgt SELECT old.* FROM wins, wupd, wdel
  RETURNING trgt.f1, trgt.f2;
SELECT pg_get_ruledef(oid, true) FROM pg_rewrite WHERE rulename = 'gsw_r7';
-- pg_read_file checks its length before the file
SELECT pg_read_file('does not exist', 0, -1);
SELECT pg_read_binary_file('does not exist', 0, -1, true);
-- a ROW's fields, and a sublink's columns, keep their own collations
CREATE TABLE gsw_c10 (a int, x text COLLATE "C", y text COLLATE "POSIX");
INSERT INTO gsw_c10 VALUES (1, 'a', 'b');
SELECT * FROM gsw_c10 WHERE (x COLLATE "POSIX", y COLLATE "C") NOT IN (SELECT y, x FROM gsw_c10);
SELECT * FROM gsw_c10 WHERE (x COLLATE "POSIX", y COLLATE "C") IN (SELECT y, x FROM gsw_c10);
SELECT * FROM gsw_c10 WHERE (x COLLATE "POSIX", y COLLATE "C") = (y, x);
SELECT (x COLLATE "POSIX", y COLLATE "C") IN (SELECT y, x FROM gsw_c10) FROM gsw_c10;
-- columns an ORDER BY of a set operation or VALUES list can't reference
CREATE TABLE gsw_i8 (q1 int8, q2 int8);
SELECT q1 FROM gsw_i8 EXCEPT SELECT q2 FROM gsw_i8 ORDER BY q2 LIMIT 1;
SELECT q1 FROM gsw_i8 UNION SELECT q2 FROM gsw_i8 ORDER BY q3;
VALUES (1) ORDER BY colum1;
-- a shell type has no array type
CREATE TYPE gsw_myshell;
SELECT pg_identify_object_as_address('pg_type'::regclass, 0, 0);
SELECT pg_identify_object('pg_type'::regclass, 0, 0);
