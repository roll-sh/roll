-- AND / OR / NOT batch filters (the outcomes folded a row at a time without a branch: `VB::eval`)
-- over NULL columns: three and more arguments, nested, under NOT, in a parallel aggregating scan,
-- against the rows PostgreSQL gives.
CREATE TABLE a267f_o (oid int8, cid int, amt numeric(12,2), tag text, note varchar(79)) WITH (autovacuum_enabled = false);
INSERT INTO a267f_o SELECT g,
  CASE WHEN g % 211 = 0 THEN NULL ELSE (g * 7) % 3600 END,
  CASE WHEN g % 13 = 0 THEN NULL ELSE (g % 1000) * 0.5 END,
  't' || (g % 5),
  CASE WHEN g % 37 = 0 THEN 'very special requests ' || g ELSE 'plain note ' || (g % 101) END
  FROM generate_series(1, 40000) g;
VACUUM ANALYZE a267f_o;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT count(*), sum(amt) FROM a267f_o WHERE cid > 100 AND amt < 200 AND note NOT LIKE '%special%';
SELECT count(*), sum(amt) FROM a267f_o WHERE cid > 3000 OR amt < 20 OR note LIKE 'very%';
SELECT count(*), sum(amt) FROM a267f_o WHERE (cid > 100 AND amt < 200) OR (cid < 50 AND amt > 400);
SELECT count(*), sum(amt) FROM a267f_o WHERE NOT (cid < 1000 OR amt > 300) AND oid > 5;
SELECT count(*), sum(amt) FROM a267f_o WHERE (cid > 100 OR amt < 200) AND (cid < 3500 OR amt > 100);
SELECT tag, count(*) FROM a267f_o WHERE NOT (cid > 100 AND amt < 200) OR note LIKE 'very%' GROUP BY tag ORDER BY 1;
DROP TABLE a267f_o;
