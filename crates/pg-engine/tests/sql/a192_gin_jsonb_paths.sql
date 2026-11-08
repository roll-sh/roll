-- GIN jsonb `@>` by path (jsonb_ops and jsonb_path_ops), exact where every array element of
-- the query gives one path, and jsonb_path_ops' jsonpath conditions by path: values under
-- other keys, in arrays or not, raw scalars, nested arrays, empty objects and arrays,
-- numbers of other scales, keys that prefix others; then after writes, and with the
-- transaction's own.
CREATE TABLE jp (id int PRIMARY KEY, j jsonb, k jsonb);
INSERT INTO jp SELECT g, CASE g % 16
    WHEN 0 THEN jsonb_build_object('a', jsonb_build_object('b', g % 7))
    WHEN 1 THEN jsonb_build_object('a', jsonb_build_array(jsonb_build_object('b', g % 7)))
    WHEN 2 THEN jsonb_build_object('a', jsonb_build_object('b', g % 7, 'c', 'x' || (g % 3)))
    WHEN 3 THEN jsonb_build_object('a', jsonb_build_array(jsonb_build_object('b', g % 7), jsonb_build_object('c', 'x' || (g % 3))))
    WHEN 4 THEN jsonb_build_object('b', g % 7, 'a', 1)
    WHEN 5 THEN jsonb_build_array(g % 7, 'x' || (g % 3), jsonb_build_array(g % 5))
    WHEN 6 THEN to_jsonb(g % 7)
    WHEN 7 THEN to_jsonb('x' || (g % 3))
    WHEN 8 THEN jsonb_build_object('tags', jsonb_build_array('t' || (g % 4), 't' || (g % 6)), 'n', (g % 7)::numeric / 2)
    WHEN 9 THEN jsonb_build_object('a', jsonb_build_object(), 'ab', jsonb_build_array())
    WHEN 10 THEN jsonb_build_object('a', jsonb_build_array(jsonb_build_array(g % 7)))
    WHEN 11 THEN jsonb_build_object('', g % 7, 'a', NULL, 'f', g % 2 = 0)
    WHEN 12 THEN jsonb_build_object('a', jsonb_build_object('b', jsonb_build_array(g % 7, (g + 1) % 7)))
    WHEN 13 THEN jsonb_build_object('a', 'b', 'ab', (g % 7)::text)
    WHEN 14 THEN jsonb_build_array(jsonb_build_object('a', g % 7, 'b', 'x' || (g % 3)), jsonb_build_object('a', (g + 3) % 7))
    ELSE NULL END, NULL FROM generate_series(1, 3200) g;
INSERT INTO jp VALUES (5001, '{"a": {"b": 1.0}}'), (5002, '{"a": {"b": 1.50}}'), (5003, '[[1, 2], [3]]'),
  (5004, '{"a": "éx"}'), (5005, '[]'), (5006, '{}'), (5007, 'null'), (5008, 'true');
