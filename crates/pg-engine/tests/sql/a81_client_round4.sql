-- Client suites, round 4 (agent 81).
-- asyncpg test_unknown_type_text_fallback: a range over citext (its default btree class is
-- an extension's), a domain over it, a composite of it.
CREATE EXTENSION citext;
CREATE DOMAIN a81_citext_dom AS citext;
CREATE TYPE a81_citext_range AS RANGE (SUBTYPE = citext);
CREATE TYPE a81_citext_comp AS (t citext);
SELECT a81_citext_range('a', 'B'), a81_citext_range('a', 'B') @> 'A'::citext,
  a81_citext_range('b', 'C') @> 'a'::citext;
SELECT a81_citext_range('B', 'a');
SELECT '[a,B]'::a81_citext_range, '{[a,B], [c,d)}'::a81_citext_multirange;
SELECT ('a'::citext)::a81_citext_dom, row('x')::a81_citext_comp;
SELECT (SELECT opcname FROM pg_opclass WHERE oid = rngsubopc)
  FROM pg_range WHERE rngtypid = 'a81_citext_range'::regtype;
-- A unique index of a citext column (citext_ops, the type's default class) as a primary key.
CREATE TABLE a81_cpk (x citext);
CREATE UNIQUE INDEX a81_cpk_i ON a81_cpk (x);
ALTER TABLE a81_cpk ADD PRIMARY KEY USING INDEX a81_cpk_i;
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'a81_cpk'::regclass;
DROP TABLE a81_cpk;
DROP TYPE a81_citext_range;
DROP TYPE a81_citext_comp;
DROP DOMAIN a81_citext_dom;
DROP EXTENSION citext;

-- asyncpg's type introspection walks a recursive CTE whose join clause is
-- `ti.oid = any(tt.attrtypoids)`: an index scan parameterized by the outer array, estimated
-- for 10 elements; an OR of such arms under a BitmapOr, whose `x IS NOT NULL` parts follow
-- from the strict comparisons.
CREATE TABLE a81_pt (id oid PRIMARY KEY, elem oid);
INSERT INTO a81_pt SELECT g, g + 1 FROM generate_series(1, 3000) g;
ANALYZE a81_pt;
EXPLAIN (COSTS OFF)
WITH RECURSIVE r(e, a) AS (SELECT 1007::oid, '{23}'::oid[]
  UNION ALL SELECT t.elem, NULL::oid[] FROM r JOIN a81_pt t ON t.id = any(r.a))
SELECT * FROM r;
EXPLAIN (COSTS OFF)
WITH RECURSIVE r(e, a) AS (SELECT 1007::oid, '{23}'::oid[]
  UNION ALL SELECT t.elem, NULL::oid[] FROM r, a81_pt t
  WHERE (r.e IS NOT NULL AND t.id = r.e) OR (r.a IS NOT NULL AND t.id = any(r.a)))
SELECT * FROM r;
WITH RECURSIVE r(e, a, d) AS (SELECT 10::oid, '{20,30}'::oid[], 0
  UNION ALL SELECT t.elem, CASE WHEN r.d = 0 THEN '{40}'::oid[] END, r.d + 1 FROM r, a81_pt t
  WHERE r.d < 3 AND ((r.e IS NOT NULL AND t.id = r.e) OR (r.a IS NOT NULL AND t.id = any(r.a))))
SELECT * FROM r ORDER BY d, e;
SELECT a81_pt.id FROM (VALUES ('{5,6,7}'::oid[]), ('{}'), (NULL)) v(a)
  JOIN a81_pt ON a81_pt.id = any(v.a) ORDER BY 1;
DROP TABLE a81_pt;
