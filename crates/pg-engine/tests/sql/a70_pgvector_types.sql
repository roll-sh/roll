-- pgvector's types as an extension's native types (agent 70): typmods, members, casts, binary
-- I/O, operators. Needs a reference with pgvector 0.8.7 installed.
CREATE EXTENSION vector;
SELECT extversion FROM pg_extension WHERE extname = 'vector';
-- The script's objects, implicit array types included.
SELECT pg_describe_object(classid, objid, objsubid) || ' ' || deptype::text
FROM pg_depend WHERE refobjid = (SELECT oid FROM pg_extension WHERE extname = 'vector')
  AND classid = 'pg_type'::regclass ORDER BY 1;
SELECT count(*) FROM pg_depend WHERE refobjid = (SELECT oid FROM pg_extension WHERE extname = 'vector');
SELECT oprname, oprleft::regtype, oprright::regtype FROM pg_operator WHERE oprname IN ('<~>', '<%>') ORDER BY 1;
-- Type modifiers.
CREATE TABLE a70_tv (a vector(3), b halfvec(4)[], c sparsevec(5), d vector);
SELECT attname, format_type(atttypid, atttypmod), atttypmod FROM pg_attribute
WHERE attrelid = 'a70_tv'::regclass AND attnum > 0 ORDER BY attnum;
INSERT INTO a70_tv (a) VALUES ('[1,2,3]');
INSERT INTO a70_tv (a) VALUES ('[1,2]');
INSERT INTO a70_tv (a) VALUES ('[1,2,3]'::vector(2));
INSERT INTO a70_tv (b) VALUES ('{"[1,2,3,4]","[1,2]"}');
INSERT INTO a70_tv (c) VALUES ('{1:1}/5'), ('{1:1}/4');
INSERT INTO a70_tv (d) VALUES ('[1]'), ('[1,2,3,4,5]');
UPDATE a70_tv SET a = '[4,5]' WHERE a IS NOT NULL;
SELECT '[1,2,3]'::vector(3), '[1,2,3]'::vector(2);
SELECT '[1,2,3]'::vector('a');
SELECT '[1,2,3]'::vector(0);
SELECT '[1,2,3]'::vector(16001);
SELECT '[1,2,3]'::vector(3, 4);
SELECT ARRAY['[1,2,3]'::vector]::vector(2)[];
SELECT pg_typeof('[1,2]'::vector(2)), '[1,2]'::vector::text::vector(2);
CREATE VIEW a70_tvv AS SELECT a::vector(3) AS a3, c FROM a70_tv;
SELECT pg_get_viewdef('a70_tvv'::regclass);
SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'a70_tvv'::regclass ORDER BY attnum;
SELECT * FROM a70_tv ORDER BY d NULLS LAST, c;
-- Distances, aggregates, casts.
SELECT '[1,2,3]'::vector <-> '[3,1,2]', '[1,2,3]'::vector <#> '[3,1,2]', '[1,2,3]'::vector <=> '[3,1,2]', '[1,2,3]'::vector <+> '[3,1,2]';
SELECT avg(v), sum(v) FROM (VALUES ('[1,2,3]'::vector), ('[4,5,6]'), (NULL)) t(v);
SELECT '{1,2,3}'::real[]::vector, '[1,2,3]'::vector::real[], '[1.5,2.5]'::vector::halfvec, '[1,0,2]'::vector::sparsevec;
SELECT binary_quantize('[1,-2,3]'::vector), '101'::bit <~> '011', '101'::bit <%> '011';
-- The access methods.
SELECT amname, amtype, amhandler FROM pg_am WHERE amname IN ('ivfflat', 'hnsw') ORDER BY 1;
SELECT amname, p, pg_indexam_has_property(a.oid, p) FROM pg_am a,
  unnest(array['can_order', 'can_unique', 'can_multi_col', 'can_exclude', 'can_include']) p
WHERE amname IN ('ivfflat', 'hnsw') ORDER BY 1, 2;
SELECT opcname, amname, opcdefault FROM pg_opclass c JOIN pg_am a ON a.oid = c.opcmethod
WHERE amname IN ('ivfflat', 'hnsw') ORDER BY 2, 1;
SHOW hnsw.ef_search;
SET ivfflat.probes = 0;
SET hnsw.iterative_scan = strict_order;
SHOW hnsw.iterative_scan;
DROP VIEW a70_tvv;
DROP TABLE a70_tv;
DROP EXTENSION vector;
