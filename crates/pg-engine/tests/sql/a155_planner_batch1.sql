-- Planner round 9, batch 1 (agent 155).
-- Liquibase's column check: an EXISTS over a catalog join with a qual on each side, its
-- correlation inside the inner side of a parameterized nested loop, pulled up to a semi join.
CREATE TABLE a155_t77 (id int, c5 int, c6 text);
CREATE TABLE a155_t78 (id int, c5 int);
SELECT 1 FROM pg_attribute a WHERE EXISTS (SELECT 1 FROM pg_class JOIN pg_catalog.pg_namespace ns
  ON ns.oid = pg_class.relnamespace WHERE lower(ns.nspname) = 'public' AND lower(relname) = 'a155_t77'
  AND pg_class.oid = a.attrelid) AND lower(a.attname) = 'c5';
SELECT a.attname FROM pg_attribute a WHERE EXISTS (SELECT 1 FROM pg_class JOIN pg_catalog.pg_namespace ns
  ON ns.oid = pg_class.relnamespace WHERE ns.nspname = 'public' AND relname LIKE 'a155_t7%'
  AND pg_class.oid = a.attrelid) AND a.attnum > 0 ORDER BY 1;
SELECT count(*) FROM pg_attribute a WHERE NOT EXISTS (SELECT 1 FROM pg_class JOIN pg_catalog.pg_namespace ns
  ON ns.oid = pg_class.relnamespace WHERE lower(ns.nspname) = 'public' AND lower(relname) = 'a155_t77'
  AND pg_class.oid = a.attrelid) AND lower(a.attname) = 'c5';
-- The same shape over user tables.
CREATE TABLE a155_ns (oid int PRIMARY KEY, nspname text);
CREATE TABLE a155_cls (oid int PRIMARY KEY, relname text, relnamespace int);
CREATE TABLE a155_att (attrelid int, attname text);
INSERT INTO a155_ns SELECT g, 'ns' || g FROM generate_series(1, 20) g;
INSERT INTO a155_cls SELECT g, 'r' || g, g % 20 + 1 FROM generate_series(1, 500) g;
INSERT INTO a155_att SELECT g % 500 + 1, 'c' || (g % 7) FROM generate_series(1, 5000) g;
ANALYZE a155_ns, a155_cls, a155_att;
SELECT count(*) FROM a155_att a WHERE EXISTS (SELECT 1 FROM a155_cls c JOIN a155_ns ns ON ns.oid = c.relnamespace
  WHERE lower(ns.nspname) = 'ns3' AND c.oid = a.attrelid) AND a.attname = 'c5';
SELECT count(*) FROM a155_att a WHERE EXISTS (SELECT 1 FROM a155_cls c JOIN a155_ns ns ON ns.oid = c.relnamespace
  WHERE ns.nspname = 'ns3' AND c.relname <> 'x' AND c.oid = a.attrelid);
-- Catalog statistics: initdb's before an ANALYZE of the catalog, the catalog's own after.
SELECT attname, null_frac, n_distinct, most_common_vals FROM pg_stats
  WHERE schemaname = 'pg_catalog' AND tablename = 'pg_class' AND attname IN ('relkind', 'relpersistence') ORDER BY 1;
ANALYZE pg_class (relkind, nosuch);
ANALYZE pg_class (relkind, relkind);
ANALYZE pg_class (relpersistence);
SELECT attname, null_frac, n_distinct, most_common_vals FROM pg_stats
  WHERE schemaname = 'pg_catalog' AND tablename = 'pg_class' AND attname IN ('relkind', 'relpersistence') ORDER BY 1;
SELECT count(*) FROM pg_statistic WHERE starelid = 'pg_class'::regclass AND staattnum = 17;
SELECT relname, reltuples > 400 AS counted FROM pg_class WHERE relname = 'pg_class';
DROP TABLE a155_t77, a155_t78, a155_ns, a155_cls, a155_att;
