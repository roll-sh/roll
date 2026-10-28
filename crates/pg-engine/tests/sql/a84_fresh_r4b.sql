-- Round 4 difftest finds, batch 2 (agent 84), from the round 4 grammar's seeds.
CREATE TABLE a84j (id int, j jsonb);
INSERT INTO a84j VALUES (1, '{"n": 1e3}'), (2, '{"n": "10"}'), (3, '{"n": 2.5}'), (4, '{"n": -0.0}'),
  (5, '{"n": 123456789012345678901234567890}'), (6, '{"a": [1, 2, 3]}'), (7, '{"a": ["1", "a", null, true]}'),
  (8, '{"a": 5, "s": "abc"}'), (9, '{"b": {"c": "x"}, "a": [1]}'), (10, '{"b": {"c": "x"}}'), (11, '[1, "x", 3]');
-- .double() of a numeric keeps the numeric (only checked to be a double).
SELECT id, jsonb_path_query_array(j, '$.n.double()', '{}', true) FROM a84j ORDER BY id;
-- A path is evaluated depth first: what an unwrapped array gave before an error goes
-- through the rest of the path, and a silenced error keeps it.
SELECT id, jsonb_path_query_array(j, '$.a.boolean()', '{}', true) FROM a84j ORDER BY id;
SELECT id, jsonb_path_query_array(j, '$[*].double()', '{}', true) FROM a84j WHERE id = 11;
SELECT t.id, r.v FROM a84j t, LATERAL jsonb_path_query(t.j, '$.a.boolean()', '{}', true) WITH ORDINALITY r(v, o) ORDER BY t.id, r.o;
-- Lax exists stops at the first item found: an error past it never happens.
SELECT id, jsonb_path_exists(j, '$.*.keyvalue()', '{}', silent => true), j @? '$.*.keyvalue()', jsonb_path_exists(j, 'strict $.*.keyvalue()', '{}', true) FROM a84j ORDER BY id;
SELECT id, JSON_EXISTS(j, '$.a.boolean()' TRUE ON ERROR), JSON_EXISTS(j, '$.a.boolean()') FROM a84j ORDER BY id;
-- SQL/JSON functions run their paths with time zones (JsonPathQuery's useTz).
SELECT JSON_QUERY('{"ts": "2025-11-02 01:30:00+00"}', '$.ts.timestamp()' RETURNING json);
SELECT JSON_VALUE('{"ts": "2025-11-02 01:30:00+00"}', '$.ts.timestamp()' RETURNING text);
SELECT * FROM JSON_TABLE('{"ts": "2025-11-02 01:30:00+00"}', '$' COLUMNS (t timestamp PATH '$.ts.timestamp()'));
-- sum(float8) and sum(float4) start from the first value (float8pl): -0 stays -0.
SELECT sum(x), avg(x) FROM (VALUES ('-0'::float8)) v(x);
SELECT sum(x) FROM (VALUES ('-0'::float8), ('-0')) v(x);
SELECT sum(x) FROM (VALUES ('-0'::float8), ('0')) v(x);
SELECT sum(x) FROM (VALUES ('-0'::float4)) v(x);
SELECT id, sum(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW) FROM (VALUES (1, '-0'::float8), (2, '1')) v(id, x) ORDER BY id;
-- NULLIF's `=` over an array of a domain: no operator against the base type's array.
CREATE DOMAIN a84jd AS int CHECK (value > -3);
CREATE TABLE a84jt (id int, c a84jd);
SELECT NULLIF('{1,2}'::int4[], array_agg(c)) FROM a84jt;
SELECT NULLIF(array_agg(c), array_agg(c)) FROM a84jt;
