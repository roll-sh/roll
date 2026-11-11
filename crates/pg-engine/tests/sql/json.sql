-- input / output
SELECT '{"b": 1, "a": [1, 2.50, "x", null, true]}'::json, '{"b": 1, "a": [1, 2.50, "x", null, true]}'::jsonb;
SELECT '{"a": 1, "a": 2}'::json, '{"a": 1, "a": 2}'::jsonb, '  [1,  2 ]'::jsonb, '"str"'::jsonb, '12.3e2'::jsonb, 'null'::jsonb;
SELECT '{"a": }'::jsonb;
SELECT '[1, 2'::json;
SELECT '"é\n"'::jsonb, '"\u0000"'::jsonb;
SELECT '{"k": "v"}'::jsonb = '{ "k" : "v" }'::jsonb, '[1,2]'::jsonb = '[2,1]'::jsonb;

-- operators
SELECT '{"a": {"b": [10, 20, 30]}}'::jsonb -> 'a', '{"a": {"b": [10, 20, 30]}}'::jsonb -> 'a' -> 'b' -> 1, '{"a": {"b": [10, 20, 30]}}'::jsonb -> 'a' -> 'b' ->> -1;
SELECT '{"a": {"b": [10, 20, 30]}}'::jsonb #> '{a,b,2}', '{"a": {"b": [10, 20, 30]}}'::jsonb #>> '{a,b}', '{"a": 1}'::jsonb -> 'zz';
SELECT '{"a": 1, "b": 2}'::json -> 'a', '{"a": "x"}'::json ->> 'a', '[1, [2, 3]]'::json #> '{1,0}';
SELECT '{"a": 1, "b": {"c": 2}}'::jsonb @> '{"b": {"c": 2}}', '{"a": 1}'::jsonb <@ '{"a": 1, "b": 2}', '[1, 2, 3]'::jsonb @> '[3, 1]', '[1, [2]]'::jsonb @> '[[2]]';
SELECT '{"a": 1, "b": 2}'::jsonb ? 'a', '{"a": 1, "b": 2}'::jsonb ?| array['x', 'b'], '{"a": 1, "b": 2}'::jsonb ?& array['a', 'x'], '["a", "b"]'::jsonb ? 'b';
SELECT '{"a": 1}'::jsonb || '{"b": 2}', '[1, 2]'::jsonb || '[3]', '[1]'::jsonb || '2', '{"a": 1}'::jsonb || '[1]';
SELECT '{"a": 1, "b": 2}'::jsonb - 'a', '["a", "b", "c"]'::jsonb - 1, '["a", "b", "a"]'::jsonb - 'a', '{"a": 1, "b": 2, "c": 3}'::jsonb - array['a', 'c'];
SELECT '{"a": {"b": 1, "c": 2}}'::jsonb #- '{a,b}', '[1, [2, 3]]'::jsonb #- '{1,0}';
SELECT '{"a": 1}'::jsonb -> 1, '[1]'::jsonb -> 'a', '1'::jsonb -> 0;

-- construction
SELECT to_json('text'::text), to_jsonb(1.50), to_json(array[1, 2]), to_jsonb(true), to_json(NULL::int), to_jsonb(date '2024-01-02');
SELECT json_build_object('a', 1, 'b', array[1, 2], 'c', NULL), jsonb_build_object('z', 1, 'a', 2), json_build_array(1, 'x', NULL, true);
SELECT jsonb_build_object('a');
SELECT json_object('{a, 1, b, 2}'), jsonb_object('{{a, 1}, {b, 2}}'), json_object(array['k1', 'k2'], array['v1', 'v2']);
SELECT row_to_json(row(1, 'x')), to_jsonb(row(1, 'x'));
SELECT array_to_json(array[[1, 2], [3, 4]]), array_to_json(array[1, 2], true);
CREATE TABLE docs (id int PRIMARY KEY, body jsonb, meta json);
INSERT INTO docs VALUES
  (1, '{"name": "ann", "tags": ["a", "b"], "age": 31, "addr": {"city": "Oslo"}}', '{"v": 1}'),
  (2, '{"name": "bob", "tags": [], "age": null}', '{"v": 2}'),
  (3, '{"name": "cy", "tags": ["b"], "age": 22, "addr": {"city": "Rome"}}', NULL),
  (4, '[1, 2, 3]', '[]');
