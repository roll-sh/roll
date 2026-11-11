-- to_json / to_jsonb of an array of a composite type stored in a table name the fields as the
-- type does (difftest seeds 5013, 5015, 5035 ...: they were f1, f2).
CREATE TYPE a104_ct AS (x int, y text);
CREATE TABLE a104_cj (id int, cs a104_ct[], c a104_ct);
INSERT INTO a104_cj VALUES (1, ARRAY[ROW(1, 'a')::a104_ct, ROW(NULL, 'b')::a104_ct], ROW(2, 'c')), (2, '{}', NULL), (3, NULL, ROW(NULL, NULL)), (4, '{"(5,q)",NULL}', '(6,r)');
SELECT id, to_jsonb(cs), to_json(cs), array_to_json(cs), to_jsonb(c), row_to_json(cs[1]) FROM a104_cj ORDER BY id;
SELECT to_jsonb(ARRAY[ROW(1, 'a')::a104_ct]), to_json(ARRAY[ROW(1, 'a')::a104_ct]);
SELECT jsonb_agg(cs ORDER BY id), json_agg(cs ORDER BY id) FROM a104_cj;
SELECT jsonb_build_object('k', cs), json_build_array(cs) FROM a104_cj WHERE id = 1;
SELECT to_jsonb(ARRAY[[ROW(1, 'a')::a104_ct], [ROW(2, 'b')::a104_ct]]);
DROP TABLE a104_cj;
DROP TYPE a104_ct;
