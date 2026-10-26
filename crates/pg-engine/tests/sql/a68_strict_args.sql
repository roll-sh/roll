-- A strict function or operator evaluates all its arguments before a NULL one makes it NULL
-- (EEOP_FUNCEXPR_STRICT), so a later argument's error still fails the statement.
CREATE TABLE a68s (id int, n int, z int, b jsonb);
INSERT INTO a68s VALUES (1, NULL, 0, 'null'), (2, 5, 1, '{"a": 1}');
SELECT id, (b #> '{a,b}') || (b - 'a') FROM a68s ORDER BY id;
SELECT id, n + 10 / z FROM a68s ORDER BY id;
SELECT id, n + 10 / z FROM a68s WHERE id = 2;
SELECT id, power(n, 10 / z) FROM a68s ORDER BY id;
SELECT id, substr(NULLIF(b::text, 'null'), 1, 10 / z) FROM a68s ORDER BY id;
UPDATE a68s SET b = (b #> '{a,b}') || (b - 'a');
SELECT id, b FROM a68s ORDER BY id;
SELECT id, n + z FROM a68s ORDER BY id;
SELECT id, lpad(NULL, 3, (10 / z)::text) FROM a68s WHERE id = 2;
DROP TABLE a68s;
