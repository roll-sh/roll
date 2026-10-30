-- GIN scans over arrays, jsonb and tsvector, and pg_trgm similarity, read the rows their
-- posting lists select (agent 114): every operator, NULL and empty values, NULL elements,
-- duplicate elements, prefixes, negations, thresholds, own uncommitted writes, updates and
-- deletes after the structure was built.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE TABLE a114_g (id int, ia int[], ta text[], fa float8[], na numeric[], j jsonb, jp jsonb, tv tsvector);
INSERT INTO a114_g SELECT g, ARRAY[g % 7, g % 11, g % 7], ARRAY['w' || (g % 13), 'x' || (g % 5)], ARRAY[(g % 4)::float8 / 2],
  ARRAY[(g % 6)::numeric / 2], jsonb_build_object('k', g % 9, 'tags', jsonb_build_array('t' || (g % 4), g % 3 = 0), 'n', CASE WHEN g % 10 = 0 THEN NULL ELSE g % 10 END),
  jsonb_build_object('a', jsonb_build_object('b', g % 5)), to_tsvector('english', 'word' || (g % 17) || ' common thing' || (g % 3) || ' ' || CASE WHEN g % 2 = 0 THEN 'even' ELSE 'odd' END)
  FROM generate_series(1, 400) g;
INSERT INTO a114_g VALUES (1001, '{}', '{}', '{}', '{}', '{}', '{}', ''),
  (1002, '{NULL}', '{NULL,w1}', '{NaN,-0}', '{1.0,2.50}', '[]', '[]', 'a:1A b:2B'),
  (1003, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
  (1004, '{1,NULL,3}', '{w1,w1}', '{0}', '{0.5}', '"t1"', '"t1"', 'word1:3C'),
  (1005, '{{1,2},{3,4}}', '{x1}', '{Infinity}', '{NaN}', '["k", {"k": 1}, [[5]]]', '{"a": {"b": [1, 2]}}', 'pre:1 prefix:2');
CREATE INDEX a114_g_ia ON a114_g USING gin (ia);
CREATE INDEX a114_g_ta ON a114_g USING gin (ta);
CREATE INDEX a114_g_fa ON a114_g USING gin (fa);
CREATE INDEX a114_g_na ON a114_g USING gin (na);
CREATE INDEX a114_g_j ON a114_g USING gin (j);
CREATE INDEX a114_g_jp ON a114_g USING gin (jp jsonb_path_ops);
CREATE INDEX a114_g_tv ON a114_g USING gin (tv);
ANALYZE a114_g;
SET enable_seqscan = off;
CREATE FUNCTION a114_ids(q text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text;
BEGIN
  EXECUTE 'SELECT string_agg(id::text, '','' ORDER BY id) FROM a114_g WHERE ' || q INTO r;
  RETURN coalesce(r, '-');
END $$;
EXPLAIN (COSTS OFF) SELECT id FROM a114_g WHERE ia @> '{3}';
EXPLAIN (COSTS OFF) SELECT id FROM a114_g WHERE j ? 'k';
EXPLAIN (COSTS OFF) SELECT id FROM a114_g WHERE tv @@ 'even';
SELECT q, a114_ids(q) FROM (VALUES
  ('ia @> ''{3}'''), ('ia @> ''{3,5}'''), ('ia @> ''{3,NULL}'''), ('ia @> ''{}'''), ('ia @> ''{1,1}'''),
  ('ia && ''{2,10}'''), ('ia && ''{NULL}'''), ('ia && ''{}'''), ('ia <@ ''{0,1,2,3}'''), ('ia <@ ''{}'''), ('ia <@ ''{NULL}'''),
  ('ia = ''{1,1}'''), ('ia = ''{}'''), ('ia = ''{NULL}'''), ('ia = ''{1,NULL,3}'''), ('ia = ''{{1,2},{3,4}}'''), ('ia = ''{1,2,3,4}'''),
  ('ta @> ''{w1}'''), ('ta @> ''{w1,x1}'''), ('ta && ''{x4,w12}'''), ('ta <@ ''{w1,x1}'''), ('ta = ''{w1,w1}'''),
  ('fa @> ''{0}'''), ('fa @> ''{-0}'''), ('fa @> ''{NaN}'''), ('fa && ''{1.5,Infinity}'''), ('fa <@ ''{0,0.5}'''),
  ('na @> ''{1}'''), ('na @> ''{2.5000}'''), ('na && ''{NaN}'''), ('na = ''{0.50}'''),
  ('ia @> ''{3}'' AND ta @> ''{w3}'''), ('ia @> ''{3}'' OR ta @> ''{w3}''')) v(q);
SELECT q, a114_ids(q) FROM (VALUES
  ('j @> ''{"k": 3}'''), ('j @> ''{"k": 3.0}'''), ('j @> ''{"tags": ["t1"]}'''), ('j @> ''{"tags": [true]}'''), ('j @> ''{"n": null}'''),
  ('j @> ''{}'''), ('j @> ''[]'''), ('j @> ''"t1"'''), ('j @> ''["k"]'''), ('j @> ''[{"k": 1}]'''), ('j @> ''[[5]]'''), ('j @> ''{"k": 3, "tags": ["t3"]}'''),
  ('j ? ''k'''), ('j ? ''t1'''), ('j ? ''tags'''), ('j ? ''nope'''), ('j ?| ''{nope,t1}'''), ('j ?| ''{}'''), ('j ?| ''{NULL}'''),
  ('j ?& ''{k,tags}'''), ('j ?& ''{}'''), ('j ?& ''{k,NULL}'''), ('j ?& ''{k,t1}'''),
  ('jp @> ''{"a": {"b": 3}}'''), ('jp @> ''{"a": {"b": [2]}}'''), ('jp @> ''{"a": {}}'''), ('jp @> ''"t1"'''), ('jp @> ''{}'''),
  ('j @? ''$.k ? (@ == 3)'''), ('jp @@ ''$.a.b == 4''')) v(q);
SELECT q, a114_ids(q) FROM (VALUES
  ('tv @@ ''even'''), ('tv @@ ''even & word3'''), ('tv @@ ''word3 | word5'''), ('tv @@ ''!even'''), ('tv @@ ''even & !word3'''),
  ('tv @@ ''!even | word2'''), ('tv @@ ''pre:*'''), ('tv @@ ''word1:*'''), ('tv @@ ''word1:* & thing1'''), ('tv @@ ''common <-> thing2'''),
  ('tv @@ ''a:A'''), ('tv @@ ''a:B'''), ('tv @@ ''b:AB'''), ('tv @@ ''word1:C'''), ('tv @@ ''nothing'''), ('tv @@ ''''::tsquery'),
  ('tv @@ to_tsquery(''english'', ''the'')'), ('tv @@ plainto_tsquery(''english'', ''common things odd'')')) v(q);
-- Own writes, then updates and deletes seen by later scans.
BEGIN;
INSERT INTO a114_g (id, ia, j, tv) VALUES (2001, '{3,99}', '{"k": 3, "own": 1}', 'even word3');
UPDATE a114_g SET ia = '{500}', j = '{"k": 500}', tv = 'moved' WHERE id = 3;
SELECT a114_ids('ia @> ''{3}'''), a114_ids('ia @> ''{500}'''), a114_ids('j @> ''{"k": 3}'''), a114_ids('j ? ''own'''), a114_ids('tv @@ ''even & word3''');
SAVEPOINT s;
DELETE FROM a114_g WHERE id = 2001;
SELECT a114_ids('ia @> ''{99}''');
ROLLBACK TO s;
SELECT a114_ids('ia @> ''{99}''');
ROLLBACK;
UPDATE a114_g SET ia = '{700, 3}', j = '{"k": 3, "z": 1}' WHERE id = 1;
DELETE FROM a114_g WHERE id = 10;
INSERT INTO a114_g (id, ia, j) VALUES (3001, '{3}', '{"z": 2}');
SELECT a114_ids('ia @> ''{3}'''), a114_ids('ia @> ''{700}'''), a114_ids('j ? ''z'''), a114_ids('j @> ''{"k": 3}''');
-- A multi-column GIN index, and a partial one.
CREATE INDEX a114_g_multi ON a114_g USING gin (ia, j);
CREATE INDEX a114_g_part ON a114_g USING gin (ta) WHERE id > 200;
SELECT a114_ids('ia @> ''{6}'' AND j @> ''{"k": 6}'''), a114_ids('ta @> ''{w2}'' AND id > 200');
DROP INDEX a114_g_ia, a114_g_j, a114_g_ta;
SELECT a114_ids('ia @> ''{6}'' AND j @> ''{"k": 6}'''), a114_ids('j ? ''z'''), a114_ids('ta @> ''{w2}'' AND id > 200');
-- A nondeterministic collation's text arrays.
CREATE COLLATION a114_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE a114_c (id int, t text[] COLLATE a114_ci);
INSERT INTO a114_c VALUES (1, '{Abc}'), (2, '{abc,x}'), (3, '{zzz}');
CREATE INDEX ON a114_c USING gin (t);
SELECT id FROM a114_c WHERE t @> '{ABC}' ORDER BY id;
SELECT id FROM a114_c WHERE t && '{ABC}' ORDER BY id;
DROP TABLE a114_c;
DROP COLLATION a114_ci;
-- pg_trgm similarity: %, %>, <%, %>> and <<% at their thresholds.
CREATE TABLE a114_w (id int, w text);
INSERT INTO a114_w SELECT g, md5(g::text) FROM generate_series(1, 300) g;
INSERT INTO a114_w VALUES (1001, 'hello world'), (1002, 'hello'), (1003, ''), (1004, NULL), (1005, 'help wanted'), (1006, 'yellow');
CREATE INDEX a114_w_gin ON a114_w USING gin (w gin_trgm_ops);
ANALYZE a114_w;
SELECT id, w FROM a114_w WHERE w % 'hello' ORDER BY id;
SELECT id FROM a114_w WHERE w % md5('7') ORDER BY id;
SELECT id FROM a114_w WHERE w %> 'helo' ORDER BY id;
SELECT id FROM a114_w WHERE 'helo' <% w ORDER BY id;
SELECT id FROM a114_w WHERE w %>> 'hel' ORDER BY id;
SELECT id FROM a114_w WHERE 'world' <<% w ORDER BY id;
SELECT id FROM a114_w WHERE w % '' ORDER BY id;
SELECT id FROM a114_w WHERE w % 'hello' AND w LIKE '%wor%' ORDER BY id;
SET pg_trgm.similarity_threshold = 0.1;
SELECT id, w FROM a114_w WHERE w % 'hello' ORDER BY id;
SET pg_trgm.similarity_threshold = 0;
SELECT count(*) FROM a114_w WHERE w % 'hello';
SET pg_trgm.similarity_threshold = 1;
SELECT id FROM a114_w WHERE w % 'hello' ORDER BY id;
RESET pg_trgm.similarity_threshold;
SET pg_trgm.word_similarity_threshold = 0.3;
SELECT id FROM a114_w WHERE w %> 'helo' ORDER BY id;
RESET pg_trgm.word_similarity_threshold;
BEGIN;
INSERT INTO a114_w VALUES (2001, 'hello there');
SELECT id FROM a114_w WHERE w % 'hello' ORDER BY id;
ROLLBACK;
CREATE INDEX a114_w_gist ON a114_w USING gist (w gist_trgm_ops);
DROP INDEX a114_w_gin;
SELECT id FROM a114_w WHERE w % 'hello' ORDER BY id;
SELECT id FROM a114_w WHERE w %> 'helo' ORDER BY id;
RESET enable_seqscan;
DROP TABLE a114_w;
DROP TABLE a114_g;
DROP FUNCTION a114_ids;
