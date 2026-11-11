-- VERBOSE target lists of UPDATE / DELETE / MERGE over a join: the processed target list
-- (new values, the target's row identity, the other relations' row marks: their ctid)
-- and each scan's reltarget order below it; Inner Unique over a partitioned table's
-- unique index.
CREATE TABLE a3_rm_t (a int, b int, c int);
CREATE TABLE a3_rm_s (a int, b int);
INSERT INTO a3_rm_t SELECT g, g, g FROM generate_series(1, 5) g;
INSERT INTO a3_rm_s SELECT g, g * 10 FROM generate_series(3, 7) g;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE a3_rm_t SET b = a3_rm_s.b FROM a3_rm_s WHERE a3_rm_t.a = a3_rm_s.a;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE a3_rm_t SET c = a3_rm_s.b + 1, b = 0 FROM a3_rm_s WHERE a3_rm_t.a = a3_rm_s.a;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM a3_rm_t USING a3_rm_s WHERE a3_rm_t.a = a3_rm_s.a;
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a3_rm_t t USING a3_rm_s s ON t.a = s.a WHEN MATCHED THEN UPDATE SET b = s.b;
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a3_rm_t t USING a3_rm_s s ON t.a = s.a WHEN NOT MATCHED THEN INSERT VALUES (s.a);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a3_rm_t t USING (SELECT * FROM a3_rm_s) s ON t.a = s.a WHEN MATCHED THEN DELETE;
UPDATE a3_rm_t SET b = a3_rm_s.b FROM a3_rm_s WHERE a3_rm_t.a = a3_rm_s.a;
DELETE FROM a3_rm_t USING a3_rm_s WHERE a3_rm_t.a = a3_rm_s.a AND a3_rm_s.b > 40;
MERGE INTO a3_rm_t t USING a3_rm_s s ON t.a = s.a WHEN NOT MATCHED THEN INSERT VALUES (s.a);
SELECT * FROM a3_rm_t ORDER BY a;

CREATE TABLE a3_rm_p (tid integer PRIMARY KEY) PARTITION BY LIST (tid);
CREATE TABLE a3_rm_pp PARTITION OF a3_rm_p DEFAULT;
CREATE TABLE a3_rm_p2 (tid integer PRIMARY KEY) PARTITION BY LIST (tid);
CREATE TABLE a3_rm_p2a PARTITION OF a3_rm_p2 FOR VALUES IN (1);
CREATE TABLE a3_rm_p2b PARTITION OF a3_rm_p2 FOR VALUES IN (2);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a3_rm_p t USING a3_rm_s s ON t.tid = s.a WHEN NOT MATCHED THEN INSERT VALUES (s.a);
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a3_rm_p t USING a3_rm_s s ON t.tid = s.a WHEN MATCHED THEN DELETE;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a3_rm_s s LEFT JOIN a3_rm_p t ON t.tid = s.a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a3_rm_s s LEFT JOIN a3_rm_p2 t ON t.tid = s.a;
MERGE INTO a3_rm_p t USING a3_rm_s s ON t.tid = s.a WHEN NOT MATCHED THEN INSERT VALUES (s.a);
SELECT * FROM a3_rm_p ORDER BY tid;
DROP TABLE a3_rm_t, a3_rm_s, a3_rm_p, a3_rm_p2;