SELECT row_to_json(d) FROM docs d WHERE id = 1;
SELECT json_agg(id ORDER BY id), jsonb_agg(body -> 'name' ORDER BY id) FROM docs;
SELECT json_object_agg(id, body ->> 'name' ORDER BY id), jsonb_object_agg(body ->> 'name', id) FROM docs WHERE id < 4;
SELECT jsonb_agg(d ORDER BY id) FROM (SELECT id, body ->> 'name' AS name FROM docs WHERE id < 3) d;

-- querying
SELECT id, body ->> 'name' AS name, (body ->> 'age')::int AS age FROM docs ORDER BY id;
SELECT id FROM docs WHERE body @> '{"tags": ["b"]}' ORDER BY id;
SELECT id FROM docs WHERE body ? 'addr' ORDER BY id;
SELECT id, body #>> '{addr,city}' FROM docs WHERE body -> 'addr' IS NOT NULL ORDER BY id;
SELECT id, jsonb_typeof(body), jsonb_typeof(body -> 'age'), json_typeof(meta) FROM docs ORDER BY id;
SELECT id, jsonb_array_length(body -> 'tags') FROM docs WHERE jsonb_typeof(body -> 'tags') = 'array' ORDER BY id;
SELECT jsonb_array_length('{"a": 1}');
SELECT body ->> 'name', count(*) FROM docs, jsonb_array_elements_text(body -> 'tags') t GROUP BY 1 ORDER BY 1;
CREATE INDEX docs_body ON docs USING gin (body);
SELECT id FROM docs WHERE body @> '{"name": "cy"}';

-- set-returning
SELECT * FROM jsonb_each('{"a": 1, "b": [2], "c": null}');
SELECT * FROM json_each_text('{"a": 1, "b": "x", "c": null}');
SELECT * FROM jsonb_object_keys('{"b": 1, "a": 2}');
SELECT * FROM json_array_elements('[1, "two", {"three": 3}]');
SELECT value FROM jsonb_array_elements('[1, [2], null]') WITH ORDINALITY ORDER BY ordinality DESC;
SELECT * FROM jsonb_to_record('{"a": 1, "b": "x", "c": [1]}') AS r(a int, b text, c jsonb, d date);
SELECT * FROM jsonb_to_recordset('[{"a": 1}, {"a": 2, "b": "y"}]') AS r(a int, b text);
SELECT * FROM json_populate_record(NULL::docs, '{"id": 9, "body": {"x": 1}}');
SELECT jsonb_path_query_array('{"a": [1, 2, 3, 4]}', '$.a[*] ? (@ > 2)');

-- modification
SELECT jsonb_set('{"a": {"b": 1}}', '{a,b}', '2'), jsonb_set('{"a": 1}', '{c}', '3'), jsonb_set('{"a": 1}', '{c}', '3', false), jsonb_set('[1, 2]', '{5}', '9');
SELECT jsonb_insert('{"a": [1, 2]}', '{a,1}', '9'), jsonb_insert('{"a": [1, 2]}', '{a,1}', '9', true);
SELECT jsonb_set_lax('{"a": 1}', '{a}', NULL), jsonb_set_lax('{"a": 1}', '{a}', NULL, true, 'delete_key');
SELECT jsonb_strip_nulls('{"a": null, "b": {"c": null, "d": 1}, "e": [null]}'), json_strip_nulls('{"a": null, "b": 1}');
SELECT jsonb_pretty('{"a": [1, {"b": 2}], "c": {}}');
UPDATE docs SET body = jsonb_set(body, '{age}', to_jsonb((body ->> 'age')::int + 1)) WHERE id = 1 RETURNING body -> 'age';
UPDATE docs SET body = body || '{"seen": true}' WHERE id IN (2, 3);
SELECT id, body FROM docs WHERE body ? 'seen' ORDER BY id;

-- casts and comparisons
SELECT '1'::jsonb::int, '"x"'::jsonb::text, '1.5'::jsonb::numeric, 'true'::jsonb::bool, '{"a": 1}'::jsonb::json, '[1]'::json::jsonb;
SELECT '"x"'::jsonb::int;
SELECT '{"a": 1}'::jsonb < '{"a": 2}'::jsonb, '[1]'::jsonb < '{}'::jsonb, 'null'::jsonb < '1'::jsonb, '"a"'::jsonb < '1'::jsonb;
SELECT body FROM docs ORDER BY body;
SELECT json_typeof('1'), jsonb_typeof('null'), '{"a": 1}'::json::text, '{"b":1,   "a":2}'::jsonb::text;
DROP TABLE docs;