UPDATE jp SET k = j;
CREATE INDEX jp_ops ON jp USING gin (j);
CREATE INDEX jp_path ON jp USING gin (k jsonb_path_ops);
ANALYZE jp;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM jp WHERE j @> '{"a": {"b": 3}}';
EXPLAIN (COSTS OFF) SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}';
EXPLAIN (COSTS OFF) SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [{"b": 3}]}') AS path FROM jp WHERE j @> '{"a": [{"b": 3}]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3, "c": "x1"}}') AS path FROM jp WHERE j @> '{"a": {"b": 3, "c": "x1"}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [{"b": 3}, {"c": "x1"}]}') AS path FROM jp WHERE j @> '{"a": [{"b": 3}, {"c": "x1"}]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [{"b": 3, "c": "x1"}]}') AS path FROM jp WHERE j @> '{"a": [{"b": 3, "c": "x1"}]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 1}}') AS path FROM jp WHERE j @> '{"a": {"b": 1}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 1.50}}') AS path FROM jp WHERE j @> '{"a": {"b": 1.50}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 1.5}}') AS path FROM jp WHERE j @> '{"a": {"b": 1.5}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"b": 3}') AS path FROM jp WHERE j @> '{"b": 3}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": 1}') AS path FROM jp WHERE j @> '{"a": 1}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[3]') AS path FROM jp WHERE j @> '[3]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[3, "x1"]') AS path FROM jp WHERE j @> '[3, "x1"]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[[3]]') AS path FROM jp WHERE j @> '[[3]]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[[1, 2]]') AS path FROM jp WHERE j @> '[[1, 2]]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[[1], [2]]') AS path FROM jp WHERE j @> '[[1], [2]]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '3') AS path FROM jp WHERE j @> '3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '"x1"') AS path FROM jp WHERE j @> '"x1"';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> 'null') AS path FROM jp WHERE j @> 'null';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> 'true') AS path FROM jp WHERE j @> 'true';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"tags": ["t1"]}') AS path FROM jp WHERE j @> '{"tags": ["t1"]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"tags": ["t1", "t3"]}') AS path FROM jp WHERE j @> '{"tags": ["t1", "t3"]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"tags": "t1"}') AS path FROM jp WHERE j @> '{"tags": "t1"}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"n": 1.5}') AS path FROM jp WHERE j @> '{"n": 1.5}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"n": 3}') AS path FROM jp WHERE j @> '{"n": 3}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {}}') AS path FROM jp WHERE j @> '{"a": {}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"ab": []}') AS path FROM jp WHERE j @> '{"ab": []}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": []}') AS path FROM jp WHERE j @> '{"a": []}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{}') AS path FROM jp WHERE j @> '{}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[]') AS path FROM jp WHERE j @> '[]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [[3]]}') AS path FROM jp WHERE j @> '{"a": [[3]]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [3]}') AS path FROM jp WHERE j @> '{"a": [3]}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"": 3, "f": false}') AS path FROM jp WHERE j @> '{"": 3, "f": false}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": null}') AS path FROM jp WHERE j @> '{"a": null}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": [3]}}') AS path FROM jp WHERE j @> '{"a": {"b": [3]}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}, "ab": "3"}') AS path FROM jp WHERE j @> '{"a": {"b": 3}, "ab": "3"}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": "b"}') AS path FROM jp WHERE j @> '{"a": "b"}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"ab": "3"}') AS path FROM jp WHERE j @> '{"ab": "3"}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[{"a": 3}]') AS path FROM jp WHERE j @> '[{"a": 3}]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[{"a": 3, "b": "x1"}]') AS path FROM jp WHERE j @> '[{"a": 3, "b": "x1"}]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '[{"a": 3}, {"a": 6}]') AS path FROM jp WHERE j @> '[{"a": 3}, {"a": 6}]';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": "éx"}') AS path FROM jp WHERE j @> '{"a": "éx"}';
SELECT id FROM jp WHERE j @> '{"a": {"b": 3}}' ORDER BY id LIMIT 5;
SELECT id FROM jp WHERE k @> '{"a": {"b": 3}}' ORDER BY id LIMIT 5;
SELECT count(*), sum(id) FROM jp WHERE j @> '{"a": [{"b": 3}]}';
SELECT count(*), sum(id) FROM jp WHERE k @> '{"a": [{"b": 3}]}';
-- jsonpath under both classes.
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @? '$.a.b ? (@ == 3)') AS path FROM jp WHERE j @? '$.a.b ? (@ == 3)';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3') AS path FROM jp WHERE j @@ '$.a.b == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a[*].b == 3') AS path FROM jp WHERE j @@ '$.a[*].b == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ 'strict $.a.b == 3') AS path FROM jp WHERE j @@ 'strict $.a.b == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a == "b"') AS path FROM jp WHERE j @@ '$.a == "b"';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.tags[*] == "t1"') AS path FROM jp WHERE j @@ '$.tags[*] == "t1"';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$ == 3') AS path FROM jp WHERE j @@ '$ == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$[*] == 3') AS path FROM jp WHERE j @@ '$[*] == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.n == 1.50') AS path FROM jp WHERE j @@ '$.n == 1.50';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.f == false') AS path FROM jp WHERE j @@ '$.f == false';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a == null') AS path FROM jp WHERE j @@ '$.a == null';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3 && $.a.c == "x1"') AS path FROM jp WHERE j @@ '$.a.b == 3 && $.a.c == "x1"';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3 || $.b == 3') AS path FROM jp WHERE j @@ '$.a.b == 3 || $.b == 3';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @? '$.a ? (@.b == 3 && @.c == "x1")') AS path FROM jp WHERE j @? '$.a ? (@.b == 3 && @.c == "x1")';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @? '$.** ? (@ == 3)') AS path FROM jp WHERE j @? '$.** ? (@ == 3)';
-- The transaction's own writes.
BEGIN;
INSERT INTO jp VALUES (6001, '{"a": {"b": 3}}', '{"a": {"b": 3}}'), (6002, '{"a": [{"b": 3}]}', '{"a": [{"b": 3}]}');
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [{"b": 3}]}') AS path FROM jp WHERE j @> '{"a": [{"b": 3}]}';
DELETE FROM jp WHERE id < 100;
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3') AS path FROM jp WHERE j @@ '$.a.b == 3';
ROLLBACK;
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
-- Rows written again and deleted since the structure read them.
UPDATE jp SET j = '{"a": {"b": 3}}', k = '{"a": {"b": 3}}' WHERE id <= 32;
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": [{"b": 3}]}') AS path FROM jp WHERE j @> '{"a": [{"b": 3}]}';
DELETE FROM jp WHERE id BETWEEN 33 AND 64;
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3}}') AS path FROM jp WHERE j @> '{"a": {"b": 3}}';
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @@ '$.a.b == 3') AS path FROM jp WHERE j @@ '$.a.b == 3';
INSERT INTO jp VALUES (7001, '{"a": {"b": 3, "c": "x1"}}', '{"a": {"b": 3, "c": "x1"}}');
SELECT count(*) AS ops, (SELECT count(*) FROM jp WHERE k @> '{"a": {"b": 3, "c": "x1"}}') AS path FROM jp WHERE j @> '{"a": {"b": 3, "c": "x1"}}';
RESET enable_seqscan;
DROP TABLE jp;

