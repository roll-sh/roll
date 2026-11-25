-- An index scan's filter holds conditions its key range applies exactly and others: the rows
-- are checked against the others (the same rows as a sequential scan's, NULLs included).
CREATE TABLE a319_l (pk int, sk int, q numeric, d date, note text);
INSERT INTO a319_l SELECT g % 40 + 1, g % 7, g % 50, date '1994-01-01' + (g * 37) % 700, CASE WHEN g % 11 = 0 THEN NULL ELSE 'n' || g % 5 END FROM generate_series(1, 4000) g;
INSERT INTO a319_l VALUES (3, 2, NULL, NULL, NULL), (3, 2, 5, NULL, 'x'), (NULL, 2, 7, date '1994-03-01', 'y');
CREATE INDEX a319_l_ps ON a319_l (pk, sk);
CREATE TABLE a319_ps (pk int, sk int, avail int);
INSERT INTO a319_ps SELECT p, s, (p * 13 + s * 7) % 60 FROM generate_series(1, 42) p, generate_series(0, 7) s;
ANALYZE a319_l, a319_ps;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
-- TPC-H q20's SubPlan: both index columns from the outer row, a date range in the filter
SELECT ps.pk, ps.sk, ps.avail FROM a319_ps ps
WHERE ps.pk <= 6 AND ps.avail > (SELECT 0.08 * sum(l.q) FROM a319_l l WHERE l.pk = ps.pk AND l.sk = ps.sk
  AND l.d >= date '1994-01-01' AND l.d < date '1994-01-01' + interval '1' year)
ORDER BY 1, 2;
SELECT ps.pk, ps.sk, (SELECT count(*) FROM a319_l l WHERE l.pk = ps.pk AND l.sk = ps.sk AND (l.note = 'n2' OR l.q IS NULL OR l.q < 3))
FROM a319_ps ps WHERE ps.pk IN (3, 41) ORDER BY 1, 2;
-- one index column applied, the other condition on the second index column not exactly (a cast)
SELECT count(*), sum(q) FROM a319_l WHERE pk = 3 AND sk::numeric = 2 AND d IS NULL;
SELECT pk, sk, q, d, note FROM a319_l WHERE pk = 3 AND sk = 2 AND (q IS NULL OR d IS NULL) ORDER BY q NULLS FIRST, d;
-- a range bound applied, an equality on a later column checked
SELECT count(*), min(d), max(d) FROM a319_l WHERE pk >= 38 AND sk = 4 AND note LIKE 'n%';
-- the same rows without the index
RESET enable_seqscan;
SET enable_indexscan = off;
SELECT ps.pk, ps.sk, ps.avail FROM a319_ps ps
WHERE ps.pk <= 6 AND ps.avail > (SELECT 0.08 * sum(l.q) FROM a319_l l WHERE l.pk = ps.pk AND l.sk = ps.sk
  AND l.d >= date '1994-01-01' AND l.d < date '1994-01-01' + interval '1' year)
ORDER BY 1, 2;
SELECT count(*), min(d), max(d) FROM a319_l WHERE pk >= 38 AND sk = 4 AND note LIKE 'n%';
