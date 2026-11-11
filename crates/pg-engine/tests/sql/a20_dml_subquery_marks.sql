-- Round 16 (agent 20): an UPDATE / DELETE / MERGE joined with a FROM subquery planned apart
-- gives that subquery a whole-row mark (ROW_MARK_COPY, preprocess_rowmarks): its Subquery
-- Scan returns `o.*` up the plan, and the relations inside it have no marks of their own
-- (even the target's table read again in it).
CREATE TABLE a20_m (k int UNIQUE, v text);
INSERT INTO a20_m SELECT i, i || ' v' FROM generate_series(1, 16, 3) i;
CREATE TABLE a20_o (k int, v text, w int);
INSERT INTO a20_o VALUES (1, 'o1', 10), (4, 'o4', 40), (100, 'o100', 1000);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a20_m m USING (SELECT 0 k, 'merge source' v OFFSET 0) o ON m.k = o.k
WHEN MATCHED THEN UPDATE SET v = 'upd'
WHEN NOT MATCHED THEN INSERT VALUES (o.k, o.v);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a20_m m USING (SELECT k, v FROM a20_o OFFSET 0) o ON m.k = o.k
WHEN MATCHED THEN UPDATE SET v = o.v
WHEN NOT MATCHED THEN INSERT VALUES (o.k, o.v);
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE a20_m m SET v = o.v FROM (SELECT k, v FROM a20_m OFFSET 0) o WHERE m.k = o.k;
EXPLAIN (VERBOSE, COSTS OFF)
DELETE FROM a20_m m USING (SELECT k FROM a20_m OFFSET 0) o WHERE m.k = o.k;
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a20_m m USING (SELECT k, v, w FROM a20_o OFFSET 0) o ON m.k = o.k
WHEN MATCHED AND o.w > 10 THEN UPDATE SET v = o.v
WHEN NOT MATCHED THEN INSERT (v, k) VALUES (o.v || o.w, o.k);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a20_m m USING a20_o o ON m.k = o.k
WHEN NOT MATCHED THEN INSERT VALUES (o.w, o.v)
WHEN MATCHED THEN DELETE;
-- Pulled up: the subquery's relations are the statement's, with their own marks.
EXPLAIN (VERBOSE, COSTS OFF)
UPDATE a20_m m SET v = o.v FROM (SELECT k, v FROM a20_o) o WHERE m.k = o.k;
-- And the rows.
MERGE INTO a20_m m USING (SELECT k, v FROM a20_o OFFSET 0) o ON m.k = o.k
WHEN MATCHED THEN UPDATE SET v = o.v
WHEN NOT MATCHED THEN INSERT VALUES (o.k, o.v);
UPDATE a20_m m SET v = o.v || '!' FROM (SELECT k, v FROM a20_m OFFSET 0) o WHERE m.k = o.k AND m.k < 5;
DELETE FROM a20_m m USING (SELECT k FROM a20_o OFFSET 0) o WHERE m.k = o.k AND o.k > 50;
SELECT * FROM a20_m ORDER BY k;
DROP TABLE a20_m, a20_o;
