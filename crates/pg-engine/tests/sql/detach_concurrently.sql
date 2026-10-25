-- DETACH PARTITION ... CONCURRENTLY run alone, and the errors around it.
CREATE TABLE dc (a int, b text) PARTITION BY LIST (a);
CREATE TABLE dc1 PARTITION OF dc FOR VALUES IN (1, 2);
CREATE TABLE dc2 PARTITION OF dc FOR VALUES IN (3);
CREATE TABLE dcr (a int PRIMARY KEY) PARTITION BY RANGE (a);
CREATE TABLE dcr1 PARTITION OF dcr FOR VALUES FROM (0) TO (10);
CREATE TABLE dcr2 PARTITION OF dcr FOR VALUES FROM (10) TO (20);
CREATE TABLE dcref (a int REFERENCES dcr);
INSERT INTO dc VALUES (1, 'x'), (3, 'y');
INSERT INTO dcr VALUES (1), (15);
INSERT INTO dcref VALUES (15);
-- In a transaction block: refused.
BEGIN;
ALTER TABLE dc DETACH PARTITION dc1 CONCURRENTLY;
ROLLBACK;
-- No pending detach to finish.
ALTER TABLE dc DETACH PARTITION dc1 FINALIZE;
ALTER TABLE dc DETACH PARTITION dcr1 CONCURRENTLY;
-- Detached: the partition keeps its constraint as a CHECK constraint.
ALTER TABLE dc DETACH PARTITION dc1 CONCURRENTLY;
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'dc1'::regclass ORDER BY 1;
SELECT relname, relispartition, relpartbound IS NULL FROM pg_class WHERE relname LIKE 'dc_' ORDER BY 1;
SELECT inhrelid::regclass, inhparent::regclass, inhdetachpending FROM pg_inherits WHERE inhparent = 'dc'::regclass;
SELECT * FROM dc ORDER BY a;
SELECT * FROM dc1 ORDER BY a;
-- A key still referenced through the partitioned table: the detach fails, nothing changes.
ALTER TABLE dcr DETACH PARTITION dcr2 CONCURRENTLY;
SELECT inhrelid::regclass, inhdetachpending FROM pg_inherits WHERE inhparent = 'dcr'::regclass ORDER BY 1;
ALTER TABLE dcr DETACH PARTITION dcr1 CONCURRENTLY;
SELECT conname, conrelid::regclass, confrelid::regclass FROM pg_constraint WHERE contype = 'f' ORDER BY 1, 2;
-- A default partition rules it out.
CREATE TABLE dcd PARTITION OF dc DEFAULT;
ALTER TABLE dc DETACH PARTITION dc2 CONCURRENTLY;
ALTER TABLE dc DETACH PARTITION dc2;
SELECT * FROM pg_partition_tree('dc') ORDER BY relid::text;
-- Text bounds: the CHECK constraint holds them as one array constant.
CREATE TABLE dct (t text) PARTITION BY LIST (t);
CREATE TABLE dct1 PARTITION OF dct FOR VALUES IN ('a b', 'x', 'it''s', NULL);
ALTER TABLE dct DETACH PARTITION dct1 CONCURRENTLY;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'dct1'::regclass;
DROP TABLE dc, dc1, dc2, dcref, dcr, dcr1, dct, dct1;
