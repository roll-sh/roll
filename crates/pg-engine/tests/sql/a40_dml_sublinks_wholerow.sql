-- Round 16, agent 40: an UPDATE / DELETE of an inheritance or partition tree pulls its
-- WHERE's sublinks up (a security barrier view's quals not), EXISTS over a join with a
-- LATERAL item or a FROM subquery, the pulled-up subquery's whole-row mark; whole-row Vars
-- returned as one value from the scans up to the node reading them.
CREATE TABLE a40_pt (a int, b int, c text) PARTITION BY RANGE (a);
CREATE TABLE a40_pt1 PARTITION OF a40_pt FOR VALUES FROM (0) TO (50);
CREATE TABLE a40_pt2 PARTITION OF a40_pt FOR VALUES FROM (50) TO (100) PARTITION BY LIST (c);
CREATE TABLE a40_pt2a PARTITION OF a40_pt2 FOR VALUES IN ('x', 'y');
CREATE TABLE a40_pt2b PARTITION OF a40_pt2 DEFAULT;
INSERT INTO a40_pt SELECT i, i % 7, CASE WHEN i % 5 = 0 THEN NULL WHEN i % 2 = 0 THEN 'x' ELSE 'z' END FROM generate_series(0, 99) i;
CREATE TABLE a40_ih (a int, b int);
CREATE TABLE a40_ih_c1 (x int) INHERITS (a40_ih);
CREATE TABLE a40_ih_c2 () INHERITS (a40_ih);
INSERT INTO a40_ih SELECT i, i % 4 FROM generate_series(0, 20) i;
INSERT INTO a40_ih_c1 SELECT i, i % 3, i FROM generate_series(21, 40) i;
INSERT INTO a40_ih_c2 SELECT i, i % 5 FROM generate_series(41, 60) i;
CREATE TABLE a40_k (v int, w int);
INSERT INTO a40_k SELECT i * 3, i FROM generate_series(0, 30) i;
ANALYZE a40_pt; ANALYZE a40_ih; ANALYZE a40_ih_c1; ANALYZE a40_ih_c2; ANALYZE a40_k;
BEGIN;
EXPLAIN (COSTS OFF) DELETE FROM a40_pt WHERE EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_pt.a);
DELETE FROM a40_pt WHERE EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_pt.a) RETURNING tableoid::regclass, *;
SELECT tableoid::regclass, * FROM a40_pt ORDER BY a;
ROLLBACK;
BEGIN;
EXPLAIN (COSTS OFF) UPDATE a40_pt SET b = b + 100 WHERE NOT EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_pt.a AND k.w > 3) AND c IS NOT NULL;
UPDATE a40_pt SET b = b + 100 WHERE NOT EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_pt.a AND k.w > 3) AND c IS NOT NULL;
SELECT tableoid::regclass, * FROM a40_pt ORDER BY a;
ROLLBACK;
BEGIN;
EXPLAIN (COSTS OFF) UPDATE a40_pt SET a = a + 50 WHERE a IN (SELECT v FROM a40_k k WHERE w < 10);
UPDATE a40_pt SET a = a + 50 WHERE a IN (SELECT v FROM a40_k k WHERE w < 10) RETURNING tableoid::regclass, *;
SELECT tableoid::regclass, * FROM a40_pt ORDER BY a, b;
ROLLBACK;
BEGIN;
EXPLAIN (COSTS OFF) DELETE FROM a40_ih WHERE EXISTS (SELECT 1 FROM a40_k k, LATERAL (SELECT k.w FROM a40_k k2 LIMIT 2) ss WHERE k.v = a40_ih.a);
DELETE FROM a40_ih WHERE EXISTS (SELECT 1 FROM a40_k k, LATERAL (SELECT k.w FROM a40_k k2 LIMIT 2) ss WHERE k.v = a40_ih.a) RETURNING tableoid::regclass, *;
SELECT tableoid::regclass, * FROM a40_ih ORDER BY a;
ROLLBACK;
BEGIN;
EXPLAIN (COSTS OFF) UPDATE a40_ih SET b = -b WHERE EXISTS (SELECT 1 FROM (SELECT v FROM a40_k k ORDER BY v LIMIT 8) s WHERE s.v = a40_ih.a);
UPDATE a40_ih SET b = -b WHERE EXISTS (SELECT 1 FROM (SELECT v FROM a40_k k ORDER BY v LIMIT 8) s WHERE s.v = a40_ih.a);
SELECT tableoid::regclass, * FROM a40_ih ORDER BY a;
ROLLBACK;
BEGIN;
DELETE FROM a40_pt WHERE EXISTS (SELECT 1 FROM a40_k k, LATERAL (SELECT k.w FROM a40_k k2 LIMIT 2) ss WHERE a40_pt.c IS NULL);
SELECT tableoid::regclass, * FROM a40_pt ORDER BY a;
ROLLBACK;
BEGIN;
DELETE FROM a40_ih_c1 WHERE x IN (SELECT v FROM a40_k k) RETURNING *;
SELECT * FROM a40_ih ORDER BY a;
ROLLBACK;
CREATE TABLE a40_int4(f1 int4);
INSERT INTO a40_int4(f1) VALUES (0), (123456), (-123456), (2147483647), (-2147483647);
CREATE TABLE a40_int2(f1 int2);
INSERT INTO a40_int2(f1) VALUES (0), (1234), (-1234), (32767), (-32767);
ANALYZE a40_int4; ANALYZE a40_int2;
explain (verbose, costs off)
select * from a40_int4 i left join
  lateral (select coalesce(i) from a40_int2 j where i.f1 = j.f1) k on true;
