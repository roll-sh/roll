-- Nested loops over a parameterized inner index scan: the inner side is rescanned for each
-- outer row and read only as far as the join asks (ExecReScan, not collected first).
CREATE TABLE a160_o (id int, k int, lim int);
CREATE TABLE a160_i (id int, k int, v int, t text);
INSERT INTO a160_o SELECT g, g % 7, g % 5 FROM generate_series(1, 40) g;
INSERT INTO a160_o VALUES (41, NULL, 1), (42, 100, 0);
INSERT INTO a160_i SELECT g, g % 7, g * 3, 'r' || g FROM generate_series(1, 200) g;
CREATE INDEX a160_i_k ON a160_i (k, id);
CREATE INDEX a160_i_v ON a160_i USING hash (v);
ANALYZE a160_o, a160_i;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
SET enable_memoize = off;
SET enable_seqscan = off;
SET enable_bitmapscan = off;

-- inner join, the inner filter reading the outer row
EXPLAIN (COSTS OFF)
SELECT o.id, i.id FROM a160_o o JOIN a160_i i ON i.k = o.k AND i.id < o.id * 2;
SELECT o.id, count(*), sum(i.v), min(i.t) FROM a160_o o JOIN a160_i i ON i.k = o.k AND i.id < o.id * 2
GROUP BY o.id ORDER BY o.id;
-- the outer rows' order, each one's inner rows in index order
SELECT o.id, i.id, i.v FROM a160_o o JOIN a160_i i ON i.k = o.k WHERE o.id IN (3, 10, 41, 42) AND i.id < 40;
-- left, semi and anti joins
SELECT o.id, i.id FROM a160_o o LEFT JOIN a160_i i ON i.k = o.k AND i.id = o.id + 7 WHERE o.id > 30 ORDER BY o.id;
EXPLAIN (COSTS OFF)
SELECT o.id FROM a160_o o WHERE EXISTS (SELECT 1 FROM a160_i i WHERE i.k = o.k AND i.v > o.id * 10);
SELECT o.id FROM a160_o o WHERE EXISTS (SELECT 1 FROM a160_i i WHERE i.k = o.k AND i.v > o.id * 10);
SELECT o.id FROM a160_o o WHERE NOT EXISTS (SELECT 1 FROM a160_i i WHERE i.k = o.k AND i.v > o.id * 15);
-- a hash index, and a backward inner scan
SELECT o.id, i.id FROM a160_o o JOIN a160_i i ON i.v = o.id * 3 WHERE o.id < 12;
SELECT o.id, (SELECT i.id FROM a160_i i WHERE i.k = o.k ORDER BY i.k DESC, i.id DESC LIMIT 1) FROM a160_o o
WHERE o.id < 9;
-- LATERAL with a LIMIT read from the outer row
SELECT o.id, l.id FROM a160_o o, LATERAL (SELECT i.id FROM a160_i i WHERE i.k = o.k ORDER BY i.k, i.id LIMIT o.lim) l
WHERE o.id BETWEEN 5 AND 12;

-- the inner rows are read only as far as the join asks: a function's notices show how far
CREATE FUNCTION a160_seen(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS
$$ BEGIN RAISE NOTICE 'inner %', $1; RETURN true; END $$;
SELECT o.id, i.id FROM a160_o o JOIN a160_i i ON i.k = o.k AND a160_seen(i.id) WHERE o.id IN (1, 2) LIMIT 3;
SELECT o.id FROM a160_o o WHERE o.id IN (1, 2) AND EXISTS (SELECT 1 FROM a160_i i WHERE i.k = o.k AND a160_seen(i.id));
-- an error in the inner side after joined rows were returned
SELECT o.id, i.id, 10 / (i.id - 50) FROM a160_o o JOIN a160_i i ON i.k = o.k WHERE o.id IN (1, 8) AND a160_seen(i.id);
-- a cursor reading part of the join, then the rest
BEGIN;
DECLARE c CURSOR FOR SELECT o.id, i.id FROM a160_o o JOIN a160_i i ON i.k = o.k WHERE o.id IN (2, 9) AND i.id < 30;
FETCH 3 FROM c;
FETCH ALL FROM c;
COMMIT;

-- each rescan counts an index scan
SELECT pg_stat_force_next_flush();
CREATE TEMP TABLE a160_before AS SELECT idx_scan FROM pg_stat_user_indexes WHERE indexrelname = 'a160_i_k';
SELECT count(*) FROM a160_o o JOIN a160_i i ON i.k = o.k AND i.id < 20;
SELECT pg_stat_force_next_flush();
SELECT s.idx_scan - b.idx_scan AS scans FROM pg_stat_user_indexes s, a160_before b WHERE s.indexrelname = 'a160_i_k';
DROP FUNCTION a160_seen(int);
DROP TABLE a160_o, a160_i;
