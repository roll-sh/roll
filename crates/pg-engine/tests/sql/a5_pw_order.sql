-- ORDER BY … LIMIT over a partitionwise join: a Merge Append of child joins planned in the
-- order; a hashed grouping with a Sort above when that costs less than sorting for a
-- GroupAggregate; an anti join's clause as written in a parameterized scan's filter.
SET enable_partitionwise_join = on;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE a5_fract (id bigint, PRIMARY KEY (id)) PARTITION BY RANGE (id);
CREATE TABLE a5_fract0 PARTITION OF a5_fract FOR VALUES FROM ('0') TO ('1000');
CREATE TABLE a5_fract1 PARTITION OF a5_fract FOR VALUES FROM ('1000') TO ('2000');
INSERT INTO a5_fract (id) SELECT generate_series(0, 1999);
ANALYZE a5_fract;
EXPLAIN (COSTS OFF)
SELECT x.id, y.id FROM a5_fract x LEFT JOIN a5_fract y USING (id) ORDER BY x.id ASC LIMIT 10;
SELECT x.id, y.id FROM a5_fract x LEFT JOIN a5_fract y USING (id) ORDER BY x.id ASC LIMIT 10;
EXPLAIN (COSTS OFF)
SELECT x.id, y.id FROM a5_fract x LEFT JOIN a5_fract y USING (id) ORDER BY x.id DESC LIMIT 10;
SELECT x.id, y.id FROM a5_fract x LEFT JOIN a5_fract y USING (id) ORDER BY x.id DESC LIMIT 10;
EXPLAIN (COSTS OFF)
SELECT x.id, y.id FROM a5_fract x LEFT JOIN a5_fract y USING (id) ORDER BY x.id DESC LIMIT 2;
CREATE TABLE a5_plt1 (a int, b int, c text) PARTITION BY LIST (c);
CREATE TABLE a5_plt1_p1 PARTITION OF a5_plt1 FOR VALUES IN ('0000', '0003', '0004', '0010');
CREATE TABLE a5_plt1_p2 PARTITION OF a5_plt1 FOR VALUES IN ('0001', '0005', '0002', '0009');
CREATE TABLE a5_plt1_p3 PARTITION OF a5_plt1 FOR VALUES IN ('0006', '0007', '0008', '0011');
INSERT INTO a5_plt1 SELECT i, i, to_char(i/50, 'FM0000') FROM generate_series(0, 599, 2) i;
ANALYZE a5_plt1;
CREATE TABLE a5_plt2 (a int, b int, c text) PARTITION BY LIST (c);
CREATE TABLE a5_plt2_p1 PARTITION OF a5_plt2 FOR VALUES IN ('0000', '0003', '0004', '0010');
CREATE TABLE a5_plt2_p2 PARTITION OF a5_plt2 FOR VALUES IN ('0001', '0005', '0002', '0009');
CREATE TABLE a5_plt2_p3 PARTITION OF a5_plt2 FOR VALUES IN ('0006', '0007', '0008', '0011');
INSERT INTO a5_plt2 SELECT i, i, to_char(i/50, 'FM0000') FROM generate_series(0, 599, 3) i;
ANALYZE a5_plt2;
EXPLAIN (COSTS OFF)
SELECT avg(t1.a), avg(t2.b), t1.c, t2.c FROM a5_plt1 t1 RIGHT JOIN a5_plt2 t2 ON t1.c = t2.c
  WHERE t1.a % 25 = 0 GROUP BY t1.c, t2.c ORDER BY t1.c, t2.c;
SELECT avg(t1.a), avg(t2.b), t1.c, t2.c FROM a5_plt1 t1 RIGHT JOIN a5_plt2 t2 ON t1.c = t2.c
  WHERE t1.a % 25 = 0 GROUP BY t1.c, t2.c ORDER BY t1.c, t2.c;
RESET enable_partitionwise_join;
RESET max_parallel_workers_per_gather;
CREATE TABLE a5_x1 (a int, b int, c int) PARTITION BY RANGE (a);
CREATE TABLE a5_x1_1 PARTITION OF a5_x1 FOR VALUES FROM (1) TO (11);
CREATE TABLE a5_x1_2 PARTITION OF a5_x1 FOR VALUES FROM (11) TO (21);
CREATE TABLE a5_x2 (a int, b int, c int) PARTITION BY RANGE (a);
CREATE TABLE a5_x2_1 PARTITION OF a5_x2 FOR VALUES FROM (1) TO (11);
CREATE TABLE a5_x2_2 PARTITION OF a5_x2 FOR VALUES FROM (11) TO (21);
CREATE INDEX ON a5_x2 (b);
CREATE INDEX ON a5_x2 (c);
INSERT INTO a5_x1 SELECT 1 + mod(i, 20), i, i FROM generate_series(1, 1000) i;
INSERT INTO a5_x2 SELECT 1 + mod(i, 20), i, i FROM generate_series(1, 1000) i;
ANALYZE a5_x1;
ANALYZE a5_x2;
SET enable_partitionwise_join = on;
EXPLAIN (COSTS OFF)
SELECT * FROM a5_x1 WHERE NOT EXISTS (SELECT 1 FROM a5_x2
  WHERE a5_x2.a = a5_x1.a AND (a5_x2.b = a5_x1.b + 1 OR a5_x2.c = 99)) AND a < 20 AND c = 91;
SELECT * FROM a5_x1 WHERE NOT EXISTS (SELECT 1 FROM a5_x2
  WHERE a5_x2.a = a5_x1.a AND (a5_x2.b = a5_x1.b + 1 OR a5_x2.c = 99)) AND a < 20 AND c = 91;
RESET enable_partitionwise_join;
DROP TABLE a5_fract, a5_plt1, a5_plt2, a5_x1, a5_x2;