-- A bounded containment matrix. Record the complete matching id sets, not just counts.
CREATE TABLE jm (id int, j jsonb, k jsonb, plain jsonb);
INSERT INTO jm SELECT ord::int, j, j, j FROM jsonb_array_elements($matrix$[
 null, true, false, 0, -0.0, 1, 1.0, 1.50, "x", "é", "", {}, [],
 [1], [1,1.0], [1,2], [[1]], [[1,2]], [[1],[2]], [[],1],
 {"a":1}, {"a":[1]}, {"a":[[1]]}, {"a":[]}, {"a":{}}, {"a":null},
 {"a":{"b":1}}, {"a":{"b":[1]}}, {"a":[{"b":1}]},
 {"a":[{"b":1,"c":2}]}, {"a":[{"b":1},{"c":2}]},
 {"a":[{"b":1,"c":0},{"b":0,"c":2}]},
 [{"a":1}], [{"a":[1]}], [{"a":1,"b":2}], [{"a":1},{"b":2}],
 [{"a":1,"b":0},{"a":0,"b":2}],
 {"a":{"b":1},"ab":1}, {"a":1,"a":2}, {"":1,"ab":"x"},
 {"a":[{"b":1},{}]}, [{"a":1},{}], {"a":[1,[]]},
 {"a":{"b":[]},"c":1}, {"a":{"b":1.000}}, {"a":[1.0,1.00]},
 {"a":[{"b":[1,2]}]}, {"a":[{"b":[1]},{"b":[2]}]}
]$matrix$::jsonb) WITH ORDINALITY AS d(j, ord);
INSERT INTO jm VALUES (100, NULL, NULL, NULL);
CREATE INDEX jm_ops ON jm USING gin (j);
CREATE INDEX jm_path ON jm USING gin (k jsonb_path_ops);
ANALYZE jm;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM jm WHERE j @> '{"a":1}';
EXPLAIN (COSTS OFF) SELECT id FROM jm WHERE k @> '{"a":1}';
CREATE FUNCTION jm_check(q jsonb) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r1 text; r2 text; r3 text;
BEGIN
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r1 FROM jm WHERE j @> q;
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r2 FROM jm WHERE k @> q;
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r3 FROM jm WHERE plain @> q;
  RETURN coalesce(r1, '-') || ' | ' || (r1 IS NOT DISTINCT FROM r2) || ' ' || (r1 IS NOT DISTINCT FROM r3);
