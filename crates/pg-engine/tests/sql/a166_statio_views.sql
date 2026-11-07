-- pg_statio_* views generated in one pass, and the statistics views of one relation alone
-- when the query names its oid (relid = ..., indexrelid = ...).
CREATE TABLE a166_io (id int PRIMARY KEY, a text, b int UNIQUE);
CREATE INDEX a166_io_a ON a166_io (a);
CREATE TABLE a166_io_noix (x int);
CREATE TABLE a166_io_part (k int) PARTITION BY RANGE (k);
CREATE TABLE a166_io_p1 PARTITION OF a166_io_part FOR VALUES FROM (0) TO (10);
CREATE MATERIALIZED VIEW a166_io_mv AS SELECT 1 AS one;
CREATE SEQUENCE a166_io_seq;
SELECT relname, schemaname, heap_blks_read >= 0 AS heap, idx_blks_read IS NULL AS no_index,
       toast_blks_read IS NULL AS no_toast, tidx_blks_hit IS NULL AS no_tidx
  FROM pg_statio_user_tables WHERE relname LIKE 'a166_io%' ORDER BY relname;
SELECT relname, idx_blks_hit >= 0, toast_blks_hit >= 0, tidx_blks_read >= 0
  FROM pg_statio_all_tables WHERE relname = 'a166_io';
SELECT count(*) FROM pg_statio_user_tables WHERE relname = 'a166_io_part';
SELECT relname, schemaname, toast_blks_read IS NULL AS no_toast FROM pg_statio_sys_tables WHERE relname = 'pg_proc';
SELECT relname, indexrelname, idx_blks_read >= 0, idx_blks_hit >= 0
  FROM pg_statio_user_indexes WHERE relname = 'a166_io' ORDER BY indexrelname;
SELECT relname, indexrelname FROM pg_statio_all_indexes WHERE schemaname = 'pg_catalog' AND relname = 'pg_class' ORDER BY 2;
SELECT relname, blks_read >= 0, blks_hit >= 0 FROM pg_statio_user_sequences WHERE relname = 'a166_io_seq';
SELECT count(*) FROM pg_statio_sys_sequences;
SELECT count(*) > 0 FROM pg_statio_sys_indexes WHERE schemaname = 'pg_toast';
-- By oid.
SELECT relname FROM pg_stat_user_tables WHERE relid = 'a166_io'::regclass;
SELECT relname FROM pg_stat_all_tables WHERE relid = 'pg_class'::regclass;
SELECT count(*) FROM pg_stat_user_tables WHERE relid = 'pg_class'::regclass;
SELECT count(*) FROM pg_stat_user_tables WHERE relid = 'a166_io_seq'::regclass;
SELECT relname, n_tup_ins >= 0 FROM pg_stat_user_tables WHERE relid = 'a166_io'::regclass AND relname = 'a166_io';
SELECT count(*) FROM pg_stat_user_tables WHERE relid = 'a166_io'::regclass AND relname = 'a166_io_noix';
SELECT relname, indexrelname FROM pg_stat_user_indexes WHERE relid = 'a166_io'::regclass ORDER BY 2;
SELECT relname, indexrelname FROM pg_stat_user_indexes WHERE indexrelid = 'a166_io_a'::regclass;
SELECT count(*) FROM pg_stat_user_indexes WHERE indexrelid = 'a166_io_a'::regclass AND relid = 'a166_io_noix'::regclass;
SELECT relname, indexrelname FROM pg_statio_user_indexes WHERE indexrelid = 'a166_io_pkey'::regclass;
SELECT relname FROM pg_statio_user_tables WHERE relid = 'a166_io_mv'::regclass;
SELECT relname FROM pg_statio_user_sequences WHERE relid = 'a166_io_seq'::regclass;
SELECT relname, indexrelname FROM pg_stat_sys_indexes WHERE relid = 'pg_namespace'::regclass ORDER BY 2;
-- A TOAST table by name and by oid.
SELECT count(*) FROM pg_stat_sys_tables s JOIN pg_class c ON c.reltoastrelid = s.relid WHERE c.relname = 'a166_io';
SELECT count(*) FROM pg_statio_all_indexes s JOIN pg_class c ON c.reltoastrelid = s.relid WHERE c.relname = 'a166_io';
DROP SEQUENCE a166_io_seq;
DROP MATERIALIZED VIEW a166_io_mv;
DROP TABLE a166_io, a166_io_noix, a166_io_part;
