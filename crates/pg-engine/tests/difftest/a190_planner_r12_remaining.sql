-- Unresolved round-12 EXPLAIN differences, retained from the original lane-190 probe.
-- Run in a fresh database on each server. These seven plans still differ from PG 18.6.
-- Gaps: functional dependency in scan order, bounded grouping sort, DELETE RETURNING
-- width, semi-join gating, DISTINCT order propagation, and catalog join paths.

CREATE TABLE a190_g (id int PRIMARY KEY, x int);

INSERT INTO a190_g SELECT i, i % 7 FROM generate_series(1, 10000) i;

ANALYZE a190_g;

EXPLAIN SELECT x, id, count(*) FROM a190_g GROUP BY id, x ORDER BY id DESC LIMIT 5;

EXPLAIN SELECT id, count(*) FROM a190_g GROUP BY id ORDER BY id DESC NULLS LAST LIMIT 5;

SET enable_hashagg = off;

RESET enable_hashagg;

CREATE TABLE a190_b (id int PRIMARY KEY, v int);

CREATE TABLE a190_s (id int PRIMARY KEY, w int);

INSERT INTO a190_b SELECT g, g FROM generate_series(1, 1000) g;

INSERT INTO a190_s SELECT g, g FROM generate_series(1, 1000) g;

ANALYZE a190_b;

ANALYZE a190_s;

EXPLAIN DELETE FROM a190_b r1 USING a190_s r2 WHERE r1.id = r2.id AND r2.w < 3 RETURNING r2.w;

CREATE TABLE a190_na (id int PRIMARY KEY, x int);

CREATE TABLE a190_nb (id int PRIMARY KEY, y int);

INSERT INTO a190_na SELECT g, g % 100 FROM generate_series(1, 1000) g;

INSERT INTO a190_nb SELECT g, g % 50 FROM generate_series(1, 1000) g;

ANALYZE a190_na;

ANALYZE a190_nb;

EXPLAIN (COSTS OFF) SELECT * FROM a190_na WHERE EXISTS (SELECT 1 FROM a190_nb WHERE a190_nb.y = a190_na.x AND EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));

EXPLAIN (COSTS OFF) SELECT * FROM a190_na WHERE x IN (SELECT y FROM a190_nb WHERE EXISTS (SELECT 1 FROM a190_na n2 WHERE n2.x = 7));

CREATE TABLE a190_dc (t text, u varchar(10));

INSERT INTO a190_dc SELECT 'v' || (g % 50), 'u' || (g % 7) FROM generate_series(1, 2000) g;

ANALYZE a190_dc;

SET enable_hashagg = off;

EXPLAIN (COSTS OFF) SELECT DISTINCT t COLLATE "C" AS x FROM a190_dc ORDER BY 1;

RESET enable_hashagg;

CREATE SCHEMA a190_ic;

CREATE TABLE a190_ic.a190_t1 (id int PRIMARY KEY, a text);

EXPLAIN (COSTS OFF) SELECT ns.nspname, cls.relname, cls.relkind
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
