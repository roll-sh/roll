CREATE TYPE gx_fn_pt AS (name text, n int);
CREATE TABLE gx_fn_people (p gx_fn_pt, hobbies text);
INSERT INTO gx_fn_people VALUES (ROW('bob', 3), 'golf');
SELECT name(p), n(p) FROM gx_fn_people;
SELECT name(gx_fn_people.p) FROM gx_fn_people;
SELECT hobbies(gx_fn_people) FROM gx_fn_people;
SELECT nosuch(p) FROM gx_fn_people;
SELECT name('abc'::text);
DROP TABLE gx_fn_people; DROP TYPE gx_fn_pt;
