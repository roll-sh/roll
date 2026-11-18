-- A scan's rows after a filter that reads columns nothing above reads (those not decoded once the
-- filter passed in place, a LIKE reading its string where it is): the same column filtered and
-- returned, joined, grouped and counted, NULLs, multibyte and out-of-line strings, serial and
-- parallel scans, against the rows PostgreSQL gives.
CREATE TABLE a258_l (id int, cust int, note text, tag varchar(12), amt numeric(12,2)) WITH (autovacuum_enabled = false);
INSERT INTO a258_l SELECT s, s % 997,
  CASE WHEN s % 41 = 0 THEN NULL WHEN s % 9 = 0 THEN 'ab special é requests ' || s WHEN s % 1000 = 7 THEN repeat('special x requests ', 400) || s ELSE 'plain note ' || s END,
  CASE WHEN s % 13 = 0 THEN NULL ELSE 't' || (s % 17) END,
  (s % 777) * 0.5
  FROM generate_series(1, 90000) s;
CREATE TABLE a258_c (cust int, name text) WITH (autovacuum_enabled = false);
INSERT INTO a258_c SELECT s, 'c' || s FROM generate_series(0, 1199) s;
VACUUM ANALYZE a258_l;
VACUUM ANALYZE a258_c;
SELECT count(*), sum(amt), sum(cust) FROM a258_l WHERE note NOT LIKE '%special%requests%';
SELECT count(*), sum(amt) FROM a258_l WHERE note LIKE '%é%' OR tag LIKE 't1%';
SELECT tag, count(*), sum(amt) FROM a258_l WHERE note LIKE 'plain%' GROUP BY tag ORDER BY tag;
SELECT c_count, count(*) AS custdist FROM (SELECT c.cust, count(l.id) FROM a258_c c LEFT JOIN a258_l l ON l.cust = c.cust AND l.note NOT LIKE '%special%requests%' GROUP BY c.cust) AS x (cust, c_count) GROUP BY c_count ORDER BY custdist DESC, c_count DESC LIMIT 15;
SELECT id, note FROM a258_l WHERE note LIKE '%special é%' AND id < 200 ORDER BY id;
SELECT id, length(note), tag FROM a258_l WHERE note LIKE '%requests 1007%' ORDER BY id;
SELECT id, cust FROM a258_l WHERE tag LIKE 't16' AND note NOT LIKE 'plain%' AND id < 2000 ORDER BY id;
SET max_parallel_workers_per_gather = 0;
SELECT count(*), sum(amt) FROM a258_l WHERE note NOT LIKE '%special%requests%' AND tag IS NOT NULL;
SELECT id, amt FROM a258_l WHERE note LIKE 'ab%' AND id < 120 ORDER BY id;
RESET max_parallel_workers_per_gather;
DROP TABLE a258_c;
DROP TABLE a258_l;
