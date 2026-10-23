-- A made cast stops counting once it is dropped (or rolled back): statements that resolve
-- functions without binding a query (CREATE AGGREGATE's support functions) don't see the cast
-- an earlier statement's binder saw.
CREATE TYPE t1 AS (a int);
CREATE TYPE t2 AS (a int);
CREATE FUNCTION t1_t2(t1) RETURNS t2 LANGUAGE sql AS 'SELECT ROW($1.a)::t2';
CREATE FUNCTION sf(t2, int) RETURNS t2 LANGUAGE sql AS 'SELECT $1';
CREATE CAST (t1 AS t2) WITH FUNCTION t1_t2(t1) AS IMPLICIT;
SELECT 1;
CREATE AGGREGATE ag1(int) (sfunc = sf, stype = t1);
DROP CAST (t1 AS t2);
CREATE AGGREGATE ag2(int) (sfunc = sf, stype = t1);
BEGIN;
CREATE CAST (t1 AS t2) WITH FUNCTION t1_t2(t1) AS IMPLICIT;
SELECT 2;
ROLLBACK;
CREATE AGGREGATE ag3(int) (sfunc = sf, stype = t1);
SELECT aggfnoid::regproc FROM pg_aggregate WHERE aggfnoid::regproc::text LIKE 'ag%' ORDER BY 1;
