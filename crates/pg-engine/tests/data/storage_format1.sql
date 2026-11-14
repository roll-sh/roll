-- A database whose files are all of block format 1 (made by a build before format 2, origin's
-- 248f9f8fa: tests/data/storage_format1.tar.gz), stopped cleanly: a table with a primary key,
-- an index of integers and one of text, out-of-line values, updated and deleted rows (their
-- versions compacted), a table loaded by COPY (its run linked and rewritten at the close), and a
-- truncated one.
CREATE TABLE f (id int PRIMARY KEY, v text, n int, big text);
CREATE INDEX f_n ON f (n);
CREATE INDEX f_v ON f (v);
INSERT INTO f SELECT i, 'value ' || i, i % 100, CASE WHEN i % 500 = 0 THEN repeat('x' || i, 1500) END FROM generate_series(1, 8000) i;
UPDATE f SET n = n + 1000 WHERE id % 3 = 0;
DELETE FROM f WHERE id % 7 = 0;
CREATE TABLE g (k bigint, w float8, s text);
\copy g FROM 'g.tsv'
CREATE INDEX g_k ON g (k);
CREATE TABLE h (a int);
INSERT INTO h SELECT generate_series(1, 1000);
TRUNCATE h;
INSERT INTO h VALUES (7);
VACUUM f;
ANALYZE;