END $$;
SELECT id, jm_check(plain) FROM jm ORDER BY id;
-- Compare lax/strict jsonpath matching id sets under both classes and the evaluator.
CREATE FUNCTION jm_path_check(q jsonpath) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r1 text; r2 text; r3 text;
BEGIN
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r1 FROM jm WHERE j @@ q;
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r2 FROM jm WHERE k @@ q;
  SELECT string_agg(id::text, ',' ORDER BY id) INTO r3 FROM jm WHERE plain @@ q;
  RETURN coalesce(r1, '-') || ' | ' || (r1 IS NOT DISTINCT FROM r2) || ' ' || (r1 IS NOT DISTINCT FROM r3);
END $$;
SELECT q, jm_path_check(q::jsonpath) FROM (VALUES
 ('$.a == 1'), ('strict $.a == 1'), ('$.a.b == 1'), ('strict $.a.b == 1'),
 ('$.a[*].b == 1'), ('strict $.a[*].b == 1'), ('$.a == null'), ('strict $.a == null'),
 ('$.a.b == 1 && $.a.c == 2'), ('strict $.a.b == 1 && $.a.c == 2'),
 ('$.a.b == 1 || $.ab == 1'), ('$.** == 1'), ('$[*] == 1'), ('strict $[*] == 1'),
 ('$ == "x"'), ('strict $ == "x"')) v(q) ORDER BY q;
-- The two-leaf object must come from one array element. Empty siblings require recheck.
SELECT jm_check('{"a":[{"b":1,"c":2}]}'), jm_check('[{"a":1,"b":2}]'), jm_check('{"a":[{"b":1},{}]}');
BEGIN;
UPDATE jm SET j = '{"a":1}', k = '{"a":1}', plain = '{"a":1}' WHERE id = 22;
DELETE FROM jm WHERE id = 21;
INSERT INTO jm VALUES (101, '{"a":1}', '{"a":1}', '{"a":1}');
SELECT jm_check('{"a":1}'), jm_check('{"a":[1]}');
ROLLBACK;
SELECT jm_check('{"a":1}'), jm_check('{"a":[1]}');
UPDATE jm SET j = '{"a":[1]}', k = '{"a":[1]}', plain = '{"a":[1]}' WHERE id = 21;
DELETE FROM jm WHERE id = 22;
INSERT INTO jm VALUES (102, '{"a":[1]}', '{"a":[1]}', '{"a":[1]}');
SELECT jm_check('{"a":1}'), jm_check('{"a":[1]}');
-- Partial indexes retain the heap predicate and recheck.
DROP INDEX jm_ops, jm_path;
CREATE INDEX jm_ops ON jm USING gin (j) WHERE id > 25;
CREATE INDEX jm_path ON jm USING gin (k jsonb_path_ops) WHERE id > 25;
SELECT string_agg(id::text, ',' ORDER BY id) FROM jm WHERE j @> '{"a":[{"b":1}]}' AND id > 25;
SELECT string_agg(id::text, ',' ORDER BY id) FROM jm WHERE k @> '{"a":[{"b":1}]}' AND id > 25;
RESET enable_seqscan;
DROP FUNCTION jm_check(jsonb), jm_path_check(jsonpath);
DROP TABLE jm;
