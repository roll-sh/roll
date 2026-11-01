-- A hash join whose build can be seen (agent 143): ExecHashJoin reads the outer side's
-- first row before building when the outer side starts sooner, so over an empty outer
-- side a build that raises notices or calls volatile functions never runs. A build that
-- can't be seen still runs first (the probes stay in a parallel scan's workers).
CREATE TABLE a143_e (id int, k int);
CREATE TABLE a143_s (k int, z int);
INSERT INTO a143_e SELECT i, i % 50 FROM generate_series(1, 10000) i;
INSERT INTO a143_s SELECT i, i % 5 FROM generate_series(1, 20) i;
CREATE SEQUENCE a143_q;
CREATE FUNCTION a143_noisy(int) RETURNS bool LANGUAGE plpgsql AS
  $$ BEGIN RAISE NOTICE 'build %', $1; RETURN true; END $$;
CREATE FUNCTION a143_quiet_notice(int) RETURNS bool LANGUAGE plpgsql IMMUTABLE AS
  $$ BEGIN IF $1 = 3 THEN RAISE NOTICE 'immutable build %', $1; END IF; RETURN true; END $$;
ANALYZE a143_e;
ANALYZE a143_s;
SET enable_mergejoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF) SELECT e.id FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN a143_s s ON s.k = e.k WHERE a143_noisy(s.k);
SELECT e.id FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN a143_s s ON s.k = e.k WHERE a143_noisy(s.k);
SELECT e.id FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN a143_s s ON s.k = e.k WHERE a143_quiet_notice(s.z + s.k);
SELECT count(*) FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN (SELECT k, nextval('a143_q') AS n FROM a143_s) s ON s.k = e.k;
SELECT nextval('a143_q');
SELECT count(*) FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e LEFT JOIN a143_s s ON s.k = e.k AND a143_noisy(s.k);
-- with an outer row the build runs
INSERT INTO a143_e VALUES (10006, 3);
SELECT e.id FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN a143_s s ON s.k = e.k WHERE a143_noisy(s.k) AND s.k < 3;
SELECT count(*) FROM (SELECT * FROM a143_e WHERE id % 10007 = 10006) e JOIN (SELECT k, nextval('a143_q') AS n FROM a143_s) s ON s.k = e.k;
SELECT nextval('a143_q');
RESET enable_mergejoin;
RESET enable_nestloop;
DROP FUNCTION a143_noisy(int);
DROP FUNCTION a143_quiet_notice(int);
DROP SEQUENCE a143_q;
DROP TABLE a143_e;
DROP TABLE a143_s;
