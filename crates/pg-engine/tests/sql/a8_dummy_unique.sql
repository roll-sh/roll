-- A relation found empty (a partitioned table without partitions, or read ONLY; a table
-- whose quals are false) still has its unique indexes: a join's inner side over it is
-- unique (Inner Unique), as rel_supports_distinctness sees its indexlist.
CREATE TABLE a8_du_t (tid integer PRIMARY KEY) PARTITION BY LIST (tid);
CREATE TABLE a8_du_tp PARTITION OF a8_du_t DEFAULT;
CREATE TABLE a8_du_s (sid integer);
INSERT INTO a8_du_s VALUES (1), (2);
DROP TABLE a8_du_tp;
EXPLAIN (VERBOSE, COSTS OFF)
MERGE INTO a8_du_t t USING a8_du_s s ON t.tid = s.sid
  WHEN NOT MATCHED THEN INSERT VALUES (s.sid);
MERGE INTO a8_du_t t USING a8_du_s s ON t.tid = s.sid
  WHEN NOT MATCHED THEN INSERT VALUES (s.sid);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_du_s s LEFT JOIN a8_du_t t ON t.tid = s.sid;
SELECT * FROM a8_du_s s LEFT JOIN a8_du_t t ON t.tid = s.sid ORDER BY 1;
CREATE TABLE a8_du_p (tid integer PRIMARY KEY) PARTITION BY LIST (tid);
CREATE TABLE a8_du_p1 PARTITION OF a8_du_p FOR VALUES IN (1);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT * FROM a8_du_s s LEFT JOIN ONLY a8_du_p t ON t.tid = s.sid;
CREATE TABLE a8_du_k (k integer PRIMARY KEY, v int);
SELECT * FROM a8_du_s s LEFT JOIN (SELECT * FROM a8_du_k WHERE false) k ON k.k = s.sid ORDER BY 1;
DROP TABLE a8_du_k;
DROP TABLE a8_du_p;
DROP TABLE a8_du_s;
DROP TABLE a8_du_t;
