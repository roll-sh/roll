-- A parallel scan's one hash join probe and its partial aggregation done from the batch's
-- columns (TPC-H q13's shape: orders, nearly all passing a NOT LIKE, probed into customer and
-- counted per customer): right and inner joins, duplicate and missing inner keys, NULL outer
-- keys and NULL aggregated values, groups by outer and inner columns, a string kept from the
-- outer row, rows stored before ADD COLUMN, against the rows PostgreSQL gives.
CREATE TABLE a267_c (id int, grp int, name text) WITH (autovacuum_enabled = false);
INSERT INTO a267_c SELECT g, g % 7, 'c' || g FROM generate_series(1, 3000) g;
INSERT INTO a267_c SELECT g, 100 + g % 3, 'dup' || g FROM generate_series(1, 3000, 97) g;
CREATE TABLE a267_o (oid int8, cid int, amt numeric(12,2), tag text, note varchar(79)) WITH (autovacuum_enabled = false);
INSERT INTO a267_o SELECT g,
  CASE WHEN g % 211 = 0 THEN NULL ELSE (g * 7) % 3600 END,
  CASE WHEN g % 13 = 0 THEN NULL ELSE (g % 1000) * 0.5 END,
  't' || (g % 5),
  CASE WHEN g % 37 = 0 THEN 'very special requests ' || g ELSE 'plain note ' || (g % 101) END
  FROM generate_series(1, 40000) g;
VACUUM ANALYZE a267_c;
VACUUM ANALYZE a267_o;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT c_count, count(*) AS custdist FROM (
  SELECT c.id, count(o.oid) FROM a267_c c LEFT OUTER JOIN a267_o o ON c.id = o.cid AND o.note NOT LIKE '%special%requests%'
  GROUP BY c.id) AS x (id, c_count)
GROUP BY c_count ORDER BY custdist DESC, c_count DESC;
SELECT c.grp, count(*), count(o.amt), sum(o.amt) FROM a267_o o JOIN a267_c c ON o.cid = c.id
  WHERE o.note NOT LIKE '%special%' GROUP BY c.grp ORDER BY 1;
SELECT o.tag, count(*), sum(o.amt), max(c.id), min(c.grp) FROM a267_o o JOIN a267_c c ON o.cid = c.id
  WHERE o.note LIKE 'plain%' GROUP BY o.tag ORDER BY 1;
SELECT count(*), count(o.oid), sum(o.amt) FROM a267_c c LEFT JOIN a267_o o ON o.cid = c.id;
SELECT c.name, count(o.oid) FROM a267_c c LEFT JOIN a267_o o ON o.cid = c.id AND o.note NOT LIKE '%a%'
  GROUP BY c.name ORDER BY 2 DESC, 1 LIMIT 8;
ALTER TABLE a267_o ADD COLUMN pri int DEFAULT 3;
INSERT INTO a267_o SELECT g, g % 3100, g * 0.25, 't9', 'late note' FROM generate_series(40001, 46000) g;
UPDATE a267_o SET pri = 4 WHERE oid % 1000 = 0;
SELECT o.pri, count(*), sum(o.amt), count(DISTINCT c.grp) FROM a267_o o JOIN a267_c c ON o.cid = c.id
  WHERE o.note NOT LIKE '%special%' GROUP BY o.pri ORDER BY 1;
SELECT c_count, count(*) AS custdist FROM (
  SELECT c.id, count(o.pri) FROM a267_c c LEFT OUTER JOIN a267_o o ON c.id = o.cid AND o.note NOT LIKE '%special%requests%'
  GROUP BY c.id) AS x (id, c_count)
GROUP BY c_count ORDER BY custdist DESC, c_count DESC;
DROP TABLE a267_o;
DROP TABLE a267_c;