select * from a40_int4 i left join
  lateral (select coalesce(i) from a40_int2 j where i.f1 = j.f1) k on true;
explain (verbose, costs off)
select i, j.f1 from a40_int4 i join a40_int2 j on i.f1 = j.f1;
explain (verbose, costs off)
select i from a40_int4 i join a40_int2 j on i.f1 = j.f1;
explain (verbose, costs off)
select * from a40_int4 i, lateral (select i, j.f1 from a40_int2 j where j.f1 > i.f1 offset 0) k;
explain (verbose, costs off)
select * from a40_int4 i left join lateral (select i from a40_int2 j where i.f1 = j.f1) k on true;
explain (verbose, costs off)
select j.f1 from a40_int4 i join a40_int2 j on i.f1 = j.f1 where i is not null;
explain (verbose, costs off)
select row_to_json(i), j from a40_int4 i join a40_int2 j on i.f1 = j.f1;
CREATE TABLE a40_wt (a int, b text);
INSERT INTO a40_wt SELECT i, 'v' || i FROM generate_series(1, 50) i;
explain (verbose, costs off) select i from a40_int4 i, a40_int2 j;
explain (verbose, costs off) select j, i.f1 from a40_int4 i left join a40_int2 j on i.f1 = j.f1;
explain (verbose, costs off) select w, i.f1 from a40_wt w join a40_int4 i on w.a = i.f1 join a40_int2 j on j.f1 = i.f1;
explain (verbose, costs off) select w.a from a40_wt w join a40_int4 i on w.a = i.f1 where w is not null and w::text <> i::text;
set enable_hashjoin = off;
explain (verbose, costs off) select w, i from a40_wt w join a40_int4 i on w.a = i.f1;
set enable_mergejoin = off;
explain (verbose, costs off) select w, i from a40_wt w join a40_int4 i on w.a = i.f1;
select * from a40_wt w, lateral (select w, i.f1 from a40_int4 i where i.f1 = w.a) ss;

reset enable_hashjoin;
reset enable_mergejoin;
-- A security barrier view's EXISTS stays a SubPlan of the base relation's quals; a plain
-- view's is pulled up.
CREATE VIEW a40_sbv WITH (security_barrier) AS SELECT * FROM a40_ih WHERE EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_ih.a);
CREATE VIEW a40_pv AS SELECT * FROM a40_ih WHERE EXISTS (SELECT 1 FROM a40_k k WHERE k.v = a40_ih.a);
BEGIN;
EXPLAIN (COSTS OFF) UPDATE a40_sbv SET b = 1000 WHERE a < 30;
UPDATE a40_sbv SET b = 1000 WHERE a < 30;
EXPLAIN (COSTS OFF) UPDATE a40_pv SET b = 2000 WHERE a > 30;
UPDATE a40_pv SET b = 2000 WHERE a > 30;
SELECT tableoid::regclass, * FROM a40_ih ORDER BY a;
ROLLBACK;
