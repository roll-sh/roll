-- Partition pruning details: a COLLATE naming the column's own collation is no relabelling,
-- IN over array values is ORed comparisons, a timestamp key compared with timestamptz
-- constants (stable) prunes only as the executor starts, a Merge Append shows the members
-- it removed then, a UNION ALL's members are pruned (an InitPlan's or a parameter's value
-- in the quals pushed into its arms), a varchar IN with a non-constant value is the array
-- coerced (no pruning).
SET timezone = 'America/Los_Angeles';
SET plan_cache_mode = force_generic_plan;
CREATE TABLE a5_coll (a text COLLATE "C") PARTITION BY LIST (a);
CREATE TABLE a5_coll_a PARTITION OF a5_coll FOR VALUES IN ('a');
CREATE TABLE a5_coll_b PARTITION OF a5_coll FOR VALUES IN ('b');
CREATE TABLE a5_coll_def PARTITION OF a5_coll DEFAULT;
EXPLAIN (COSTS OFF) SELECT * FROM a5_coll WHERE a COLLATE "C" = 'a' COLLATE "C";
EXPLAIN (COSTS OFF) SELECT * FROM a5_coll WHERE a COLLATE "POSIX" = 'a' COLLATE "POSIX";
CREATE TABLE a5_arr (a int[]) PARTITION BY LIST (a);
CREATE TABLE a5_arr1 PARTITION OF a5_arr FOR VALUES IN ('{1}');
CREATE TABLE a5_arr2 PARTITION OF a5_arr FOR VALUES IN ('{2, 3}', '{4, 5}');
INSERT INTO a5_arr VALUES ('{1}'), ('{2,3}'), ('{4,5}');
EXPLAIN (COSTS OFF) SELECT * FROM a5_arr WHERE a IN ('{4, 5}', '{1}');
EXPLAIN (COSTS OFF) SELECT * FROM a5_arr WHERE a NOT IN ('{4, 5}', '{1}');
SELECT * FROM a5_arr WHERE a IN ('{4, 5}', '{1}') ORDER BY a;
SELECT * FROM a5_arr WHERE a NOT IN ('{4, 5}', '{1}');
CREATE TABLE a5_ts (a timestamp) PARTITION BY RANGE (a);
CREATE TABLE a5_ts1 PARTITION OF a5_ts FOR VALUES FROM ('2000-01-01') TO ('2000-02-01');
CREATE TABLE a5_ts2 PARTITION OF a5_ts FOR VALUES FROM ('2000-02-01') TO ('2000-03-01');
CREATE TABLE a5_ts3 PARTITION OF a5_ts FOR VALUES FROM ('3000-02-01') TO ('3000-03-01');
INSERT INTO a5_ts VALUES ('2000-01-15'), ('2000-02-15'), ('3000-02-15');
EXPLAIN (COSTS OFF) SELECT * FROM a5_ts WHERE a < '2000-02-01'::timestamptz;
EXPLAIN (COSTS OFF) SELECT * FROM a5_ts WHERE a = ANY (ARRAY['2010-02-01', '2020-01-01']::timestamptz[]);
EXPLAIN (COSTS OFF) SELECT * FROM a5_ts WHERE a = ANY (ARRAY['2000-02-01', '2010-01-01']::timestamptz[]);
SELECT * FROM a5_ts WHERE a < '2000-02-01'::timestamptz;
SELECT * FROM a5_ts WHERE a >= '2000-02-01 08:00:00+00'::timestamptz ORDER BY a;
SELECT * FROM a5_ts WHERE a = ANY (ARRAY['2000-02-15', '2010-01-01']::timestamptz[]);
CREATE TABLE a5_tz (a timestamptz) PARTITION BY RANGE (a);
CREATE TABLE a5_tz1 PARTITION OF a5_tz FOR VALUES FROM ('2000-01-01 00:00+00') TO ('2000-02-01 00:00+00');
CREATE TABLE a5_tz2 PARTITION OF a5_tz FOR VALUES FROM ('2000-02-01 00:00+00') TO ('2000-03-01 00:00+00');
INSERT INTO a5_tz VALUES ('2000-01-31 23:00+00'), ('2000-02-15 00:00+00');
EXPLAIN (COSTS OFF) SELECT * FROM a5_tz WHERE a > '2000-02-01'::timestamp;
SELECT * FROM a5_tz WHERE a > '2000-01-31 14:00'::timestamp ORDER BY a;
SET enable_seqscan = off;
SET enable_sort = off;
CREATE TABLE a5_ma (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE a5_ma_p1 PARTITION OF a5_ma FOR VALUES FROM (0) TO (10);
CREATE TABLE a5_ma_p2 PARTITION OF a5_ma FOR VALUES FROM (10) TO (20);
CREATE TABLE a5_ma_p3 PARTITION OF a5_ma FOR VALUES FROM (20) TO (30);
INSERT INTO a5_ma SELECT x, x FROM generate_series(0, 29) t(x);
CREATE INDEX ON a5_ma (b);
ANALYZE a5_ma;
PREPARE a5_mq1 (int) AS SELECT a FROM a5_ma WHERE a >= $1 AND a % 10 = 5 ORDER BY b;
EXPLAIN (COSTS OFF) EXECUTE a5_mq1(15);
EXECUTE a5_mq1(15);
EXPLAIN (COSTS OFF) EXECUTE a5_mq1(35);
EXECUTE a5_mq1(35);
DEALLOCATE a5_mq1;
PREPARE a5_mq2 (int) AS SELECT * FROM a5_ma WHERE a >= $1 ORDER BY b LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE a5_mq2 (35);
DEALLOCATE a5_mq2;
RESET enable_seqscan;
RESET enable_sort;
CREATE TABLE a5_ab (a int NOT NULL, b int NOT NULL) PARTITION BY LIST (a);
CREATE TABLE a5_ab_a1 PARTITION OF a5_ab FOR VALUES IN (1) PARTITION BY LIST (b);
CREATE TABLE a5_ab_a1_b1 PARTITION OF a5_ab_a1 FOR VALUES IN (1);
CREATE TABLE a5_ab_a1_b2 PARTITION OF a5_ab_a1 FOR VALUES IN (2);
CREATE TABLE a5_ab_a2 PARTITION OF a5_ab FOR VALUES IN (2);
INSERT INTO a5_ab VALUES (1, 1), (1, 2), (2, 1);
CREATE TABLE a5_xy (x int, y int);
INSERT INTO a5_xy VALUES (100, -10);
PREPARE a5_uq (int) AS
SELECT * FROM (SELECT tableoid::regclass, a, b FROM a5_ab
               UNION ALL SELECT tableoid::regclass, x, y FROM a5_xy
               UNION ALL SELECT tableoid::regclass, a, b FROM a5_ab) s
WHERE a = $1 AND b = (SELECT 1);
EXPLAIN (COSTS OFF) EXECUTE a5_uq(1);
EXECUTE a5_uq(1);
DEALLOCATE a5_uq;
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT * FROM a5_ab UNION ALL SELECT 1, 1 UNION ALL SELECT * FROM a5_xy) s(a, b)
WHERE s.a = 1 AND s.b = (SELECT 1);
SELECT * FROM (SELECT * FROM a5_ab UNION ALL SELECT 1, 1 UNION ALL SELECT * FROM a5_xy) s(a, b)
WHERE s.a = 1 AND s.b = (SELECT 1);
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM a5_ab UNION ALL SELECT * FROM a5_ab WHERE a = 1) s;
EXPLAIN (COSTS OFF) SELECT * FROM (SELECT * FROM a5_ab WHERE a = 2 UNION ALL SELECT * FROM a5_ab WHERE a = 1) s;
CREATE TABLE a5_vc (a varchar) PARTITION BY LIST (a);
CREATE TABLE a5_vc_ab PARTITION OF a5_vc FOR VALUES IN ('ab');
CREATE TABLE a5_vc_bc PARTITION OF a5_vc FOR VALUES IN ('bc');
INSERT INTO a5_vc VALUES ('ab'), ('bc');
EXPLAIN (COSTS OFF) SELECT * FROM a5_vc WHERE a IN ('ab', to_char(125, '999'));
SELECT * FROM a5_vc WHERE a IN ('ab', to_char(125, '999'));
SELECT * FROM a5_vc WHERE a NOT IN ('ab', to_char(125, '999'));
CREATE TABLE a5_phv (a int, b text) PARTITION BY LIST (a);
CREATE TABLE a5_phv_1 PARTITION OF a5_phv FOR VALUES IN (1);
CREATE TABLE a5_phv_2 PARTITION OF a5_phv FOR VALUES IN (2);
CREATE TABLE a5_phv_null PARTITION OF a5_phv FOR VALUES IN (NULL);
INSERT INTO a5_phv VALUES (1, 'one'), (2, 'two'), (NULL, 'null');
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT a, b FROM a5_phv) t WHERE a = 1 GROUP BY GROUPING SETS (a, b);
EXPLAIN (COSTS OFF)
SELECT * FROM (SELECT a::oid AS x, b FROM a5_phv) t WHERE x::int = 1 GROUP BY GROUPING SETS (x, b);
SELECT * FROM (SELECT a::oid AS x, b FROM a5_phv) t WHERE x::int = 1 GROUP BY GROUPING SETS (x, b)
ORDER BY 1, 2;
DROP TABLE a5_coll, a5_arr, a5_ts, a5_tz, a5_ma, a5_ab, a5_xy, a5_vc, a5_phv;
