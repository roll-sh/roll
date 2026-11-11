-- literals and output
SELECT ARRAY[1, 2, 3], '{1,2,3}'::int[], '{{1,2},{3,4}}'::int[], ARRAY['a', NULL, 'b c', 'd"e'], '{}'::text[], ARRAY[]::int[];
SELECT '[0:2]={1,2,3}'::int[], ('[0:2]={1,2,3}'::int[])[0], array_lower('[0:2]={1,2,3}'::int[], 1), '{"x,y", "", NULL, "NULL"}'::text[];
SELECT '{1,2'::int[];
SELECT '{{1,2},{3}}'::int[];
SELECT ARRAY[[1, 2], [3, 4]] = '{{1,2},{3,4}}', ARRAY[1, 2] < ARRAY[1, 3], ARRAY[1, 2] < ARRAY[1, 2, 0], ARRAY[NULL::int] = ARRAY[NULL::int];
-- subscripts and slices
SELECT (ARRAY[10, 20, 30])[2], (ARRAY[10, 20, 30])[5], (ARRAY[10, 20, 30])[2:3], (ARRAY[10, 20, 30])[:2], (ARRAY[10, 20, 30])[2:], ('{{1,2},{3,4}}'::int[])[2][1];
SELECT ('{{1,2,3},{4,5,6}}'::int[])[1:2][2:3], ('{{1,2},{3,4}}'::int[])[1], (ARRAY[1, 2])[0:5];
-- operators
SELECT ARRAY[1, 2] || ARRAY[3], ARRAY[1, 2] || 3, 0 || ARRAY[1, 2], ARRAY[[1, 2]] || ARRAY[3, 4], ARRAY[1, 2, 3] @> ARRAY[2, 1], ARRAY[1] <@ ARRAY[1, 2], ARRAY[1, 2] && ARRAY[2, 9];
SELECT 2 = ANY (ARRAY[1, 2]), 2 = ALL (ARRAY[1, 2]), 3 > ALL ('{1,2}'), NULL = ANY (ARRAY[1]), 1 = ANY (ARRAY[NULL, 1]), 2 = ANY (ARRAY[NULL, 1]), 5 <> ALL ('{}'::int[]);
-- functions
SELECT array_length(ARRAY[1, 2, 3], 1), array_length('{}'::int[], 1), array_dims('{{1,2},{3,4}}'::int[]), array_ndims(ARRAY[[1]]), array_upper(ARRAY[1, 2], 1), cardinality('{{1,2},{3,4}}'::int[]);
SELECT array_append(ARRAY[1], 2), array_prepend(0, ARRAY[1]), array_cat(ARRAY[1], ARRAY[2, 3]), array_remove(ARRAY[1, 2, 1], 1), array_replace(ARRAY[1, 2, 1], 1, 9), array_position(ARRAY['a', 'b', 'a'], 'a', 2), array_positions(ARRAY[1, 2, 1], 1);
SELECT array_to_string(ARRAY[1, NULL, 3], ',', '*'), array_to_string(ARRAY['a', 'b'], '-'), string_to_array('a,b,,c', ','), string_to_array('a b c', ' ', 'b'), array_fill(7, ARRAY[2, 2]), array_fill(0, ARRAY[3], ARRAY[2]);
SELECT trim_array(ARRAY[1, 2, 3, 4], 2), array_reverse(ARRAY[1, 2, 3]), array_sample(ARRAY[5], 1), array_shuffle(ARRAY[7]), array_sort(ARRAY[3, 1, 2]), array_sort(ARRAY[3, 1, 2], true);
SELECT unnest(ARRAY[1, 2, 3]), unnest('{{1,2},{3,4}}'::int[]) ORDER BY 1 LIMIT 3;
SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c']) AS t(n, s);
SELECT * FROM unnest(ARRAY['x', 'y']) WITH ORDINALITY AS t(v, i);
SELECT array_agg(x ORDER BY x DESC), array_agg(DISTINCT x % 2) FROM generate_series(1, 5) x;
SELECT array_agg(ARRAY[x, x * 10]) FROM generate_series(1, 3) x;
SELECT ARRAY(SELECT generate_series(1, 4)), ARRAY(SELECT 'v' || x FROM generate_series(1, 2) x), ARRAY(SELECT 1 WHERE false);
-- in tables
CREATE TABLE arr (id int PRIMARY KEY, tags text[] NOT NULL DEFAULT '{}', grid int[][], scores numeric(5,2)[]);
INSERT INTO arr VALUES (1, '{a,b}', '{{1,2},{3,4}}', '{1.5, 2.25}'), (2, '{b}', NULL, '{}'), (3, DEFAULT, '{{5}}', NULL);
UPDATE arr SET tags[3] = 'z' WHERE id = 1;
UPDATE arr SET tags = array_append(tags, 'n') WHERE id = 3;
UPDATE arr SET grid[1][1] = 0 WHERE id = 3;
SELECT id, tags, grid, scores, tags[1], array_length(tags, 1) FROM arr ORDER BY id;
SELECT id FROM arr WHERE 'b' = ANY (tags) ORDER BY id;
SELECT id FROM arr WHERE tags @> '{b}' ORDER BY id;
CREATE INDEX arr_tags ON arr USING gin (tags);
SELECT id FROM arr WHERE tags && ARRAY['z', 'n'] ORDER BY id;
SELECT tag, count(*) FROM arr, unnest(tags) tag GROUP BY tag ORDER BY tag;
INSERT INTO arr VALUES (4, '{x}', '{{1,2},{3}}');
UPDATE arr SET scores[2] = 1000 WHERE id = 1;
DROP TABLE arr;
