-- pg_stat_*_tables and pg_stat_*_indexes generated in one pass, and of one table alone when
-- the query names it.
CREATE TABLE a146_st (id int PRIMARY KEY, a text, b int UNIQUE);
CREATE INDEX a146_st_a ON a146_st (a);
CREATE TABLE a146_st_noix (x int);
CREATE TABLE a146_st_part (k int) PARTITION BY RANGE (k);
CREATE TABLE a146_st_p1 PARTITION OF a146_st_part FOR VALUES FROM (0) TO (10);
CREATE MATERIALIZED VIEW a146_st_mv AS SELECT 1 AS one;
SELECT relname, schemaname, idx_scan IS NULL AS no_index, idx_tup_fetch IS NULL AS no_fetch, n_tup_ins >= 0 AS counted
  FROM pg_stat_user_tables WHERE relname LIKE 'a146_st%' ORDER BY relname;
SELECT relname, idx_scan IS NULL FROM pg_stat_user_tables WHERE relname = 'a146_st';
SELECT relname, idx_scan IS NULL FROM pg_stat_user_tables WHERE relname = 'a146_st_noix';
SELECT count(*) FROM pg_stat_user_tables WHERE relname = 'a146_no_such_table';
SELECT count(*) FROM pg_stat_user_tables WHERE schemaname = 'public' AND relname = 'a146_st_part';
SELECT relname, schemaname FROM pg_stat_sys_tables WHERE relname = 'pg_class';
SELECT count(*) FROM pg_stat_all_tables WHERE relname = 'pg_class' AND schemaname = 'pg_catalog';
SELECT relname, indexrelname, idx_scan >= 0, idx_tup_read >= 0, idx_tup_fetch >= 0
  FROM pg_stat_user_indexes WHERE relname = 'a146_st' ORDER BY indexrelname;
SELECT relname, indexrelname FROM pg_stat_user_indexes WHERE relname LIKE 'a146_st%' ORDER BY 1, 2;
SELECT relname, indexrelname FROM pg_stat_all_indexes WHERE schemaname = 'pg_catalog' AND relname = 'pg_class' ORDER BY 2;
SELECT relname, indexrelname FROM pg_stat_sys_indexes WHERE relname = 'pg_namespace' ORDER BY 2;
SELECT count(*) > 0 FROM pg_stat_sys_indexes WHERE schemaname = 'pg_toast';
DROP MATERIALIZED VIEW a146_st_mv;
DROP TABLE a146_st, a146_st_noix, a146_st_part;
