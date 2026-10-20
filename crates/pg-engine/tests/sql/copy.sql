CREATE TABLE cp (id int PRIMARY KEY, name text, score numeric(5,2), tags text[], born date, note text DEFAULT 'none');
-- text format
COPY cp (id, name, score, tags, born) FROM STDIN;
1	alice	9.5	{a,b}	2001-02-03
2	bob	\N	{}	\N
3	tab\there	1.25	{"x y"}	1999-12-31
4	back\\slash	0	{NULL}	2020-01-01
\.
SELECT * FROM cp ORDER BY id;
COPY cp TO STDOUT;
COPY cp (id, name) TO STDOUT;
COPY (SELECT id, name FROM cp WHERE id > 2 ORDER BY id) TO STDOUT;
COPY cp TO STDOUT WITH (DELIMITER '|', NULL 'NULL');
-- CSV
COPY cp TO STDOUT WITH (FORMAT csv, HEADER);
COPY cp (id, name) TO STDOUT WITH (FORMAT csv, FORCE_QUOTE *);
COPY cp (id, name, note) TO STDOUT WITH (FORMAT csv, FORCE_QUOTE (name), QUOTE '''', DELIMITER ';');
TRUNCATE cp;
COPY cp (id, name, score) FROM STDIN WITH (FORMAT csv, HEADER);
id,name,score
10,"quoted, comma",1.5
11,"say ""hi""",
12,,2
\.
SELECT id, name, score, name IS NULL AS null_name FROM cp ORDER BY id;
COPY cp (id, name) FROM STDIN WITH (FORMAT csv, FORCE_NOT_NULL (name));
13,
\.
COPY cp (id, name) FROM STDIN WITH (FORMAT csv, FORCE_NULL (name));
14,""
\.
COPY cp (id, name) FROM STDIN WITH (FORMAT csv, NULL 'nil');
15,nil
16,"nil"
\.
SELECT id, name IS NULL AS null_name, name FROM cp WHERE id >= 13 ORDER BY id;
COPY cp (id, name) FROM STDIN WITH (FORMAT csv, HEADER MATCH);
id,name
17,matched
\.
COPY cp (id, name) FROM STDIN WITH (FORMAT csv, HEADER MATCH);
id,wrong
18,x
\.
-- DEFAULT marker
COPY cp (id, name, note) FROM STDIN WITH (DEFAULT '\D');
20	d	\D
21	e	explicit
\.
SELECT id, note FROM cp WHERE id >= 20 ORDER BY id;
-- errors
COPY cp (id, name) FROM STDIN;
30	a	extra
\.
COPY cp (id, name) FROM STDIN;
31
\.
COPY cp (id, score) FROM STDIN;
32	notanumber
\.
COPY cp (id, name) FROM STDIN;
1	duplicate
\.
COPY cp (nope) FROM STDIN;
\.
COPY cp_missing FROM STDIN;
\.
COPY cp FROM STDIN WITH (FORMAT xml);
\.
COPY cp FROM STDIN WITH (DELIMITER 'ab');
\.
COPY cp TO STDOUT WITH (FORMAT csv, FORCE_QUOTE (id), FORCE_QUOTE (name));
COPY cp TO STDOUT WITH (FORCE_QUOTE (id));
COPY cp FROM STDIN WITH (HEADER);
\.
SELECT count(*) FROM cp;
-- ON_ERROR ignore
COPY cp (id, score) FROM STDIN WITH (ON_ERROR ignore);
40	1
41	bad
42	3
\.
SELECT id, score FROM cp WHERE id >= 40 ORDER BY id;
-- views and generated columns
CREATE VIEW cpv AS SELECT id FROM cp;
COPY cpv TO STDOUT;
COPY cpv FROM STDIN;
\.
CREATE TABLE cpg (a int, b int GENERATED ALWAYS AS (a * 2) STORED);
COPY cpg (a) FROM STDIN;
1
2
\.
COPY cpg (a, b) FROM STDIN;
3	4
\.
COPY cpg TO STDOUT;
-- triggers fire for COPY FROM
CREATE TABLE cpt (a int, b text);
CREATE FUNCTION cpt_trig() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.b := upper(NEW.b);
  RAISE NOTICE 'row %', NEW.a;
  RETURN NEW;
END $$;
CREATE TRIGGER cpt_t BEFORE INSERT ON cpt FOR EACH ROW EXECUTE FUNCTION cpt_trig();
COPY cpt FROM STDIN;
1	x
2	y
\.
SELECT * FROM cpt ORDER BY a;
-- in a transaction
BEGIN;
COPY cpt FROM STDIN;
3	z
\.
ROLLBACK;
SELECT count(*) FROM cpt;
DROP TABLE cp, cpg, cpt CASCADE;
DROP FUNCTION cpt_trig();
-- errors name the input line
CREATE TABLE cpe (a int PRIMARY KEY, b text CHECK (b <> 'bad'));
COPY cpe FROM STDIN;
1	x
1	y
\.
COPY cpe FROM STDIN;
1	x
2	bad
\.
COPY cpe (b) FROM STDIN;
z
\.
CREATE FUNCTION cpe_trig() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER cpe_t BEFORE INSERT ON cpe FOR EACH ROW EXECUTE FUNCTION cpe_trig();
COPY cpe FROM STDIN;
1	x
1	y
\.
CREATE FUNCTION cpe_after() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'after %', NEW.a; RETURN NULL; END $$;
CREATE TRIGGER cpe_a AFTER INSERT ON cpe FOR EACH ROW EXECUTE FUNCTION cpe_after();
COPY cpe FROM STDIN;
5	x
6	y
\.
DROP TABLE cpe;
DROP FUNCTION cpe_trig();
DROP FUNCTION cpe_after();
