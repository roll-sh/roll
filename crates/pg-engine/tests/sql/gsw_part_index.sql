-- Partitioned indexes: ON ONLY in their definitions, relhassubclass, validity going up
-- the tree as partitions' indexes attach, and the checks DefineIndex / ATTACH make.
CREATE TABLE gsw_ip (a int, b int, c text) PARTITION BY RANGE (a);
CREATE INDEX gsw_ip_idx ON gsw_ip (a);
SELECT indexdef FROM pg_indexes WHERE indexname = 'gsw_ip_idx';
SELECT pg_get_indexdef('gsw_ip_idx'::regclass);
CREATE TABLE gsw_ip1 PARTITION OF gsw_ip FOR VALUES FROM (0) TO (10);
SELECT relname, relkind, relhassubclass FROM pg_class WHERE relname LIKE 'gsw_ip%idx' ORDER BY 1;
DROP INDEX CONCURRENTLY gsw_ip_idx;
DROP INDEX gsw_ip_idx;
-- ALTER TABLE on a partitioned index
CREATE INDEX gsw_ip_c ON ONLY gsw_ip (c);
CREATE INDEX gsw_ip1_c ON gsw_ip1 (c);
ALTER TABLE gsw_ip_c ATTACH PARTITION gsw_ip1_c FOR VALUES FROM (10) TO (20);
ALTER TABLE gsw_ip_c DETACH PARTITION gsw_ip1_c;
ALTER INDEX gsw_ip_c ATTACH PARTITION gsw_ip1;
ALTER INDEX gsw_ip_c ATTACH PARTITION gsw_ip1_c;
DROP TABLE gsw_ip;
-- validity propagates up
CREATE TABLE gsw_vp (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE gsw_vp1 PARTITION OF gsw_vp FOR VALUES FROM (1) TO (1000) PARTITION BY RANGE (a);
CREATE TABLE gsw_vp11 PARTITION OF gsw_vp1 FOR VALUES FROM (1) TO (100);
CREATE INDEX ON ONLY gsw_vp1 (a);
CREATE INDEX ON ONLY gsw_vp (a);
SELECT relname, indisvalid FROM pg_class JOIN pg_index ON indexrelid = oid WHERE relname LIKE 'gsw_vp%' ORDER BY relname;
ALTER INDEX gsw_vp_a_idx ATTACH PARTITION gsw_vp1_a_idx;
SELECT relname, indisvalid FROM pg_class JOIN pg_index ON indexrelid = oid WHERE relname LIKE 'gsw_vp%' ORDER BY relname;
CREATE INDEX ON gsw_vp11 (a);
ALTER INDEX gsw_vp1_a_idx ATTACH PARTITION gsw_vp11_a_idx;
SELECT relname, indisvalid FROM pg_class JOIN pg_index ON indexrelid = oid WHERE relname LIKE 'gsw_vp%' ORDER BY relname;
SELECT relname, relhassubclass FROM pg_class WHERE relname LIKE 'gsw_vp%idx' ORDER BY 1;
DROP TABLE gsw_vp;
-- an index under a constraint takes one under a constraint
CREATE TABLE gsw_pk (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE gsw_pk1 PARTITION OF gsw_pk FOR VALUES FROM (0) TO (10);
ALTER TABLE ONLY gsw_pk ADD PRIMARY KEY (a);
CREATE INDEX gsw_pk1_a_idx ON gsw_pk1 (a);
ALTER INDEX gsw_pk_pkey ATTACH PARTITION gsw_pk1_a_idx;
DROP TABLE gsw_pk;
-- exclusion constraints on partitioned tables
CREATE TABLE gsw_ex (a int4range, b int4range, EXCLUDE USING GIST (a WITH =)) PARTITION BY RANGE (a, b);
CREATE TABLE gsw_ex (a int4range, EXCLUDE USING GIST (a WITH -|-)) PARTITION BY RANGE (a);
CREATE TABLE gsw_ex (a int4range, b int4range, EXCLUDE USING GIST (a WITH =, b WITH &&)) PARTITION BY RANGE (a);
DROP TABLE gsw_ex;
CREATE TABLE gsw_ex (a int4range, b int4range, c int4range) PARTITION BY RANGE (a, b);
ALTER TABLE gsw_ex ADD EXCLUDE USING GIST (a WITH =);
ALTER TABLE gsw_ex ADD EXCLUDE USING GIST (a WITH -|-);
ALTER TABLE gsw_ex ADD EXCLUDE USING GIST (b WITH =, c WITH &&);
ALTER TABLE gsw_ex ADD EXCLUDE USING GIST (a WITH =, b WITH =, c WITH &&);
CREATE TABLE gsw_ex1 PARTITION OF gsw_ex FOR VALUES FROM ('[1,2)', '[1,2)') TO ('[3,4)', '[3,4)') PARTITION BY RANGE (c);
DROP TABLE gsw_ex;
CREATE TABLE gsw_ux (a int, b int, UNIQUE (a, b)) PARTITION BY RANGE (a, b);
CREATE TABLE gsw_ux1 (a int, b int, UNIQUE (a)) PARTITION BY RANGE (b);
CREATE TABLE gsw_ux2 PARTITION OF gsw_ux FOR VALUES FROM (0, 0) TO (10, 10) PARTITION BY RANGE (b);
DROP TABLE gsw_ux, gsw_ux1;
-- exclusion operators must match to attach
CREATE TABLE gsw_xp (a int4range, b int4range) PARTITION BY LIST (a);
CREATE TABLE gsw_xp_1 (a int4range, b int4range);
ALTER TABLE ONLY gsw_xp ATTACH PARTITION gsw_xp_1 FOR VALUES IN ('[0,1)'::int4range);
ALTER TABLE ONLY gsw_xp ADD CONSTRAINT gsw_xp_excl EXCLUDE USING gist (a WITH =, b WITH &&);
ALTER TABLE gsw_xp_1 ADD CONSTRAINT gsw_xp_1_excl EXCLUDE USING gist (a WITH &&, b WITH &&);
ALTER INDEX gsw_xp_excl ATTACH PARTITION gsw_xp_1_excl;
ALTER TABLE gsw_xp_1 DROP CONSTRAINT gsw_xp_1_excl;
ALTER TABLE gsw_xp_1 ADD CONSTRAINT gsw_xp_1_excl EXCLUDE USING gist (a WITH =, b WITH &&);
ALTER INDEX gsw_xp_excl ATTACH PARTITION gsw_xp_1_excl;
DROP TABLE gsw_xp;
-- attaching again re-checks validity; a temporary partitioned index drops non-concurrently
CREATE TABLE gsw_rv (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE gsw_rv1 PARTITION OF gsw_rv FOR VALUES FROM (1) TO (1000) PARTITION BY RANGE (a);
CREATE TABLE gsw_rv11 PARTITION OF gsw_rv1 FOR VALUES FROM (1) TO (100);
CREATE INDEX ON ONLY gsw_rv ((a / b));
CREATE INDEX ON ONLY gsw_rv1 ((a / b));
INSERT INTO gsw_rv11 VALUES (1, 0);
CREATE INDEX CONCURRENTLY ON gsw_rv11 ((a / b));
ALTER INDEX gsw_rv1_expr_idx ATTACH PARTITION gsw_rv11_expr_idx;
ALTER INDEX gsw_rv_expr_idx ATTACH PARTITION gsw_rv1_expr_idx;
SELECT relname, indisvalid FROM pg_class JOIN pg_index ON indexrelid = oid WHERE relname LIKE 'gsw_rv%' ORDER BY relname;
DELETE FROM gsw_rv11 WHERE b = 0;
REINDEX INDEX CONCURRENTLY gsw_rv11_expr_idx;
ALTER INDEX gsw_rv1_expr_idx ATTACH PARTITION gsw_rv11_expr_idx;
SELECT relname, indisvalid FROM pg_class JOIN pg_index ON indexrelid = oid WHERE relname LIKE 'gsw_rv%' ORDER BY relname;
DROP TABLE gsw_rv;
CREATE TEMP TABLE gsw_tmp (a int) PARTITION BY RANGE (a);
CREATE INDEX ON gsw_tmp (a);
DROP INDEX CONCURRENTLY gsw_tmp_a_idx;
SELECT count(*) FROM pg_class WHERE relname = 'gsw_tmp_a_idx';
DROP TABLE gsw_tmp;
