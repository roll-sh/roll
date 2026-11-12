-- VACUUM FULL and CLUSTER copy each row with its xmin and freeze what they copy: relfrozenxid
-- moves to the copy's cutoff, and the first VACUUM after either finds nothing older and
-- moves it on (its new relfrozenxid line). GIN's pending list: the pages VACUUM's cleanup
-- deletes, and those free for reuse after. (The golden harness masks transaction ids.)
-- Agent 222.
CREATE TABLE rw_h (a int, b text) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
CREATE INDEX rw_h_a ON rw_h (a);
INSERT INTO rw_h SELECT g, 'x' FROM generate_series(1, 50) g;
INSERT INTO rw_h SELECT g, 'y' FROM generate_series(51, 100) g;
DELETE FROM rw_h WHERE a % 10 = 0;
UPDATE rw_h SET b = 'z' WHERE a % 10 = 1;
CREATE TABLE rw_k (a int PRIMARY KEY, b text) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
INSERT INTO rw_k SELECT g, 'x' FROM generate_series(1, 50) g;
INSERT INTO rw_k SELECT g, 'y' FROM generate_series(51, 100) g;
CREATE TEMP TABLE rw_x AS SELECT 'h' AS t, a, xmin AS x FROM rw_h UNION ALL SELECT 'k', a, xmin FROM rw_k;
CLUSTER rw_h USING rw_h_a;
SELECT count(*) FILTER (WHERE r.xm = x.x) AS same_xmin, count(*) AS n FROM (SELECT a, xmin AS xm FROM rw_h) r JOIN rw_x x ON x.t = 'h' AND x.a = r.a;
VACUUM (VERBOSE) rw_h;
SELECT age(relfrozenxid) < 3 AS young FROM pg_class WHERE relname = 'rw_h';
VACUUM FULL rw_h;
SELECT count(*) FILTER (WHERE r.xm = x.x) AS same_xmin, count(*) AS n FROM (SELECT a, xmin AS xm FROM rw_h) r JOIN rw_x x ON x.t = 'h' AND x.a = r.a;
VACUUM (VERBOSE) rw_h;
VACUUM FULL rw_k;
CLUSTER rw_k USING rw_k_pkey;
SELECT count(*) FILTER (WHERE r.xm = x.x) AS same_xmin, count(*) AS n FROM (SELECT a, xmin AS xm FROM rw_k) r JOIN rw_x x ON x.t = 'k' AND x.a = r.a;
VACUUM (VERBOSE) rw_k;
-- GIN: rows inserted after the build wait in the pending list; VACUUM moves them into the
-- tree and deletes the list's pages, free for the next list.
CREATE TABLE rw_g (id int, t int[]) WITH (autovacuum_enabled = off, toast.autovacuum_enabled = off);
CREATE INDEX rw_g_t ON rw_g USING gin (t);
INSERT INTO rw_g SELECT g, ARRAY[g % 10, g % 7] FROM generate_series(1, 200) g;
INSERT INTO rw_g SELECT g, ARRAY[g % 13, g % 5, g] FROM generate_series(1, 3000) g;
VACUUM (VERBOSE) rw_g;
VACUUM (VERBOSE) rw_g;
INSERT INTO rw_g SELECT g, ARRAY[g % 3] FROM generate_series(1, 2000) g;
VACUUM (VERBOSE) rw_g;
