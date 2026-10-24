-- Round 16, agent 40: partitionwise paths of join relations inside groups with outer joins
-- (try_partitionwise_join for every join relation), child joins made only of partitioned
-- parent relations (a tie between join orders kept as Postgres's search makes them),
-- class clauses first made by index paths in range table order (a rule action's OLD).
SET enable_partitionwise_join to true;
CREATE TABLE a40_p1 (a int, b int, c varchar) PARTITION BY RANGE(a);
CREATE TABLE a40_p1_1 PARTITION OF a40_p1 FOR VALUES FROM (0) TO (250);
CREATE TABLE a40_p1_3 PARTITION OF a40_p1 FOR VALUES FROM (500) TO (600);
CREATE TABLE a40_p1_2 PARTITION OF a40_p1 FOR VALUES FROM (250) TO (500);
INSERT INTO a40_p1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 2 = 0;
CREATE INDEX a40_i1_1_a on a40_p1_1(a);
CREATE INDEX a40_i1_2_a on a40_p1_2(a);
CREATE INDEX a40_i1_3_a on a40_p1_3(a);
ANALYZE a40_p1;
CREATE TABLE a40_p2 (a int, b int, c varchar) PARTITION BY RANGE(b);
CREATE TABLE a40_p2_1 PARTITION OF a40_p2 FOR VALUES FROM (0) TO (250);
CREATE TABLE a40_p2_2 PARTITION OF a40_p2 FOR VALUES FROM (250) TO (500);
CREATE TABLE a40_p2_3 PARTITION OF a40_p2 FOR VALUES FROM (500) TO (600);
INSERT INTO a40_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(0, 599) i WHERE i % 3 = 0;
CREATE INDEX a40_i2_1_b on a40_p2_1(b);
CREATE INDEX a40_i2_2_b on a40_p2_2(b);
CREATE INDEX a40_i2_3_b on a40_p2_3(b);
ANALYZE a40_p2;
CREATE TABLE a40_plain (a int, b int);
INSERT INTO a40_plain SELECT i, i % 7 FROM generate_series(0, 599, 5) i;
ANALYZE a40_plain;
CREATE TABLE a40_p3 (a int, b int) PARTITION BY LIST (b);
CREATE TABLE a40_p3_1 PARTITION OF a40_p3 FOR VALUES IN (0, 1, 2);
CREATE TABLE a40_p3_2 PARTITION OF a40_p3 FOR VALUES IN (3, 4, 5, 6);
INSERT INTO a40_p3 SELECT i, i % 7 FROM generate_series(0, 599, 4) i;
ANALYZE a40_p3;
-- the regress unit
EXPLAIN (COSTS OFF)
SELECT t1.a, t1.c, t2.b, t2.c FROM (SELECT * FROM a40_p1 WHERE a = 1 AND a = 2) t1 RIGHT JOIN a40_p2 t2 ON t1.a = t2.b, a40_p1 t3 WHERE t2.b = t3.a;
SELECT t1.a, t1.c, t2.b, t2.c FROM (SELECT * FROM a40_p1 WHERE a = 1 AND a = 2) t1 RIGHT JOIN a40_p2 t2 ON t1.a = t2.b, a40_p1 t3 WHERE t2.b = t3.a ORDER BY t2.b, t2.c;
-- an inner pair below a left join to a a40_plain table
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, p.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN a40_plain p ON p.a = t1.a WHERE t1.b < 20;
SELECT t1.a, t2.b, p.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN a40_plain p ON p.a = t1.a WHERE t1.b < 20 ORDER BY 1, 2, 3;
-- a left join from a a40_plain table to an inner pair (the pair is the nullable side)
EXPLAIN (COSTS OFF)
SELECT p.a, t1.a, t2.b FROM a40_plain p LEFT JOIN (a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b) ON p.a = t1.a;
SELECT p.a, t1.a, t2.b FROM a40_plain p LEFT JOIN (a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b) ON p.a = t1.a ORDER BY 1, 2, 3;
-- an inner pair, the outer join's other side partitioned otherwise
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, t3.a FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN a40_p3 t3 ON t3.a = t2.b AND t3.b > 2;
SELECT t1.a, t2.b, t3.a FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN a40_p3 t3 ON t3.a = t2.b AND t3.b > 2 ORDER BY 1, 2, 3;
-- a full join over an inner pair
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, p.a FROM (a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b) FULL JOIN a40_plain p ON p.a = t1.a;
SELECT t1.a, t2.b, p.a FROM (a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b) FULL JOIN a40_plain p ON p.a = t1.a ORDER BY 1, 2, 3;
-- anti / semi joins with an inner pair
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b WHERE NOT EXISTS (SELECT 1 FROM a40_plain p WHERE p.a = t1.a);
SELECT t1.a, t2.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b WHERE NOT EXISTS (SELECT 1 FROM a40_plain p WHERE p.a = t1.a) ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b WHERE EXISTS (SELECT 1 FROM a40_plain p WHERE p.a = t1.a AND p.b > 1);
SELECT t1.a, t2.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b WHERE EXISTS (SELECT 1 FROM a40_plain p WHERE p.a = t1.a AND p.b > 1) ORDER BY 1, 2;
-- four items: two inner pairs, a left join between
EXPLAIN (COSTS OFF)
SELECT t1.a, t2.b, t3.a, t4.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN (a40_p1 t3 JOIN a40_p2 t4 ON t3.a = t4.b) ON t3.a = t1.a AND t3.b < 3 WHERE t1.b = 0;
SELECT t1.a, t2.b, t3.a, t4.b FROM a40_p1 t1 JOIN a40_p2 t2 ON t1.a = t2.b LEFT JOIN (a40_p1 t3 JOIN a40_p2 t4 ON t3.a = t4.b) ON t3.a = t1.a AND t3.b < 3 WHERE t1.b = 0 ORDER BY 1, 2, 3, 4;
CREATE TABLE a40_adv1 (a int, b int, c varchar) PARTITION BY RANGE (a);
CREATE TABLE a40_adv1_p1 PARTITION OF a40_adv1 FOR VALUES FROM (100) TO (200);
CREATE TABLE a40_adv1_p2 PARTITION OF a40_adv1 FOR VALUES FROM (200) TO (300);
CREATE TABLE a40_adv1_p3 PARTITION OF a40_adv1 FOR VALUES FROM (300) TO (400);
CREATE INDEX a40_adv1_a_idx ON a40_adv1 (a);
INSERT INTO a40_adv1 SELECT i, i % 25, to_char(i, 'FM0000') FROM generate_series(100, 399) i;
ANALYZE a40_adv1;

CREATE TABLE a40_adv2 (a int, b int, c varchar) PARTITION BY RANGE (b);
CREATE TABLE a40_adv2_p1 PARTITION OF a40_adv2 FOR VALUES FROM (100) TO (150);
CREATE TABLE a40_adv2_p2 PARTITION OF a40_adv2 FOR VALUES FROM (200) TO (300);
CREATE TABLE a40_adv2_p3 PARTITION OF a40_adv2 FOR VALUES FROM (350) TO (500);
CREATE INDEX a40_adv2_b_idx ON a40_adv2 (b);
INSERT INTO a40_adv2_p1 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(100, 149) i;
INSERT INTO a40_adv2_p2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(200, 299) i;
INSERT INTO a40_adv2_p3 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(350, 499) i;
ANALYZE a40_adv2;
CREATE TABLE a40_adv2_extra PARTITION OF a40_adv2 FOR VALUES FROM (500) TO (MAXVALUE);
INSERT INTO a40_adv2 SELECT i % 25, i, to_char(i, 'FM0000') FROM generate_series(500, 599) i;
ANALYZE a40_adv2;
EXPLAIN (COSTS OFF) SELECT t1.b, t1.c, t2.a, t2.c, t3.a, t3.c FROM a40_adv2 t1 LEFT JOIN a40_adv1 t2 ON (t1.b = t2.a) INNER JOIN a40_adv1 t3 ON (t1.b = t3.a) WHERE t1.a = 0 ORDER BY t1.b, t2.a, t3.a;
SELECT t1.b, t1.c, t2.a, t2.c, t3.a, t3.c FROM a40_adv2 t1 LEFT JOIN a40_adv1 t2 ON (t1.b = t2.a) INNER JOIN a40_adv1 t3 ON (t1.b = t3.a) WHERE t1.a = 0 ORDER BY t1.b, t2.a, t3.a;
RESET enable_partitionwise_join;
set enable_hashjoin = off;
set enable_mergejoin = off;
create table a40_sj (a int unique, b int, c int unique); create table a40_sj2 (a int unique, b int, c int unique);
insert into a40_sj values (1, null, 2), (null, 2, null), (2, 1, 1),(3,1,3); insert into a40_sj2 select * from a40_sj; analyze a40_sj; analyze a40_sj2;
CREATE OR REPLACE RULE a40_sj_del_rule AS ON DELETE TO a40_sj
  DO INSTEAD
    UPDATE a40_sj SET a = 1 WHERE a = old.a;
EXPLAIN (COSTS OFF) DELETE FROM a40_sj;
DROP RULE a40_sj_del_rule ON a40_sj;
CREATE OR REPLACE RULE a40_sj_del_rule AS ON DELETE TO a40_sj
  DO INSTEAD
    UPDATE a40_sj SET a = 1 WHERE old.a = a;
EXPLAIN (COSTS OFF) DELETE FROM a40_sj;
DROP RULE a40_sj_del_rule ON a40_sj;
CREATE OR REPLACE RULE a40_sj_del_rule AS ON DELETE TO a40_sj
  DO INSTEAD
    UPDATE a40_sj2 SET a = 1 WHERE a = old.a;
EXPLAIN (COSTS OFF) DELETE FROM a40_sj;
DROP RULE a40_sj_del_rule ON a40_sj;
CREATE OR REPLACE RULE a40_sj_del_rule AS ON DELETE TO a40_sj
  DO INSTEAD
    UPDATE a40_sj2 SET a = 1 WHERE b = old.b;
EXPLAIN (COSTS OFF) DELETE FROM a40_sj;
DROP RULE a40_sj_del_rule ON a40_sj;
EXPLAIN (COSTS OFF) UPDATE a40_sj2 SET a = 1 FROM a40_sj WHERE a40_sj2.a = a40_sj.a;
EXPLAIN (COSTS OFF) UPDATE a40_sj SET a = 1 FROM a40_sj s2 WHERE a40_sj.a = s2.a;
EXPLAIN (COSTS OFF) UPDATE a40_sj SET a = 1 FROM a40_sj s2 WHERE a40_sj.b = s2.b;
reset enable_hashjoin;
reset enable_mergejoin;
