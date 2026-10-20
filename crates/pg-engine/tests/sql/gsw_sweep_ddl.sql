-- DDL fixed in the regression sweep: dependencies of a made access method, opclasses
-- binary-coercible to their input, TRUNCATE ... RESTART IDENTITY / CASCADE, DROP OWNED
-- and others' user mappings, information_schema.check_constraints, PL/pgSQL texts.
DROP OWNED BY gsw_r1 CASCADE;
DROP OWNED BY gsw_r2 CASCADE;
DROP ROLE IF EXISTS gsw_r1;
DROP ROLE IF EXISTS gsw_r2;
CREATE ACCESS METHOD gsw_gist2 TYPE INDEX HANDLER gisthandler;
CREATE TABLE gsw_fe (home_base box);
CREATE OPERATOR CLASS gsw_box_ops DEFAULT FOR TYPE box USING gsw_gist2 AS
  OPERATOR 1 <<, OPERATOR 2 &<, OPERATOR 3 &&, OPERATOR 4 &>, OPERATOR 5 >>, OPERATOR 6 ~=,
  OPERATOR 7 @>, OPERATOR 8 <@, OPERATOR 9 &<|, OPERATOR 10 <<|, OPERATOR 11 |>>, OPERATOR 12 |&>,
  FUNCTION 1 gist_box_consistent(internal, box, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal);
CREATE INDEX gsw_fe_g2 ON gsw_fe USING gsw_gist2 (home_base);
DROP ACCESS METHOD gsw_gist2;
DROP ACCESS METHOD gsw_gist2 CASCADE;
SELECT count(*) FROM pg_opclass WHERE opcname = 'gsw_box_ops';
-- text is binary-coercible to bpchar
CREATE TABLE gsw_bb (f1 text COLLATE "C");
CREATE INDEX ON gsw_bb (f1 bpchar_ops);
INSERT INTO gsw_bb VALUES ('foo'), ('fool'), ('bar'), ('quux');
SELECT * FROM gsw_bb WHERE f1 LIKE 'foo%' ORDER BY 1;
-- TRUNCATE ... RESTART IDENTITY restarts sequences made the table's with OWNED BY
CREATE SEQUENCE gsw_ta_id1 START WITH 33;
CREATE TABLE gsw_ta (id serial, id1 integer DEFAULT nextval('gsw_ta_id1'));
ALTER SEQUENCE gsw_ta_id1 OWNED BY gsw_ta.id1;
INSERT INTO gsw_ta DEFAULT VALUES;
INSERT INTO gsw_ta DEFAULT VALUES;
TRUNCATE gsw_ta;
INSERT INTO gsw_ta DEFAULT VALUES;
SELECT * FROM gsw_ta;
TRUNCATE gsw_ta RESTART IDENTITY;
INSERT INTO gsw_ta DEFAULT VALUES;
INSERT INTO gsw_ta DEFAULT VALUES;
SELECT * FROM gsw_ta;
-- TRUNCATE ... CASCADE names a referencing partitioned table's partitions too
CREATE TABLE gsw_tp (a int PRIMARY KEY);
CREATE TABLE gsw_tpart (a int REFERENCES gsw_tp) PARTITION BY RANGE (a);
CREATE TABLE gsw_tpart_1 PARTITION OF gsw_tpart FOR VALUES FROM (0) TO (100);
CREATE TABLE gsw_tpart_2 PARTITION OF gsw_tpart FOR VALUES FROM (100) TO (200) PARTITION BY RANGE (a);
CREATE TABLE gsw_tpart_2_1 PARTITION OF gsw_tpart_2 FOR VALUES FROM (100) TO (150);
CREATE TABLE gsw_tpart_2_d PARTITION OF gsw_tpart_2 DEFAULT;
TRUNCATE TABLE gsw_tp;
TRUNCATE TABLE gsw_tp CASCADE;
-- DROP OWNED: another role's user mapping on their server depends on it
CREATE ROLE gsw_r1;
CREATE ROLE gsw_r2;
CREATE FOREIGN DATA WRAPPER gsw_fdw;
CREATE SERVER gsw_s5 FOREIGN DATA WRAPPER gsw_fdw;
ALTER SERVER gsw_s5 OWNER TO gsw_r1;
CREATE USER MAPPING FOR gsw_r1 SERVER gsw_s5;
REASSIGN OWNED BY gsw_r1 TO gsw_r2;
DROP OWNED BY gsw_r2;
DROP OWNED BY gsw_r2 CASCADE;
SELECT count(*) FROM pg_foreign_server WHERE srvname = 'gsw_s5';
DROP FOREIGN DATA WRAPPER gsw_fdw;
DROP ROLE gsw_r1;
DROP ROLE gsw_r2;
-- information_schema.check_constraints: the constraint's expression as pg_get_expr prints it
CREATE TABLE gsw_jc (js text, i int,
  CONSTRAINT gsw_jc1 CHECK (js IS JSON),
  CONSTRAINT gsw_jc2 CHECK (i > 0 AND i < 100),
  CONSTRAINT gsw_jc3 CHECK (JSON_VALUE(js::jsonb, '$.a' RETURNING int DEFAULT '12' ON EMPTY ERROR ON ERROR) > i),
  CONSTRAINT gsw_jc4 CHECK (js > 'a' COLLATE "C"));
SELECT constraint_name, check_clause FROM information_schema.check_constraints
  WHERE constraint_name LIKE 'gsw_jc%' ORDER BY 1;
SELECT conname, pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname LIKE 'gsw_jc%' ORDER BY 1;
CREATE VIEW gsw_cvv AS SELECT * FROM gsw_jc WHERE js < ('x' COLLATE "C") AND (js COLLATE "C") > 'b';
SELECT pg_get_viewdef('gsw_cvv', true);
SELECT pg_get_viewdef('gsw_cvv', false);
-- PL/pgSQL: a bound cursor's arguments as one expression; a statement's text trimmed
CREATE FUNCTION gsw_c7() RETURNS void AS $$
DECLARE
  c1 CURSOR (p1 int, p2 int) FOR SELECT * FROM pg_class WHERE oid = p1 AND relpages = p2;
BEGIN
  OPEN c1 (p2 := 77, p1 := 42/0);
END $$ LANGUAGE plpgsql;
SELECT gsw_c7();
CREATE TABLE gsw_rt (f1 int, f4 int);
CREATE FUNCTION gsw_rtf() RETURNS int LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  UPDATE gsw_rt SET f1 = 1
    RETURNING nosuch INTO x;  -- trailing
  RETURN x;
END $$;
SELECT gsw_rtf();
-- a materialized view given a new access method is rewritten (table_rewrite)
CREATE ACCESS METHOD gsw_heap2 TYPE TABLE HANDLER heap_tableam_handler;
CREATE FUNCTION gsw_rw() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'rewriting % (reason = %)', pg_event_trigger_table_rewrite_oid()::regclass, pg_event_trigger_table_rewrite_reason();
END $$;
CREATE EVENT TRIGGER gsw_rw ON table_rewrite EXECUTE FUNCTION gsw_rw();
CREATE MATERIALIZED VIEW gsw_mv USING heap AS SELECT 1 AS a;
ALTER MATERIALIZED VIEW gsw_mv SET ACCESS METHOD gsw_heap2;
ALTER MATERIALIZED VIEW gsw_mv SET ACCESS METHOD gsw_heap2;
DROP EVENT TRIGGER gsw_rw;
