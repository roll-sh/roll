-- Round 16, agent 15: a view's whole row over its pulled-up subquery is a ROW() of the
-- subquery's outputs typed as the view's row (pullup_replace_vars), not the scanned table's
-- whole-row Var; a composite type's ROW() stays a ROW(); a table's whole row is `t.*`.
CREATE TABLE a15_base (id int, stuff text);
INSERT INTO a15_base VALUES (23, 'skidoo');
CREATE VIEW a15_v1 AS SELECT * FROM a15_base bt;
CREATE VIEW a15_v2 AS SELECT *, to_json(v1) AS j FROM a15_v1 v1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a15_v2;
SELECT * FROM a15_v2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v1 FROM a15_v1 v1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT bt, to_json(bt) FROM a15_base bt;
CREATE OR REPLACE VIEW a15_v1 AS SELECT *, 2 + 2 AS more FROM a15_base bt;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM a15_v2;
SELECT * FROM a15_v2;
CREATE TYPE a15_comp AS (id int, stuff text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT ROW(bt.id, bt.stuff)::a15_comp FROM a15_base bt;
CREATE MATERIALIZED VIEW a15_mv AS SELECT * FROM a15_base;
EXPLAIN (VERBOSE, COSTS OFF) SELECT m FROM a15_mv m;
-- Catalog scans repeated per outer row (a nested loop's parameterized inner side, a
-- correlated sub-select) return what one scan would.
SELECT a.attname, (SELECT t.typname FROM pg_type t WHERE t.oid = a.atttypid) AS typ
  FROM pg_attribute a WHERE a.attrelid = 'a15_base'::regclass AND a.attnum > 0 ORDER BY a.attnum;
SELECT c.relname, a.attname, t.typname
  FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid JOIN pg_type t ON t.oid = a.atttypid
  WHERE c.relname IN ('a15_base', 'a15_mv') AND a.attnum > 0 ORDER BY 1, 2;
SELECT * FROM information_schema.column_domain_usage WHERE table_name LIKE 'a15%';
CREATE DOMAIN a15_dom AS int CHECK (VALUE > 0);
CREATE TABLE a15_dt (d a15_dom, e a15_dom);
SELECT domain_name, table_name, column_name FROM information_schema.column_domain_usage
  WHERE table_name LIKE 'a15%' ORDER BY column_name;
SELECT n.nspname, (SELECT count(*) FROM pg_class c WHERE c.relnamespace = n.oid AND c.relname LIKE 'a15%') AS n
  FROM pg_namespace n WHERE n.nspname IN ('public', 'pg_catalog') ORDER BY 1;
-- A text column compared with a name pseudoconstant (texteqname) is its class's one source
-- clause, used as written.
CREATE TABLE a15_cust (cid int PRIMARY KEY, name text NOT NULL, passwd text);
CREATE VIEW a15_cust_v AS SELECT * FROM a15_cust WHERE name = current_user;
EXPLAIN (COSTS OFF) SELECT * FROM a15_cust_v WHERE passwd = 'x';
EXPLAIN (COSTS OFF) SELECT * FROM a15_cust WHERE name = current_user;
DROP VIEW a15_cust_v;
DROP TABLE a15_cust;
DROP TABLE a15_dt;
DROP DOMAIN a15_dom;
DROP MATERIALIZED VIEW a15_mv;
DROP TYPE a15_comp;
DROP VIEW a15_v2;
DROP VIEW a15_v1;
DROP TABLE a15_base;
