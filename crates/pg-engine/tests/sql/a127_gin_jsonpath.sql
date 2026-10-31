-- jsonb `@?` and `@@` jsonpath conditions over GIN (jsonb_ops and jsonb_path_ops) read the
-- rows the entries gin_extract_jsonb_query extracts select (agent 127); every candidate is
-- rechecked, so the rows are those of a sequential scan.
CREATE TABLE a127_j (id int, j jsonb);
INSERT INTO a127_j SELECT g, jsonb_build_object('a', g % 10, 'b', jsonb_build_object('c', 'v' || (g % 7)),
  'arr', jsonb_build_array(g % 5, 'x' || (g % 3)), 'k' || (g % 4), g % 2 = 0) FROM generate_series(1, 400) g;
INSERT INTO a127_j VALUES (1001, '{"a": null}'), (1002, '[1, "a", {"a": 3}]'), (1003, '"v1"'), (1004, '{"b": {"c": ["v1", "v2"]}}'),
  (1005, '{"a": [3, 4]}'), (1006, '{"x1": 1, "arr": ["x1"]}'), (1007, NULL), (1008, '{}'), (1009, '{"a": 3.0}'), (1010, '{"t": true, "f": false}');
CREATE INDEX a127_j_ops ON a127_j USING gin (j);
CREATE TABLE a127_p (id int, j jsonb);
INSERT INTO a127_p SELECT * FROM a127_j;
CREATE INDEX a127_p_ops ON a127_p USING gin (j jsonb_path_ops);
ANALYZE a127_j, a127_p;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM a127_j WHERE j @? '$.a ? (@ == 3)';
EXPLAIN (COSTS OFF) SELECT id FROM a127_p WHERE j @@ '$.b.c == "v2"';
-- The same conditions on both, compared with a scan of the table.
CREATE FUNCTION a127_check(q jsonpath, pred bool) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r1 text; r2 text; r3 text;
BEGIN
  IF pred THEN
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r1 FROM a127_j WHERE j @@ q;
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r2 FROM a127_p WHERE j @@ q;
    SET LOCAL enable_bitmapscan = off; SET LOCAL enable_seqscan = on;
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r3 FROM a127_j WHERE j @@ q;
  ELSE
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r1 FROM a127_j WHERE j @? q;
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r2 FROM a127_p WHERE j @? q;
    SET LOCAL enable_bitmapscan = off; SET LOCAL enable_seqscan = on;
    SELECT string_agg(id::text, ',' ORDER BY id) INTO r3 FROM a127_j WHERE j @? q;
  END IF;
  SET LOCAL enable_bitmapscan = on; SET LOCAL enable_seqscan = off;
  RETURN coalesce(r1, '-') || ' | ' || (r1 IS NOT DISTINCT FROM r2) || ' ' || (r1 IS NOT DISTINCT FROM r3);
END $$;
SELECT a127_check('$.a ? (@ == 3)', false);
SELECT a127_check('$.a == 3', true);
SELECT a127_check('strict $.a == 3', true);
SELECT a127_check('$.a', false);
SELECT a127_check('$.k1', false);
SELECT a127_check('$.b.c == "v2"', true);
SELECT a127_check('strict $.b.c == "v1"', true);
SELECT a127_check('$.b.c[*] == "v1"', true);
SELECT a127_check('strict $.b.c[*] == "v1"', true);
SELECT a127_check('$.arr[*] == "x1"', true);
SELECT a127_check('strict $.arr[*] == "x1"', true);
SELECT a127_check('$.* == "x1"', true);
SELECT a127_check('$.** == "v6"', true);
SELECT a127_check('strict $.** == "v6"', true);
SELECT a127_check('$ == "v1"', true);
SELECT a127_check('strict $ == "v1"', true);
SELECT a127_check('$[*] == "a"', true);
SELECT a127_check('$.a == null', true);
SELECT a127_check('$.t == true', true);
SELECT a127_check('$.f == false', true);
SELECT a127_check('$.k2 == true', true);
SELECT a127_check('$.a == 3 && $.b.c == "v3"', true);
SELECT a127_check('$.a == 3 || $.b.c == "v3"', true);
SELECT a127_check('$.a == 3 || $.a > 8', true);
SELECT a127_check('$.a == 3 && $.a > 2', true);
SELECT a127_check('!($.a == 3)', true);
SELECT a127_check('!($.a != 3)', true);
SELECT a127_check('!(!($.a == 3))', true);
SELECT a127_check('exists($.b ? (@.c == "v4"))', true);
SELECT a127_check('!exists($.k3)', true);
SELECT a127_check('$.b ? (@.c == "v4" && @.c == "v4")', false);
SELECT a127_check('$ ? (@.a == 2) ? (@.b.c == "v2")', false);
SELECT a127_check('$.b ? ($.a == 4)', false);
SELECT a127_check('$.a ? (@ == -1)', false);
SELECT a127_check('$.a.type() == "number"', true);
SELECT a127_check('$.a ? (@ == $x)', false);
SELECT a127_check('$.a ? (@ like_regex "^3")', false);
SELECT a127_check('$.arr[0] == 3', true);
SELECT a127_check('$.arr[last] == "x2"', true);
SELECT a127_check('$.a == 3.0', true);
SELECT a127_check('$.a == 3 && exists($.k1)', true);
SELECT a127_check('$.nope == 1', true);
SELECT a127_check('$.a ? (@ == 3 || @ == 4)', false);
-- Rows written since the index and the transaction's own, in it too.
INSERT INTO a127_j VALUES (2001, '{"a": 3, "b": {"c": "v2"}}');
INSERT INTO a127_p VALUES (2001, '{"a": 3, "b": {"c": "v2"}}');
BEGIN;
INSERT INTO a127_j VALUES (2002, '{"b": {"c": "v2"}}');
INSERT INTO a127_p VALUES (2002, '{"b": {"c": "v2"}}');
UPDATE a127_j SET j = '{"a": 99}' WHERE id = 3;
UPDATE a127_p SET j = '{"a": 99}' WHERE id = 3;
SELECT a127_check('$.a == 3', true);
SELECT a127_check('$.b.c == "v2"', true);
ROLLBACK;
SELECT a127_check('$.b.c == "v2"', true);
RESET enable_seqscan;
DROP FUNCTION a127_check(jsonpath, bool);
DROP TABLE a127_j, a127_p;
