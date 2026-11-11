-- A pseudoconstant ON clause of an inner join inside an outer join's nullable side gates only
-- that join; a field of an outer join's nullable whole row is its column; a LATERAL subquery
-- level takes only the join clauses over its lateral sources; a scan under a nested loop's
-- Materialize returns just the columns read; a constant on a FULL JOIN USING column
-- restricts both sides.
CREATE TABLE gpu_i8 (q1 int8, q2 int8);
INSERT INTO gpu_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
CREATE TABLE gpu_i4 (f1 int);
INSERT INTO gpu_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
CREATE TABLE gpu_txt (f1 text);
INSERT INTO gpu_txt VALUES ('doh!'), ('hi de ho neighbor');

SELECT x0.f1, EXISTS (
  SELECT * FROM gpu_i8 t1 LEFT JOIN
    (gpu_i8 t2 INNER JOIN gpu_i8 t3 ON x0.f1 = 1
     LEFT JOIN gpu_i8 t4 ON t2.q2 = t4.q2)
  ON t1.q1 = t2.q1
) FROM gpu_i4 x0 ORDER BY 1;
SELECT x0.f1, (SELECT count(*) FROM gpu_i8 t1 LEFT JOIN
    (gpu_i8 t2 INNER JOIN gpu_i8 t3 ON x0.f1 = 0
     LEFT JOIN gpu_i8 t4 ON t2.q2 = t4.q2)
  ON t1.q1 = t2.q1) FROM gpu_i4 x0 ORDER BY 1;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT (t2.*).q1 FROM gpu_i4 t1 LEFT JOIN gpu_i8 t2 ON t1.f1 = t2.q1;
SELECT (t2.*).q1, (t2.*).q2 IS NULL FROM gpu_i8 t1 LEFT JOIN gpu_i8 t2 ON t1.q2 = t2.q1 ORDER BY 1, 2;
CREATE FUNCTION gpu_field(t gpu_i8) RETURNS int8 AS $$ SELECT t.q2; $$ LANGUAGE sql IMMUTABLE;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT (t2.*).q1, gpu_field(t2) FROM gpu_i4 t1
    LEFT JOIN gpu_i8 t2 ON t1.f1 = t2.q1
    LEFT JOIN gpu_txt t3 ON true;

EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM gpu_txt t1 LEFT JOIN gpu_i8 i8 ON i8.q2 = 123,
  LATERAL (SELECT i8.q1, t2.f1 FROM gpu_txt t2 LIMIT 1) AS ss
WHERE t1.f1 = ss.f1;
SELECT * FROM gpu_txt t1 LEFT JOIN gpu_i8 i8 ON i8.q2 = 123,
  LATERAL (SELECT i8.q1, t2.f1 FROM gpu_txt t2 LIMIT 1) AS ss
WHERE t1.f1 = ss.f1;

CREATE TABLE gpu_t (unique1 int, unique2 int);
INSERT INTO gpu_t SELECT i, (i * 7919) % 10000 FROM generate_series(0, 9999) i;
CREATE INDEX gpu_t_unique2 ON gpu_t (unique2);
ANALYZE gpu_t;
EXPLAIN (COSTS OFF)
SELECT * FROM gpu_t a FULL JOIN gpu_t b USING (unique2) WHERE unique2 = 42;
SELECT a.unique1, b.unique1, unique2 FROM gpu_t a FULL JOIN gpu_t b USING (unique2) WHERE unique2 = 42;
SELECT * FROM (VALUES (1), (2)) a(x) FULL JOIN (VALUES (2), (3)) b(x) USING (x) WHERE x = 3;
SELECT * FROM (VALUES (1), (2)) a(x) FULL JOIN (VALUES (2), (3)) b(x) USING (x) WHERE x = 2;
