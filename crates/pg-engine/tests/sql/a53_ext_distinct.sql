-- IS [NOT] DISTINCT FROM and NULLIF compare with the operand types' own `=` (an extension's
-- operator: hstore, cube, ltree, citext), as make_distinct_op does.
CREATE EXTENSION hstore; CREATE EXTENSION ltree; CREATE EXTENSION cube; CREATE EXTENSION citext;
SELECT 'a=>1,b=>2'::hstore IS DISTINCT FROM 'b=>2, a=>1'::hstore AS h1, 'a=>1'::hstore IS NOT DISTINCT FROM 'a=>1'::hstore AS h2;
SELECT 'Abc'::citext IS DISTINCT FROM 'aBC'::citext AS c1, 'Abc'::citext IS NOT DISTINCT FROM 'abc'::citext AS c2;
SELECT '(1,2)'::cube IS DISTINCT FROM '(1.0, 2.0)'::cube AS q1, '(1),(1)'::cube IS DISTINCT FROM '(1)'::cube AS q2;
SELECT 'a.b'::ltree IS DISTINCT FROM 'a.b'::ltree AS l1, NULL::ltree IS DISTINCT FROM 'a'::ltree AS l2;
SELECT ROW('Abc'::citext) IS DISTINCT FROM ROW('abc'::citext) AS r1;
SELECT ARRAY['Abc'::citext] IS DISTINCT FROM ARRAY['abc'::citext] AS a1;
SELECT nullif('Abc'::citext, 'abc'::citext) AS n1;
SELECT 'Abc'::citext = ANY(ARRAY['abc'::citext]) AS any1;
SELECT CASE 'Abc'::citext WHEN 'abc' THEN 'match' ELSE 'no' END AS case1;
