-- Catalog values clients (pgAdmin, DBeaver, Alembic, sqlc) read, as PostgreSQL has them.
CREATE TABLE t (id int PRIMARY KEY, m text, b bigint UNIQUE, jb jsonb, arr int[], s smallint);
CREATE TABLE u (id int REFERENCES t, b bigint REFERENCES t (b), i int REFERENCES t (b), s smallint REFERENCES t);
CREATE UNIQUE INDEX t_b_id ON t (b, id);
CREATE TABLE u2 (x int, y bigint, FOREIGN KEY (y, x) REFERENCES t (b, id));

-- A sequence's columns are NOT NULL (sqlc).
CREATE SEQUENCE s;
SELECT attname, attnotnull FROM pg_attribute WHERE attrelid = 's'::regclass AND attnum > 0 ORDER BY attnum;

-- jsonb and array subscripts print as subscripts (Alembic compares index expressions).
CREATE INDEX t_jb_foo ON t ((jb['foo']));
CREATE INDEX t_jb_foo_bar ON t ((jb['foo']['bar']), (jb[0]), (arr[1]));
SELECT pg_get_indexdef('t_jb_foo'::regclass), pg_get_indexdef('t_jb_foo_bar'::regclass);
SELECT k, pg_get_indexdef('t_jb_foo_bar'::regclass, k, true) FROM generate_series(1, 3) k;
CREATE VIEW v AS SELECT jb['a'] AS a, jb['a'][1] AS a1, jb[m] AS bym, arr[1:2] AS sl, (jb -> 'x')['y'] AS xy FROM t;
SELECT pg_get_viewdef('v'::regclass, true);
SELECT pg_get_viewdef('v'::regclass);
SELECT pg_get_expr(indexprs, indrelid) FROM pg_index WHERE indexrelid = 't_jb_foo_bar'::regclass;
INSERT INTO t VALUES (1, 'k', 10, '{"foo": {"bar": 7}, "a": [1, 2], "k": "v", "1": "one", "x": {"y": true}}', '{4,5,6}', 1);
INSERT INTO t VALUES (2, '0', 20, '[10, 20, 30]', NULL, 2);
SELECT id, jb['foo'], jb['foo']['bar'], jb[1], jb['1'], jb[-1], jb[m], jb[NULL::text], arr[1] FROM t ORDER BY id;
SELECT * FROM v ORDER BY a1;
EXPLAIN (COSTS OFF, VERBOSE) SELECT jb['foo']['bar'] FROM t WHERE jb['a'][0] = '1';
-- Subscripts that differ are different expressions (an index on one is no index on another).
CREATE INDEX t_arr1 ON t ((arr[1]));
SET enable_seqscan = off;
SELECT id FROM t WHERE jb['a'][0] = '1';
SELECT id FROM t WHERE jb['foo']['bar'] = '7';
SELECT id FROM t WHERE arr[2] = 4;
SELECT id FROM t WHERE arr[1] = 4;
RESET enable_seqscan;
SELECT arr[1], count(*) FROM t GROUP BY arr[1] ORDER BY 1;
CREATE TABLE ta (a int[]);
INSERT INTO ta VALUES ('{1,9}'), ('{2,8}'), ('{3,7}');
SELECT a[1] FROM ta ORDER BY a[2];
SELECT a[1], a[2] FROM ta GROUP BY a[1], a[2] ORDER BY a[2];
DROP TABLE ta;
DROP INDEX t_arr1;

-- Argument defaults: the expression list, which pg_get_expr prints.
CREATE PROCEDURE bump(IN by int DEFAULT 1) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION f(a int, b text DEFAULT 'x', c int[] DEFAULT '{1}', d numeric DEFAULT 1.5 + 2) RETURNS int LANGUAGE sql AS 'SELECT a';
CREATE FUNCTION g(a int) RETURNS int LANGUAGE sql AS 'SELECT a';
SELECT proname, pronargdefaults, proargdefaults IS NULL AS null_defaults, pg_get_expr(proargdefaults, 0)
FROM pg_proc WHERE proname IN ('bump', 'f', 'g') ORDER BY 1;

-- A materialized view has its _RETURN rule (pgAdmin's properties filter on relhasrules).
CREATE MATERIALIZED VIEW mv AS SELECT m, count(*) AS n FROM t GROUP BY m;
SELECT relname, relkind, relhasrules FROM pg_class WHERE relname IN ('v', 'mv', 't') ORDER BY 1;

-- A foreign key's equality operators (PK = FK, PK = PK, FK = FK).
SELECT conname, conpfeqop::regoper[], conppeqop::regoper[], conffeqop::regoper[],
  conpfeqop, conppeqop, conffeqop
FROM pg_constraint WHERE conrelid IN ('u'::regclass, 'u2'::regclass) ORDER BY conname;
SELECT conname, conpfeqop IS NULL FROM pg_constraint WHERE conrelid = 't'::regclass ORDER BY conname;

-- pg_get_indexdef of a column number no column has: '' (NULL for no index).
CREATE INDEX t_inc ON t (id, (s + 1)) INCLUDE (m);
SELECT k, quote_nullable(pg_get_indexdef('t_inc'::regclass, k, true)), quote_nullable(pg_get_indexdef('t_inc'::regclass, k, false))
FROM generate_series(-1, 5) k;
SELECT k, quote_nullable(pg_get_indexdef('pg_class_oid_index'::regclass, k, true)) FROM generate_series(-1, 2) k;
SELECT quote_nullable(pg_get_indexdef('t'::regclass, 1, true));

-- Recovery control functions outside recovery (pgAdmin's check_recovery.sql).
SELECT pg_is_in_recovery();
SELECT pg_is_wal_replay_paused();
SELECT pg_get_wal_replay_pause_state();
SELECT pg_wal_replay_pause();
SELECT pg_wal_replay_resume();
SELECT pg_last_wal_replay_lsn();

-- A native SUBSCRIPT handler's fetch (hstore) prints as a subscript too.
CREATE EXTENSION hstore;
CREATE TABLE th (h hstore);
CREATE INDEX th_k ON th ((h['k']));
CREATE VIEW vh AS SELECT h['k'] AS k FROM th;
SELECT pg_get_indexdef('th_k'::regclass), pg_get_indexdef('th_k'::regclass, 1, true);
SELECT pg_get_viewdef('vh'::regclass);
INSERT INTO th VALUES ('k=>v');
SELECT * FROM vh;
DROP TABLE th CASCADE;
DROP EXTENSION hstore;
