-- A plain COPY (no triggers, WHERE, DEFAULT markers or unique indexes) parses its lines straight
-- into values and inserts them without the general per-row path: its errors, contexts, defaults
-- and pg_stat counts are as PostgreSQL's.
CREATE TABLE a175_p (id int NOT NULL, a int CHECK (a >= 0), b text DEFAULT 'dflt', c float8);
CREATE INDEX a175_p_a ON a175_p (a);
COPY a175_p (id, a, c) FROM STDIN;
1	10	0.1
2	20	-1.5e-7
3	\N	1e300
\.
COPY a175_p FROM STDIN;
4	40	x	2.5
5	\N	\N	\N
\.
SELECT * FROM a175_p ORDER BY id;
-- NOT NULL and CHECK violations name the line and its text.
COPY a175_p FROM STDIN;
6	60	y	1
\N	70	z	2
\.
COPY a175_p FROM STDIN;
8	80	y	1
9	-1	z	2
\.
-- A bad value and a missing column.
COPY a175_p FROM STDIN;
10	abc	y	1
\.
COPY a175_p FROM STDIN;
11	1
\.
SELECT count(*) FROM a175_p;
-- Its rows' pg_stat counts are in before the statement ends.
BEGIN;
COPY a175_p FROM STDIN;
12	1	a	1
13	1	b	2
14	5	c	3
\.
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a175_p';
SAVEPOINT s;
COPY a175_p FROM STDIN;
15	1	a	1
16	1	b	2
\N	5	c	3
\.
ROLLBACK TO s;
SELECT n_tup_ins FROM pg_stat_xact_user_tables WHERE relname = 'a175_p';
ROLLBACK;
-- COPY TO with a delimiter numbers' text can hold escapes it there.
COPY (SELECT -1 AS i, -2.5::float8 AS f, 1e-7::float8 AS e, 'x-y' AS t) TO STDOUT (DELIMITER '-');
COPY (SELECT 1.5::float8, 'NaN'::float8, '-Infinity'::float8, 0.1::float8, 123456789012345678::float8) TO STDOUT (DELIMITER 'N');
COPY (SELECT 1.5::float8, 3::int) TO STDOUT (DELIMITER '+');
COPY a175_p TO STDOUT;
DROP TABLE a175_p;
