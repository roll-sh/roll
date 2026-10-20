-- COPY FROM rules, AFTER-trigger WHEN timing, fast defaults and rewrites
CREATE TABLE gb_cx (a int, b int, c text DEFAULT 'x', d text);
COPY gb_cx FROM stdin WHERE a = max(b);
\.
COPY gb_cx FROM stdin WHERE a IN (SELECT 1);
\.
COPY gb_cx FROM stdin WHERE a IN (generate_series(1, 5));
\.
COPY gb_cx FROM stdin WHERE a = row_number() OVER (b);
\.
COPY gb_cx (a) FROM stdin WITH (default '\D');
\D
\.
COPY gb_cx FROM stdin;

\.
COPY gb_cx FROM stdin FREEZE;
1	2	3	4
\.
BEGIN;
TRUNCATE gb_cx;
SAVEPOINT s1;
COPY gb_cx FROM stdin FREEZE;
1	2	3	4
\.
ROLLBACK;
CREATE TABLE gb_eoc (a text);
INSERT INTO gb_eoc VALUES ('\.'), ('\.b'), ('a\.');
COPY gb_eoc TO stdout CSV;
CREATE TABLE gb_eoc2 (a text, b int);
INSERT INTO gb_eoc2 VALUES ('\.', 1);
COPY gb_eoc2 TO stdout CSV;
CREATE DOMAIN gb_dnn varchar(15) NOT NULL;
CREATE TABLE gb_dt (n int, l gb_dnn);
COPY gb_dt FROM stdin WITH (on_error ignore, log_verbosity verbose);
1	a
2	\N
3	c
\.
SELECT * FROM gb_dt;
CREATE TABLE gb_vt (id serial, name text);
CREATE VIEW gb_vv AS SELECT ''::text AS str;
COPY gb_vv FROM stdin;
\.
CREATE FUNCTION gb_vv_ins() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN INSERT INTO gb_vt (name) VALUES (NEW.str); RETURN NULL; END $$;
CREATE TRIGGER gb_vv_t INSTEAD OF INSERT ON gb_vv FOR EACH ROW EXECUTE FUNCTION gb_vv_ins();
COPY gb_vv FROM stdin;
one
two
\.
SELECT * FROM gb_vt;
CREATE FUNCTION gb_bark(text) RETURNS bool LANGUAGE plpgsql IMMUTABLE AS
  $$ BEGIN RAISE NOTICE '% <- woof!', $1; RETURN true; END $$;
CREATE FUNCTION gb_tn() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN RAISE NOTICE 'trigger % on % for %', TG_NAME, TG_TABLE_NAME, NEW.a; RETURN NULL; END $$;
CREATE TABLE gb_tw (a int, b text);
CREATE TRIGGER gb_tw_t AFTER INSERT ON gb_tw FOR EACH ROW WHEN (gb_bark(new.b) AND new.a % 2 = 1)
  EXECUTE FUNCTION gb_tn();
INSERT INTO gb_tw VALUES (1, 'aardvark'), (2, 'aanimals'), (3, 'ant');
CREATE FUNCTION gb_rw() RETURNS event_trigger LANGUAGE plpgsql AS
  $$ BEGIN RAISE NOTICE 'rewriting % for reason %', pg_event_trigger_table_rewrite_oid()::regclass,
       pg_event_trigger_table_rewrite_reason(); END $$;
CREATE EVENT TRIGGER gb_rw_et ON table_rewrite EXECUTE FUNCTION gb_rw();
CREATE TABLE gb_fd (a int, b varchar(10));
INSERT INTO gb_fd VALUES (1, 'x');
ALTER TABLE gb_fd ADD COLUMN c int DEFAULT 5;
ALTER TABLE gb_fd ADD COLUMN v int GENERATED ALWAYS AS (a + 1) VIRTUAL;
ALTER TABLE gb_fd ALTER COLUMN v TYPE float8;
ALTER TABLE gb_fd ALTER COLUMN b TYPE varchar(20) USING b::varchar(20);
SELECT attname, atthasmissing FROM pg_attribute WHERE attrelid = 'gb_fd'::regclass AND attnum > 0 ORDER BY attnum;
ALTER TABLE gb_fd ADD COLUMN t timestamptz DEFAULT clock_timestamp();
SELECT attname, atthasmissing FROM pg_attribute WHERE attrelid = 'gb_fd'::regclass AND attnum > 0 ORDER BY attnum;
SELECT a, b, c, v FROM gb_fd;
DROP EVENT TRIGGER gb_rw_et;
DROP FUNCTION gb_rw();
SET search_path = pg_temp, public;
CREATE DOMAIN pg_temp.gb_nonempty AS text CHECK (value <> '');
SELECT gb_nonempty('');
SELECT pg_temp.gb_nonempty('x');
RESET search_path;
DROP TABLE gb_fd;
DROP TABLE gb_tw;
DROP FUNCTION gb_tn();
DROP FUNCTION gb_bark(text);
DROP VIEW gb_vv;
DROP FUNCTION gb_vv_ins();
DROP TABLE gb_vt;
DROP TABLE gb_dt;
DROP DOMAIN gb_dnn;
DROP TABLE gb_eoc2;
DROP TABLE gb_eoc;
DROP TABLE gb_cx;
