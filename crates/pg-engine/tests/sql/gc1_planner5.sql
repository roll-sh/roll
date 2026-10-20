-- Planner and EXPLAIN shapes (group C1), part 5: MIN / MAX as InitPlans over an index,
-- RETURNING OLD / NEW under VERBOSE, NULLS LAST indexes.
CREATE TABLE gc5_t (a int, b int, c text);
INSERT INTO gc5_t SELECT i, i % 100, 'x' || i FROM generate_series(1, 5000) i;
CREATE INDEX gc5_t_a ON gc5_t (a);
CREATE INDEX gc5_t_ba ON gc5_t (b, a);
ANALYZE gc5_t;
EXPLAIN (COSTS OFF) SELECT min(a) FROM gc5_t;
SELECT min(a) FROM gc5_t;
EXPLAIN (COSTS OFF) SELECT max(a) FROM gc5_t WHERE a < 42;
SELECT max(a) FROM gc5_t WHERE a < 42;
EXPLAIN (COSTS OFF) SELECT max(a) FROM gc5_t WHERE b = 33;
SELECT max(a) FROM gc5_t WHERE b = 33;
EXPLAIN (COSTS OFF) SELECT min(a), max(a) FROM gc5_t;
SELECT min(a), max(a) FROM gc5_t;
EXPLAIN (COSTS OFF) SELECT max(a) FROM gc5_t ORDER BY max(a) + 1;
EXPLAIN (COSTS OFF) SELECT DISTINCT max(b) FROM gc5_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT max(a) FROM gc5_t;
EXPLAIN (COSTS OFF) SELECT max(c) FROM gc5_t;
-- RETURNING OLD / NEW.
CREATE TABLE gc5_r (f1 int, f2 text, f3 int DEFAULT 42, f4 bigint DEFAULT 99);
INSERT INTO gc5_r VALUES (1, 'one'), (5, 'five');
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO gc5_r VALUES (4) RETURNING old.f1, new.*, new.f3, *;
INSERT INTO gc5_r VALUES (4) RETURNING old.f1, new.*, new.f3, *;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE gc5_r SET f4 = 100 WHERE f1 = 5 RETURNING old.*, old, new.f4, old.f4::text || '->' || new.f4::text AS change;
UPDATE gc5_r SET f4 = 100 WHERE f1 = 5 RETURNING old.*, old, new.f4, old.f4::text || '->' || new.f4::text AS change;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM gc5_r WHERE f1 = 1 RETURNING old.f2, new.f2;
DELETE FROM gc5_r WHERE f1 = 1 RETURNING old.f2, new.f2;
