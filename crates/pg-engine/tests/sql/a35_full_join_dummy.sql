-- Found by the differential tester (tests/difftest.rs): a FULL JOIN whose own join
-- relation has a constant-false restriction (WHERE false / NULL over it) is dummy before
-- PostgreSQL looks for merge or hash join clauses: no "FULL JOIN is only supported" error.
CREATE TABLE t0 (id int4, a int4, b int4);
CREATE TABLE t2 (id int4 PRIMARY KEY, d jsonb);
INSERT INTO t0 VALUES (1, 2, 3);
INSERT INTO t2 VALUES (1, '{"a": 1}');
SELECT count(*) FROM (t0 t1 FULL JOIN t2 ON (t2.d ?| ARRAY['a', 'x'])) WHERE NOT NULL;
SELECT count(*), 5 FROM (t0 t1 FULL JOIN t2 ON (t2.d ?| ARRAY['a', 'x'])) WHERE NULL;
SELECT * FROM t0 FULL JOIN t2 ON t0.a < t2.id WHERE false;
SELECT * FROM t0 FULL JOIN t2 ON t0.a < t2.id;
SELECT * FROM t0 FULL JOIN t2 ON t0.a < t2.id WHERE t0.id IS NULL AND false;
INSERT INTO t0 (id, a, b) SELECT 1, 5, count(*) FROM (t0 t1 FULL JOIN t2 ON (t2.d ?| ARRAY['a', 'x'])) WHERE NOT NULL;
SELECT * FROM t0 ORDER BY id, a;
