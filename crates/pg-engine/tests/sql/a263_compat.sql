-- Differences the frameworks and tools found (agent 263, tools/compat): each case is what one
-- of them reads.
-- pg_dump writes COLLATE on a column whose attcollation isn't its type's typcollation: a
-- domain over citext has citext's collation, so its column has none written.
CREATE EXTENSION citext;
CREATE DOMAIN a263_email AS citext CHECK (VALUE ~ '@');
CREATE DOMAIN a263_ci AS citext COLLATE "C";
CREATE TABLE a263_people (e a263_email, c a263_ci, n citext);
SELECT typname, typcollation::regcollation FROM pg_type WHERE typname IN ('a263_email', 'a263_ci', '_a263_email') ORDER BY 1;
SELECT a.attname, a.attcollation = t.typcollation AS same FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
  WHERE a.attrelid = 'a263_people'::regclass AND a.attnum > 0 ORDER BY a.attnum;
-- COPY TO writes reg* columns as their output functions do (pg_dump's data of an audit table
-- with a regclass column), qualified as the search path needs.
CREATE SCHEMA a263_s;
CREATE TABLE a263_s.t (r regclass, ty regtype, p regproc, n regnamespace);
INSERT INTO a263_s.t VALUES ('a263_s.t', 'a263_email', 'now', 'a263_s');
COPY a263_s.t TO STDOUT;
SET search_path = a263_s, public;
COPY a263_s.t TO STDOUT (FORMAT csv);
RESET search_path;
-- oidvector keys compare their elements whatever the lower bound (ActiveRecord's "is there a
-- lower() for this column's type" query joins ARRAY[casttarget]::oidvector to proargtypes).
CREATE TABLE a263_v1 AS SELECT '25'::oidvector AS v;
CREATE TABLE a263_v2 AS SELECT ARRAY[25::oid]::oidvector AS v;
SELECT array_lower(v1.v, 1) AS l1, array_lower(v2.v, 1) AS l2 FROM a263_v1 v1, a263_v2 v2;
SET enable_nestloop = off;
SET enable_hashjoin = off;
SELECT count(*) FROM a263_v1 JOIN a263_v2 USING (v);
SET enable_mergejoin = off;
SET enable_hashjoin = on;
SELECT count(*) FROM a263_v1 JOIN a263_v2 USING (v);
RESET enable_nestloop;
RESET enable_hashjoin;
RESET enable_mergejoin;
SELECT exists(SELECT FROM pg_proc JOIN pg_cast ON ARRAY[casttarget]::oidvector = proargtypes
  WHERE proname = 'lower' AND castsource = 'character varying(50)'::regtype) AS has_lower;
