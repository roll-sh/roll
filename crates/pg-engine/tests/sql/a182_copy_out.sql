-- COPY TO of a table writes integers, floats and strings from the stored rows; the rest, and
-- rows stored before a column was added, from their values: the text is PostgreSQL's either way.
CREATE TABLE a182_o (i int2, j int4, k int8, f float8, t text, v varchar(10), n numeric, d date);
INSERT INTO a182_o VALUES
  (1, -2, 9223372036854775807, 0.1, 'plain', 'v', 1.5, '2024-01-02'),
  (-32768, 2147483647, -9223372036854775808, -1e300, E'tab\there', E'nl\nx', NULL, NULL),
  (NULL, NULL, NULL, 'NaN', E'back\\slash', '', -0.0, 'infinity'),
  (0, 0, 0, '-Infinity', 'ünï ✓', NULL, 1e-20, '1999-12-31'),
  (7, 7, 7, 1.5e-7, E'del|im', 'a|b', 7, '2000-02-29');
COPY a182_o TO STDOUT;
COPY a182_o TO STDOUT (DELIMITER '|');
COPY a182_o TO STDOUT (DELIMITER '-');
COPY a182_o TO STDOUT (NULL 'NUL');
COPY a182_o TO STDOUT (FORMAT csv);
COPY a182_o (t, j) TO STDOUT;
SET extra_float_digits = 0;
COPY a182_o (f) TO STDOUT;
RESET extra_float_digits;
-- Rows stored before a column was added, and a dropped column.
ALTER TABLE a182_o ADD COLUMN z int DEFAULT 42;
INSERT INTO a182_o (i, z) VALUES (8, 8);
COPY a182_o (i, z) TO STDOUT;
ALTER TABLE a182_o DROP COLUMN n;
COPY a182_o TO STDOUT;
CREATE TABLE a182_g (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL, c text);
INSERT INTO a182_g (a, c) VALUES (1, 'x'), (2, NULL);
COPY a182_g TO STDOUT;
COPY (SELECT * FROM a182_g) TO STDOUT;
DROP TABLE a182_o, a182_g;
