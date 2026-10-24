-- A pulled-up subquery's constant output read in an outer join's own ON clause, through
-- joins that don't null it, is the constant itself (no PlaceHolderVar there): the clause
-- may read only the other side, a join filter of the outer join. Below the node computing
-- the target list a join returns its inputs' columns, then the PlaceHolderVars it passes up
-- or computes; a Materialize over a join returns what the join does.
CREATE TABLE a19_t (f1 text);
CREATE TABLE a19_i8 (q1 int8, q2 int8);
CREATE TABLE a19_i4 (f1 int4);
INSERT INTO a19_t VALUES ('doh!'), ('hi de ho neighbor');
INSERT INTO a19_i8 VALUES (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
INSERT INTO a19_i4 VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
ANALYZE a19_t;
ANALYZE a19_i8;
ANALYZE a19_i4;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.* FROM a19_t t1 LEFT JOIN (SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
  ON (t1.f1 = b1.d1);
SELECT t1.* FROM a19_t t1 LEFT JOIN (SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
  ON (t1.f1 = b1.d1);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.*, i8.q2 FROM a19_t t1
  LEFT JOIN ((SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
             LEFT JOIN a19_i8 i8 ON (i8.q1 = b1.q2))
  ON (t1.f1 = b1.d1);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.*, i8.q2 FROM
  a19_t t1
  LEFT JOIN (SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
    LEFT JOIN a19_i8 i8
      LEFT JOIN (SELECT *, null::int AS d2 FROM a19_i8 i8b2) b2
      ON (i8.q1 = b2.q1)
    ON (b2.d2 = b1.q2)
  ON (t1.f1 = b1.d1);
SELECT t1.*, i8.q2 FROM
  a19_t t1
  LEFT JOIN (SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
    LEFT JOIN a19_i8 i8
      LEFT JOIN (SELECT *, null::int AS d2 FROM a19_i8 i8b2) b2
      ON (i8.q1 = b2.q1)
    ON (b2.d2 = b1.q2)
  ON (t1.f1 = b1.d1)
ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.*, i8.q2 FROM
  a19_t t1
  LEFT JOIN (SELECT *, '***'::text AS d1 FROM a19_i8 i8b1) b1
    LEFT JOIN a19_i8 i8
      LEFT JOIN (SELECT *, null::int AS d2 FROM a19_i8 i8b2, a19_i4 i4b2
                 WHERE q1 = f1) b2
      ON (i8.q1 = b2.q1)
    ON (b2.d2 = b1.q2)
  ON (t1.f1 = b1.d1);
-- Nulled inside the side (its own outer join): a PlaceHolderVar still.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT t1.* FROM a19_t t1
  LEFT JOIN (a19_i8 i8 LEFT JOIN (SELECT *, 'doh!'::text AS d1 FROM a19_i8 i8b1) b1 ON (i8.q1 = b1.q2))
  ON (t1.f1 = b1.d1);
SELECT t1.*, i8.q1 FROM a19_t t1
  LEFT JOIN (a19_i8 i8 LEFT JOIN (SELECT *, 'doh!'::text AS d1 FROM a19_i8 i8b1) b1 ON (i8.q1 = b1.q2))
  ON (t1.f1 = b1.d1) ORDER BY 1, 2;
