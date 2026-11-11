-- SQL/JSON path: canonical text
SELECT '$.a[*] ? (@ > 2)'::jsonpath, 'strict $.a.b'::jsonpath, '$.a + 1'::jsonpath, '-1'::jsonpath, 'lax $'::jsonpath;
SELECT '$ ? (@.a > -1 && !(@.b == "x") || exists(@.c))'::jsonpath, '$[1, 2 to 3, last]'::jsonpath, '$.**{2 to last}'::jsonpath;
SELECT '$.type().size()'::jsonpath, '$.a like_regex "^x" flag "i"'::jsonpath, '$.a starts with $x'::jsonpath, '($.a > 1) is unknown'::jsonpath;
SELECT '$.datetime("YYYY")'::jsonpath, '$.decimal(5,2)'::jsonpath, '1 + 2 * 3'::jsonpath, '(1 + 2) * 3'::jsonpath, '$."a b".*'::jsonpath;
SELECT '$var'::jsonpath, '1.50'::jsonpath, '1e3'::jsonpath, '$.A.TYPE()'::jsonpath, '$ - -1'::jsonpath, '- $.a'::jsonpath, '$.a.b ? (@ < 1).c'::jsonpath;
SELECT '0x1F'::jsonpath, '1.e2'::jsonpath, '.5'::jsonpath, '$.size'::jsonpath, '$."é\n"'::jsonpath, '$.**'::jsonpath, '$.**{1}'::jsonpath;
SELECT '$.a['::jsonpath;
SELECT '@.a'::jsonpath;
SELECT 'last'::jsonpath;
SELECT '$ ? (1)'::jsonpath;
SELECT '$.a == 1 == 2'::jsonpath;
SELECT '$ ? (@ +)'::jsonpath;
SELECT pg_typeof('$'::jsonpath), '$.a'::jsonpath::text, '$.a'::text::jsonpath;

