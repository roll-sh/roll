-- A multi-insert COPY on the general insert path (a unique index, an AFTER ROW trigger, a
-- partitioned table): a row failing its defaults or constraints fails before the buffer is
-- flushed, so the buffer's earlier rows are never inserted or counted in n_tup_ins. A unique
-- violation is found at the flush, after heap_multi_insert counted the buffer's rows.
CREATE TABLE a182_u (id int NOT NULL, a int CHECK (a >= 0));
CREATE UNIQUE INDEX a182_u_id ON a182_u (id);
CREATE TABLE a182_t (id int NOT NULL, a int CHECK (a >= 0));
CREATE FUNCTION a182_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER a182_t_after AFTER INSERT ON a182_t FOR EACH ROW EXECUTE FUNCTION a182_noop();
CREATE TABLE a182_p (id int NOT NULL, a int CHECK (a >= 0)) PARTITION BY RANGE (id);
CREATE TABLE a182_p1 PARTITION OF a182_p FOR VALUES FROM (0) TO (100);
CREATE TABLE a182_p2 PARTITION OF a182_p FOR VALUES FROM (100) TO (200);
CREATE TABLE a182_b (id int NOT NULL, a int CHECK (a >= 0));
CREATE FUNCTION a182_keep() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER a182_b_before BEFORE INSERT ON a182_b FOR EACH ROW EXECUTE FUNCTION a182_keep();
BEGIN;
COPY a182_u FROM STDIN;
1	1
2	2
\.
SAVEPOINT s;
-- A CHECK failure on the third row: none of the three goes in.
COPY a182_u FROM STDIN;
3	3
4	4
5	-1
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_u';
-- A NOT NULL failure.
COPY a182_u FROM STDIN;
6	6
\N	7
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_u';
-- A bad value.
COPY a182_u FROM STDIN;
8	8
x	9
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_u';
-- A unique violation: the flush inserted the buffer's rows first.
COPY a182_u FROM STDIN;
10	10
1	11
12	12
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_u';
-- An AFTER ROW trigger's table.
COPY a182_t FROM STDIN;
1	1
2	-2
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_t';
COPY a182_t FROM STDIN;
1	1
2	2
\.
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_t';
SAVEPOINT s2;
-- Partitions: rows in both, then one no partition takes, or one failing its CHECK.
COPY a182_p FROM STDIN;
1	1
150	2
300	3
\.
ROLLBACK TO s2;
SELECT relname, n_tup_ins FROM pg_stat_xact_user_tables WHERE relname LIKE 'a182_p%' ORDER BY 1;
COPY a182_p FROM STDIN;
2	1
151	2
3	-3
\.
ROLLBACK TO s2;
SELECT relname, n_tup_ins FROM pg_stat_xact_user_tables WHERE relname LIKE 'a182_p%' ORDER BY 1;
COPY a182_p FROM STDIN;
4	1
152	2
\.
SELECT relname, n_tup_ins FROM pg_stat_xact_user_tables WHERE relname LIKE 'a182_p%' ORDER BY 1;
SAVEPOINT s3;
-- A BEFORE ROW trigger makes COPY insert row by row: the rows before the failure went in.
COPY a182_b FROM STDIN;
1	1
2	2
3	-3
\.
ROLLBACK TO s3;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a182_b';
COMMIT;
SELECT * FROM a182_u ORDER BY id;
SELECT * FROM a182_p ORDER BY id;
DROP TABLE a182_u, a182_t, a182_p, a182_b;
DROP FUNCTION a182_noop(), a182_keep();
