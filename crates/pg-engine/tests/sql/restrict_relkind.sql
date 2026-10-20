-- restrict_nonsystem_relation_kind: non-system views aren't expanded, foreign tables
-- aren't planned.
CREATE TABLE rr_tbl (a int);
CREATE VIEW rr_v AS SELECT a FROM rr_tbl;
CREATE FOREIGN DATA WRAPPER rr_fdw;
CREATE SERVER rr_srv FOREIGN DATA WRAPPER rr_fdw;
CREATE FOREIGN TABLE rr_ft (a int) SERVER rr_srv;
SET restrict_nonsystem_relation_kind TO 'view';
SHOW restrict_nonsystem_relation_kind;
SELECT a FROM rr_v WHERE a > 0;
INSERT INTO rr_v VALUES (1);
UPDATE rr_v SET a = 2;
DELETE FROM rr_v;
MERGE INTO rr_v t USING (SELECT 1 a) s ON t.a = s.a WHEN MATCHED THEN DELETE;
SELECT * FROM rr_tbl, LATERAL (SELECT * FROM rr_v) s;
EXPLAIN (COSTS OFF) SELECT * FROM rr_v;
WITH x AS (SELECT * FROM rr_v) SELECT 1;
COPY (SELECT * FROM rr_v) TO stdout;
CREATE TABLE rr_tbl2 AS SELECT * FROM rr_v;
-- Only analyzed, not planned: allowed.
CREATE VIEW rr_v2 AS SELECT * FROM rr_v;
SELECT pg_get_viewdef('rr_v'::regclass);
-- System views are fine.
SELECT count(*) FROM pg_views WHERE viewname = 'rr_v';
SELECT table_name FROM information_schema.tables WHERE table_name = 'rr_tbl';
SET restrict_nonsystem_relation_kind TO 'foreign-table';
SELECT * FROM rr_ft;
INSERT INTO rr_ft VALUES (1);
SELECT a FROM rr_v;
SET restrict_nonsystem_relation_kind TO 'view, foreign-table';
SHOW restrict_nonsystem_relation_kind;
SELECT * FROM rr_ft;
SET restrict_nonsystem_relation_kind TO 'table';
SET restrict_nonsystem_relation_kind TO 'view,';
SET restrict_nonsystem_relation_kind TO '';
SELECT a FROM rr_v;
SET restrict_nonsystem_relation_kind TO 'VIEW';
SHOW restrict_nonsystem_relation_kind;
SELECT * FROM rr_v;
RESET restrict_nonsystem_relation_kind;
SELECT * FROM rr_v;
