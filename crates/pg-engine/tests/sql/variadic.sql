CREATE FUNCTION vsum(VARIADIC xs int[]) RETURNS int LANGUAGE sql AS 'SELECT sum(x)::int FROM unnest(xs) x';
SELECT vsum(1, 2, 3), vsum(VARIADIC ARRAY[4, 5]), vsum(10);
SELECT vsum();
SELECT vsum(VARIADIC 1);
SELECT vsum(VARIADIC NULL::int[]) IS NULL;
SELECT vsum(xs => ARRAY[1, 2]);
SELECT vsum(VARIADIC xs => ARRAY[3, 4]);
CREATE FUNCTION vfmt(fmt text, VARIADIC args text[]) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN fmt || ':' || array_to_string(args, ','); END $$;
SELECT vfmt('x', 'a', 'b'), vfmt('y', VARIADIC '{c,d}'), vfmt('z', 'only');
SELECT vfmt('x');
SELECT concat_ws('-', VARIADIC ARRAY['a', 'b']);
SELECT format('%s+%s', VARIADIC ARRAY['1', '2']);
SELECT proname, provariadic::regtype, pg_get_function_arguments(oid), pg_get_function_identity_arguments(oid), proargmodes FROM pg_proc WHERE proname IN ('vsum', 'vfmt') ORDER BY 1;
CREATE FUNCTION vany(VARIADIC "any") RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION vbad(VARIADIC xs int) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION vbad2(VARIADIC xs int[], y int) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION vdef(a int, VARIADIC rest int[] DEFAULT '{}') RETURNS int LANGUAGE sql AS 'SELECT a + coalesce(array_length(rest, 1), 0)';
SELECT vdef(1), vdef(1, 5, 6);
CREATE FUNCTION vpoly(VARIADIC anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT $1[array_upper($1, 1)]';
SELECT vpoly(1, 2, 3), vpoly('a'::text, 'b');
CREATE PROCEDURE vproc(VARIADIC xs int[]) LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'got %', xs; END $$;
CALL vproc(1, 2);
DROP FUNCTION vsum(int[]);
DROP FUNCTION vfmt(text, text[]);
DROP FUNCTION vdef, vpoly;
DROP PROCEDURE vproc(VARIADIC int[]);
SELECT count(*) FROM pg_proc WHERE proname LIKE 'v%' AND pronamespace = 'public'::regnamespace;

-- multiple-column assignment and RETURNING
CREATE TABLE mc (a int, b int[], c text);
INSERT INTO mc VALUES (1, '{1,2}', 'x');
INSERT INTO mc VALUES (1, '{1,2}', 'x') RETURNING generate_series(1, 2);
UPDATE mc SET (a, b) = ROW(2, '{3}') RETURNING *;
UPDATE mc SET (a, b) = (SELECT 3, '{4}'::int[]) RETURNING *;
UPDATE mc SET (a) = ROW(5) RETURNING *;
UPDATE mc SET (a, b) = ROW(6) RETURNING *;
UPDATE mc SET (a, c) = (a + 1, DEFAULT) RETURNING *;
UPDATE mc SET (a, b[1]) = (10, 20) RETURNING *;
SELECT (mc.b)[1], mc.b[1:1] FROM mc;
SELECT (ARRAY[ROW(1, 2)])[1].f1;
SELECT current_role, current_catalog, current_schema, user, session_user, current_user;
SELECT 1 OPERATOR(pg_catalog.+) 2, 2 OPERATOR(+) 3;
SELECT 2 ^ 3, |/ 16, ||/ 27, @ -5, ~ 5;
DROP TABLE mc;

-- builtin VARIADIC calls
SELECT concat(VARIADIC NULL::text[]) IS NULL, concat_ws(',', VARIADIC NULL::text[]) IS NULL, format('x', VARIADIC NULL::text[]);
SELECT concat(VARIADIC '{}'::text[]), json_build_array(VARIADIC '{}'::int[]), num_nulls(VARIADIC ARRAY[1, NULL]), format('%s %s', VARIADIC ARRAY[1, 2]), concat(VARIADIC ARRAY[1, 2]);
SELECT format('%s', VARIADIC ARRAY[[1, 2], [3, 4]]), length(VARIADIC 'abc');
SELECT concat(VARIADIC 1);
SELECT concat();
SELECT json_build_object(VARIADIC ARRAY['a', '1']);
CREATE VIEW vv AS SELECT concat('a', 'b') AS c1, concat(VARIADIC ARRAY['x', 'y']) AS c2;
SELECT pg_get_viewdef('vv');
DROP VIEW vv;

-- polymorphic SQL functions and parameter display
CREATE FUNCTION pid(x anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT x';
SELECT pid(1), pid('a'::text), pid(2.5);
CREATE FUNCTION pfirst(xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';
SELECT pfirst(ARRAY[3, 4]), pfirst(ARRAY['p', 'q']);
CREATE FUNCTION zz1(a int, INOUT b int, OUT c int, VARIADIC d int[]) AS 'SELECT 1, 2' LANGUAGE sql;
CREATE PROCEDURE zz2(a int, INOUT b int, OUT c int) LANGUAGE sql AS 'SELECT 1, 2';
CREATE FUNCTION zz3(a int) RETURNS TABLE(x int, y int) LANGUAGE sql AS 'SELECT 1, 2';
CREATE FUNCTION zz4(a int, b int DEFAULT 3, OUT c int) LANGUAGE sql AS 'SELECT 1';
SELECT proname, pg_get_function_arguments(oid), pg_get_function_identity_arguments(oid), proargmodes, proargnames, proallargtypes, pronargs, pronargdefaults, proargtypes FROM pg_proc WHERE proname LIKE 'zz%' ORDER BY 1;
CREATE FUNCTION zz5(VARIADIC "any") RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE PROCEDURE zz6(VARIADIC a int[], OUT b int) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION zz7(VARIADIC a int[], VARIADIC b int[]) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION zz8(a int[]) RETURNS int LANGUAGE sql AS 'SELECT 1';
SELECT zz8(VARIADIC ARRAY[1]);
DROP FUNCTION zz1, zz3, zz4, zz8, pid, pfirst;
DROP PROCEDURE zz2;

-- field selection on records
CREATE TABLE zt (a int, b text);
INSERT INTO zt VALUES (1, 'x');
SELECT (array_agg(zt))[1].a FROM zt;
SELECT (ARRAY[zt])[1].b FROM zt;
SELECT (array_agg(s))[1].n FROM (SELECT 1 AS n) s;
SELECT (row(1, 2)).f1, (x).f2 FROM (SELECT row(1, 2) x) s;
UPDATE zt SET (a, b) = (SELECT 1);
DROP TABLE zt;
