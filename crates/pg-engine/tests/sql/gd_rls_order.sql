-- Row-level security: policy quals run before the statement's own (leakproof cheap ones may
-- go first), views over policies, COPY TO, FK details, SETOF composite columns.
DROP ROLE IF EXISTS gd_rl_u;
CREATE ROLE gd_rl_u;
CREATE FUNCTION gd_rl_leak(text) RETURNS bool COST 0.0000001 LANGUAGE plpgsql
  AS 'BEGIN RAISE NOTICE ''leak => %'', $1; RETURN true; END';
GRANT EXECUTE ON FUNCTION gd_rl_leak(text) TO PUBLIC;
CREATE TABLE gd_rl_t (a int, b text);
INSERT INTO gd_rl_t VALUES (1, 'aaa'), (2, 'bbb'), (3, 'ccc'), (4, 'ddd');
GRANT ALL ON gd_rl_t TO gd_rl_u;
CREATE POLICY gd_rl_p ON gd_rl_t USING (a % 2 = 0);
ALTER TABLE gd_rl_t ENABLE ROW LEVEL SECURITY;
SET SESSION AUTHORIZATION gd_rl_u;
SELECT * FROM gd_rl_t WHERE gd_rl_leak(b) ORDER BY a;
UPDATE gd_rl_t SET b = b WHERE gd_rl_leak(b);
DELETE FROM gd_rl_t WHERE a = 1 AND gd_rl_leak(b);
SELECT * FROM gd_rl_t WHERE gd_rl_leak('x') ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM gd_rl_t WHERE a <= 2;
EXPLAIN (COSTS OFF) UPDATE gd_rl_t SET b = b WHERE a <= 2;
EXPLAIN (COSTS OFF) DELETE FROM gd_rl_t WHERE gd_rl_leak(b);
RESET SESSION AUTHORIZATION;
-- a view over a table with policies: policies apply when the view is used, not when made
CREATE TABLE gd_rl_r1 (x int, y int);
CREATE TABLE gd_rl_r2 (a int, b int);
CREATE POLICY gd_rl_r1p ON gd_rl_r1 USING (x = (SELECT a FROM gd_rl_r2 WHERE b = y));
CREATE POLICY gd_rl_r2p ON gd_rl_r2 USING (a = (SELECT x FROM gd_rl_r1 WHERE y = b));
ALTER TABLE gd_rl_r1 ENABLE ROW LEVEL SECURITY;
ALTER TABLE gd_rl_r2 ENABLE ROW LEVEL SECURITY;
GRANT ALL ON gd_rl_r1, gd_rl_r2 TO gd_rl_u;
CREATE VIEW gd_rl_v1 AS SELECT * FROM gd_rl_r1;
GRANT ALL ON gd_rl_v1 TO gd_rl_u;
ALTER POLICY gd_rl_r2p ON gd_rl_r2 USING (a = (SELECT x FROM gd_rl_v1 WHERE y = b));
SET SESSION AUTHORIZATION gd_rl_u;
SELECT * FROM gd_rl_r1;
SELECT * FROM gd_rl_v1;
RESET SESSION AUTHORIZATION;
DROP VIEW gd_rl_v1 CASCADE;
-- COPY TO: privileges are checked before row security
CREATE TABLE gd_rl_c (a int, b text);
CREATE POLICY gd_rl_cp ON gd_rl_c USING (a % 2 = 0);
ALTER TABLE gd_rl_c ENABLE ROW LEVEL SECURITY;
SET SESSION AUTHORIZATION gd_rl_u;
SET row_security TO OFF;
COPY gd_rl_c TO STDOUT;
RESET row_security;
RESET SESSION AUTHORIZATION;
-- a foreign key violation shows no key under row security / without SELECT
CREATE TABLE gd_rl_pk (id int PRIMARY KEY);
CREATE TABLE gd_rl_fk (id int REFERENCES gd_rl_pk);
INSERT INTO gd_rl_pk VALUES (1);
INSERT INTO gd_rl_fk VALUES (1);
GRANT DELETE, SELECT ON gd_rl_pk TO gd_rl_u;
GRANT INSERT ON gd_rl_fk TO gd_rl_u;
ALTER TABLE gd_rl_pk ENABLE ROW LEVEL SECURITY;
CREATE POLICY gd_rl_pkp ON gd_rl_pk USING (true);
SET SESSION AUTHORIZATION gd_rl_u;
DELETE FROM gd_rl_pk WHERE id = 1;
INSERT INTO gd_rl_fk VALUES (2);
RESET SESSION AUTHORIZATION;
CREATE POLICY gd_rl_bad ON gd_rl_pk AS UGLY USING (true);
-- a SETOF function of a table's row type has the table's columns
CREATE TABLE gd_rl_rt (c text);
INSERT INTO gd_rl_rt VALUES ('x');
CREATE FUNCTION gd_rl_f() RETURNS SETOF gd_rl_rt STABLE LANGUAGE sql
  AS $$ SELECT exists(SELECT * FROM gd_rl_rt)::text $$;
SELECT * FROM gd_rl_f();
CREATE OR REPLACE FUNCTION gd_rl_f() RETURNS SETOF gd_rl_rt STABLE LANGUAGE sql
  AS $$ SELECT * FROM (SELECT array_agg(c) AS cs FROM gd_rl_rt) _ GROUP BY cs $$;
SELECT * FROM gd_rl_f();
DROP FUNCTION gd_rl_f();
DROP TABLE gd_rl_t, gd_rl_r1, gd_rl_r2, gd_rl_c, gd_rl_fk, gd_rl_pk, gd_rl_rt;
DROP FUNCTION gd_rl_leak(text);
DROP ROLE gd_rl_u;
