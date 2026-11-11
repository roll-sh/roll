-- Found by the differential tester (tests/difftest.rs): self-join elimination took an
-- outer join's ON clauses as equalities between the scans it compares, so two scans of a
-- table on the outer join's preserved side, both equated to the nullable side's column,
-- were merged into one (rows lost).
CREATE TABLE t0 (a int4, d timestamptz);
CREATE TABLE t2 (id int4 PRIMARY KEY, c text);
INSERT INTO t2 VALUES (1, NULL), (55, 'A');
INSERT INTO t0 VALUES (1, NULL);
SELECT t7.id, t8.id FROM (t2 t7 CROSS JOIN t2 t8) LEFT JOIN t0 t9 ON t8.id = t9.a AND t7.id = t9.a ORDER BY 1, 2;
SELECT count(*) FROM (t2 t7 CROSS JOIN t2 t8 LEFT JOIN t0 t9 ON t8.id = t9.a AND t7.id = t9.a);
SELECT t7.id, t8.id, t9.a FROM t0 t9 RIGHT JOIN (t2 t7 CROSS JOIN t2 t8) ON t8.id = t9.a AND t7.id = t9.a ORDER BY 1, 2;
SELECT t7.id, t8.id FROM t2 t7 JOIN t2 t8 ON t7.id = t8.id LEFT JOIN t0 t9 ON t8.id = t9.a ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT t7.id, t8.id FROM t2 t7 JOIN t2 t8 ON t7.id = t8.id LEFT JOIN t0 t9 ON t8.id = t9.a;
SELECT t7.id, t8.id FROM t0 t9 LEFT JOIN (t2 t7 JOIN t2 t8 ON t7.id = t8.id) ON t8.id = t9.a ORDER BY 1;
SELECT t7.id, t8.id FROM t0 t9 LEFT JOIN (t2 t7 CROSS JOIN t2 t8) ON t8.id = t9.a AND t7.id = t9.a ORDER BY 1, 2;