-- querying
SELECT jsonb_path_query_array('{"a": [1, 2, 3, 4]}', '$.a[*] ? (@ > 2)'), jsonb_path_exists('{"a":1}', '$.b'), jsonb_path_match('{"a":1}', '$.a == 1'), jsonb_path_query_first('[1,2]', '$[*]');
SELECT jsonb_path_query_array('{"a":{"b":1,"c":[2,3]}}', '$.**'), jsonb_path_query_array('[1,"a",null,true,{},[]]', '$[*].type()'), jsonb_path_query_array('{"x":[1,2]}', 'lax $.x.size()');
SELECT jsonb_path_query_array('{"a":1,"b":2}', '$.keyvalue()'), jsonb_path_query_array('[1.5,-2.5]', '$[*].floor()'), jsonb_path_query_array('["1.5", 2]', '$[*].double()'), jsonb_path_query_array('[1.5,-2.5]', '$[*].ceiling().abs()');
SELECT jsonb_path_query_array('{"a":[1,2,3]}', '$.a[last, 0 to 1]'), jsonb_path_query_array('{"a":"abc"}', '$.a ? (@ like_regex "^A" flag "i")'), jsonb_path_query_array('[1,2,3]', '$[*] ? (@ > $min)', '{"min": 1}');
SELECT '{"a":[1,2]}'::jsonb @? '$.a[*] ? (@ == 2)', '{"a":[1,2]}'::jsonb @@ '$.a[*] == 2', '{"a":[1,2]}'::jsonb @@ '$.a[*] == 3', '{"a":1}'::jsonb @? 'strict $.b';
SELECT jsonb_path_query_array('{"a":[{"b":1},{"b":2}]}', '$.a.b'), jsonb_path_query_array('{"a":{"x":{"y":1}}}', '$.a.keyvalue()'), jsonb_path_query_array('{"a":{"x":{"y":1}, "z": {"q": 2.5}}}', '$.a.*.keyvalue()');
SELECT jsonb_path_query_array('[[1,[2]],3]', '$.**{1 to 2}'), jsonb_path_query_array('[1,2]', '$.size() + 1'), jsonb_path_query_array('[1, "x", null]', '$[*] ? (@ == null || @ starts with "x")'), jsonb_path_query_array('[1, "x"]', '$[*] ? ((@ > 0) is unknown)');
SELECT jsonb_path_query_array('{"a": [1, 2], "b": 3}', '$.a[*] + $.b'), jsonb_path_query_array('[1, 2, 3]', '$[*] ? (@ % 2 == 1)'), jsonb_path_query_array('{"a": 7}', '$.a / 2'), jsonb_path_query_array('[{"n": "x", "v": 1}, {"n": "y"}]', '$[*] ? (exists (@.v)).n');
SELECT jsonb_path_query_array('[1, [2, [3]]]', 'strict $[*]'), jsonb_path_query_array('[1, [2, [3]]]', 'lax $[*][*]'), jsonb_path_query_array('{"a": 1}', '$[0].a'), jsonb_path_query_array('[1]', '$[1.7]');
SELECT jsonb_path_query_array('["2024-01-02", "10:30:00", "2024-01-02T10:00:00", "2024-01-02 10:00:00+02"]', '$[*].datetime()'), jsonb_path_query_array('["2024-01-02", "2024-01-02 10:00:00+02"]', '$[*].datetime().type()');
SELECT jsonb_path_query_array('["2024-01-02", "2023-05-06"]', '$[*] ? (@.datetime() < "2024-01-01".datetime())'), jsonb_path_query_array('"12/31/2024"', '$.datetime("MM/DD/YYYY")');
SELECT jsonb_path_query_array('[1, "2", 3.7]', '$[*].integer()'), jsonb_path_query_array('["1.25", 2]', '$[*].number()'), jsonb_path_query_array('[1.255]', '$[*].decimal(4, 2)'), jsonb_path_query_array('[true, "yes", 0]', '$[*].boolean()'), jsonb_path_query_array('[1, true, "s"]', '$[*].string()');
SELECT * FROM jsonb_path_query('{"items": [{"id": 1, "tags": ["a"]}, {"id": 2, "tags": ["b", "c"]}]}', '$.items[*] ? (@.tags[*] == "c").id');
SELECT jsonb_path_query('{"a": [5, 6]}', '$.a[*]') AS v ORDER BY 1 DESC;
SELECT jsonb_path_exists('{"a": 1}', '$.b ? (@ > $x)', '{"x": 0}'), jsonb_path_match('{"a": 2}', '$.a > $x', vars => '{"x": 1}'), jsonb_path_query_first('[]', '$[0]');
SELECT jsonb_path_query('{"a": 1}', 'strict $.b', '{}', true), jsonb_path_match('[1,2]', '$[*]', '{}', true), jsonb_path_exists('[1]', 'strict $.a', silent => true);

-- errors
SELECT jsonb_path_query('{"a":1}', 'strict $.b');
SELECT jsonb_path_query('[1]', 'strict $.a');
SELECT jsonb_path_query('[1]', 'strict $[5]');
SELECT jsonb_path_query('[1,2]', '$[*] + 1');
SELECT jsonb_path_query('1', '$ / 0');
SELECT jsonb_path_query('1', '$x');
SELECT jsonb_path_match('[1,2]', '$[*]');
SELECT jsonb_path_query('"a"', '-$');
SELECT jsonb_path_query('"a"', '$.abs()');
SELECT jsonb_path_query('1', 'strict $.size()');
SELECT jsonb_path_query('1', 'strict $[*]');
SELECT jsonb_path_query('1', 'strict $.*');
SELECT jsonb_path_query('1', '$', '[]');
SELECT jsonb_path_query('[1, 2]', '$[$]');
SELECT jsonb_path_query('"x"', '$.datetime()');
SELECT jsonb_path_query('"abc"', '$.double()');

-- in tables
CREATE TABLE jp (id int, doc jsonb);
INSERT INTO jp VALUES (1, '{"name": "ann", "langs": ["en", "fr"], "age": 31}'), (2, '{"name": "bob", "langs": ["de"], "age": 17}'), (3, '{"name": "cy"}');
SELECT id FROM jp WHERE doc @? '$.langs[*] ? (@ == "fr")' ORDER BY id;
SELECT id, jsonb_path_query_first(doc, '$.age') FROM jp WHERE doc @@ '$.age >= 18' OR NOT doc ? 'age' ORDER BY id;
SELECT id, jsonb_path_query_array(doc, '$.langs[*] ? (@ starts with $p)', jsonb_build_object('p', 'e')) FROM jp ORDER BY id;
DROP TABLE jp;
