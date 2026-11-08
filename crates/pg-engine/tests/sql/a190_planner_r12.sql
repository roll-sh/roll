-- Planner round 12 (agent 190).
-- A grouping ordered by its keys in another direction: the index read backward under the
-- GroupAggregate, under a LIMIT for the fraction of the sorted groups it reads.
CREATE TABLE a190_g (id int PRIMARY KEY, x int);
INSERT INTO a190_g SELECT i, i % 7 FROM generate_series(1, 10000) i;
ANALYZE a190_g;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
EXPLAIN SELECT id, sum(x) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
EXPLAIN SELECT id FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
SELECT id FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id LIMIT 5;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5 OFFSET 10;
SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5 OFFSET 10;
EXPLAIN SELECT id, count(*) FROM a190_g WHERE id > 9000 GROUP BY id ORDER BY id DESC;
EXPLAIN SELECT id, count(*) FROM a190_g WHERE id < 50 GROUP BY id ORDER BY id DESC;
EXPLAIN SELECT id, count(*) FROM a190_g WHERE id < 50 GROUP BY id ORDER BY id;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC;
EXPLAIN SELECT x, count(*) FROM a190_g GROUP BY x ORDER BY x DESC LIMIT 2;
SET enable_seqscan = off;
SET enable_hashagg = off;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC;
EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC LIMIT 5;
RESET enable_seqscan;
RESET enable_hashagg;
-- A DELETE ... USING join returns the target's ctid and the other relation's row mark.
CREATE TABLE a190_b (id int PRIMARY KEY, v int);
CREATE TABLE a190_s (id int PRIMARY KEY, w int);
INSERT INTO a190_b SELECT g, g FROM generate_series(1, 1000) g;
INSERT INTO a190_s SELECT g, g FROM generate_series(1, 1000) g;
ANALYZE a190_b;
ANALYZE a190_s;
EXPLAIN DELETE FROM a190_b r1 USING a190_s r2 WHERE r1.id = r2.id AND r2.w < 3;
EXPLAIN VERBOSE DELETE FROM a190_b r1 USING a190_s r2 WHERE r1.id = r2.id AND r2.w < 3;
EXPLAIN DELETE FROM a190_b r1 USING a190_s r2 WHERE r1.v = r2.w AND r1.id < 3;
EXPLAIN UPDATE a190_b r1 SET v = 0 FROM a190_s r2 WHERE r1.id = r2.id AND r2.w < 3;
BEGIN;
DELETE FROM a190_b r1 USING a190_s r2 WHERE r1.id = r2.id AND r2.w < 3 RETURNING r1.id, r2.w;
ROLLBACK;
-- A pulled-up sublink's variable-free qual (an InitPlan's value) is a join filter of the
-- semi or anti join, not a gating Result over its right side.
CREATE TABLE a190_na (id int PRIMARY KEY, x int);
CREATE TABLE a190_nb (id int PRIMARY KEY, y int);
INSERT INTO a190_na SELECT g, g % 100 FROM generate_series(1, 1000) g;
INSERT INTO a190_nb SELECT g, g % 50 FROM generate_series(1, 1000) g;
ANALYZE a190_na;
ANALYZE a190_nb;
EXPLAIN (COSTS OFF) SELECT * FROM a190_na WHERE NOT EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));
SELECT count(*) FROM a190_na WHERE NOT EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));
SELECT count(*) FROM a190_na WHERE EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));
EXPLAIN (COSTS OFF) SELECT * FROM a190_na WHERE NOT EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND NOT EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 700));
SELECT count(*) FROM a190_na WHERE NOT EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND NOT EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 700));
EXPLAIN (COSTS OFF) SELECT * FROM a190_na WHERE NOT EXISTS (SELECT 1 FROM a190_nb WHERE EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));
-- A sorted DISTINCT over a written COLLATE of another collation sorts the RelabelType.
CREATE TABLE a190_dc (t text, u varchar(10));
INSERT INTO a190_dc SELECT 'v' || (g % 50), 'u' || (g % 7) FROM generate_series(1, 2000) g;
ANALYZE a190_dc;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "C" FROM a190_dc;
EXPLAIN (COSTS OFF) SELECT DISTINCT u COLLATE "C" FROM a190_dc;
SELECT DISTINCT u COLLATE "C" FROM a190_dc;
RESET enable_hashagg;
-- NOT EXISTS over a catalog with an InitPlan in its WHERE (EF Core's tables query): an anti
-- join, the sub-select an InitPlan above it.
CREATE SCHEMA a190_ic;
CREATE TABLE a190_ic.a190_t1 (id int PRIMARY KEY, a text);
EXPLAIN (COSTS OFF) SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid AND d.deptype IN ('e', 'x'))
  AND c.relname IN ('a190_t1');
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid AND d.deptype IN ('e', 'x'))
  AND c.relname IN ('a190_t1');
SELECT ns.nspname, cls.relname, cls.relkind
FROM pg_class AS cls
JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace
LEFT OUTER JOIN pg_description AS des ON des.objoid = cls.oid AND des.objsubid = 0
WHERE cls.relkind IN ('r', 'v', 'm', 'f', 'p')
  AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
  AND NOT EXISTS (
      SELECT * FROM pg_depend AS dep
      WHERE dep.classid = (SELECT cls.oid FROM pg_class AS cls JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace WHERE relname = 'pg_class' AND ns.nspname = 'pg_catalog')
        AND dep.objid = cls.oid AND dep.deptype IN ('e', 'x'))
  AND cls.relname IN ('a190_t1')
ORDER BY 2;
DROP SCHEMA a190_ic CASCADE;
-- A cursor's startup cost can favor an ordered grouping; the resulting plan determines
-- whether a backward FETCH is allowed and whether its transaction can commit.
CREATE TABLE a190_cursor (id int PRIMARY KEY, v int, w text) WITH (autovacuum_enabled = off);
CREATE INDEX a190_cursor_v ON a190_cursor (v);
INSERT INTO a190_cursor SELECT i, (i * 7) % 31, 'w' || i FROM generate_series(1, 6) i;
CREATE TABLE a190_cursor_effects (id int);
EXPLAIN (COSTS OFF) SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
EXPLAIN (COSTS OFF) DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
EXPLAIN (COSTS OFF) DECLARE c SCROLL CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
BEGIN;
INSERT INTO a190_cursor_effects VALUES (1);
DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
SELECT name, is_scrollable FROM pg_cursors ORDER BY name;
FETCH ABSOLUTE -1 FROM c;
SELECT 1;
COMMIT;
SELECT * FROM a190_cursor_effects;
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
SELECT name, is_scrollable FROM pg_cursors ORDER BY name;
FETCH ABSOLUTE -1 FROM c;
ROLLBACK;
SET cursor_tuple_fraction = 1;
EXPLAIN (COSTS OFF) DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
EXPLAIN (COSTS OFF) DECLARE c SCROLL CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
BEGIN;
DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
SELECT name, is_scrollable FROM pg_cursors ORDER BY name;
FETCH ABSOLUTE -1 FROM c;
ROLLBACK;
SET cursor_tuple_fraction = 0;
EXPLAIN (COSTS OFF) DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
BEGIN;
DECLARE c CURSOR FOR SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
SELECT name, is_scrollable FROM pg_cursors ORDER BY name;
FETCH ABSOLUTE -1 FROM c;
ROLLBACK;
RESET cursor_tuple_fraction;
EXPLAIN (COSTS OFF) SELECT v, count(*), min(id) FROM a190_cursor GROUP BY v ORDER BY v;
