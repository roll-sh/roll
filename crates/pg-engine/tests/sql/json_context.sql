-- CONTEXT lines of JSON syntax errors: the input up to the offending token, on its line,
-- shortened to about 50 bytes.
SELECT '{"a": 1, "b": }'::json;
SELECT '{"a": 1 "b": 2}'::json;
SELECT '[1, 2, 3,]'::jsonb;
SELECT '{"aaaaaaaaaa": 1, "bbbbbbbbbbbb": 2, "cccccccccccc": 3, "dddddddddd": x}'::json;
SELECT '{"aaaaaaaaaa": 1, "bbbbbbbbbbbb": 2, "cccccccccccc": 3, "dddddddddd": 4, "e": tru, "f": 1}'::jsonb;
SELECT E'{\n  "a": 1,\n  "b": nul\n}'::json;
SELECT E'[1,\n2,\n'::json;
SELECT '"abc\q"'::json;
SELECT '"\u12"'::json;
SELECT '"\ud800"'::json;
SELECT '"\udc00x"'::jsonb;
SELECT E'"a\tb"'::json;
SELECT '01'::json;
SELECT '1.'::json;
SELECT '-'::json;
SELECT 'nulll'::json;
SELECT '{"a":1} x'::json;
SELECT '"é ü \u0000 tail"'::jsonb;
SELECT '[1, 2'::json;
SELECT '{'::json;
SELECT ''::json;
SELECT '   '::jsonb;
SELECT '{"a" 1}'::jsonb;
SELECT '{1: 2}'::jsonb;
SELECT json_extract_path('{"a": }', 'a');
SELECT '{"x": [1, 2, {"y": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz" , "w": ]}]}'::jsonb;
