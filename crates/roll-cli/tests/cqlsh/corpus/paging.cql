-- Paging, EXPAND across pages, empty results, LWT results and warnings.
DROP KEYSPACE IF EXISTS cli_g_paging;
CREATE KEYSPACE cli_g_paging WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
USE cli_g_paging;
CREATE TABLE t (k int, c int, v text, PRIMARY KEY (k, c));
INSERT INTO t (k, c, v) VALUES (1, 1, 'a');
INSERT INTO t (k, c, v) VALUES (1, 2, 'bb');
INSERT INTO t (k, c, v) VALUES (1, 3, 'ccc');
INSERT INTO t (k, c, v) VALUES (2, 1, 'dddd');
INSERT INTO t (k, c, v) VALUES (3, 1, 'eeeee');
INSERT INTO t (k, c, v) VALUES (4, 1, 'ffffffffffff');
PAGING 2
SELECT * FROM t;
EXPAND ON
SELECT * FROM t;
EXPAND OFF
PAGING 100
SELECT * FROM t WHERE k = 1 AND c > 5;
SELECT count(*) FROM t;
SELECT * FROM t WHERE v = 'a' ALLOW FILTERING;
PAGING OFF
SELECT * FROM t;
INSERT INTO t (k, c, v) VALUES (1, 1, 'x') IF NOT EXISTS;
INSERT INTO t (k, c, v) VALUES (9, 9, 'new') IF NOT EXISTS;
UPDATE t SET v = 'z' WHERE k = 1 AND c = 1 IF v = 'nope';
UPDATE t SET v = 'z' WHERE k = 1 AND c = 1 IF v = 'a';
DELETE FROM t WHERE k = 77 AND c = 1 IF EXISTS;
BEGIN UNLOGGED BATCH INSERT INTO t (k, c, v) VALUES (20, 1, 'a'); INSERT INTO t (k, c, v) VALUES (21, 1, 'b'); APPLY BATCH;
SELECT k, c, v FROM t WHERE k IN (20, 21);
TRUNCATE t;
SELECT * FROM t;
DROP KEYSPACE cli_g_paging;
