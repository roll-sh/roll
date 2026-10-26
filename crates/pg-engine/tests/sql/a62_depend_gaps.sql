-- Dependencies DDL records (round 19, agent 62), recorded from PostgreSQL: field selections
-- and stores of composite values (a function's composite result, a column of a composite
-- type, a row constructor cast to one, a sub-select's), `(expr).*`, constraint triggers,
-- rules' actions apart from their conditions, policies' USING and WITH CHECK apart, user
-- casts' functions, collations of COLLATE clauses, relabelings and partition keys, arrays of
-- domains, and what DROP and ALTER TYPE ... DROP ATTRIBUTE refuse or cascade to.
CREATE SCHEMA pd;
SET search_path = pd, public;
CREATE TABLE p (id int PRIMARY KEY, note text, qty int);
CREATE FUNCTION getp(int) RETURNS p LANGUAGE sql AS 'SELECT * FROM p WHERE id = $1';
CREATE FUNCTION getps() RETURNS SETOF p LANGUAGE sql AS 'SELECT * FROM p';
CREATE VIEW v_field AS SELECT (getp(1)).note AS n;
CREATE VIEW v_field2 AS SELECT (getp(1)).* ;
CREATE VIEW v_field3 AS SELECT getp(1) AS whole;
CREATE VIEW v_srf AS SELECT note FROM getps();
CREATE VIEW v_srf2 AS SELECT (getps()).qty;
CREATE TYPE ct AS (a int, b text);
CREATE TABLE t2 (k int, c ct, ca ct[]);
CREATE VIEW v_cfield AS SELECT (c).b, (ca[1]).a FROM t2;
CREATE VIEW v_rowcast AS SELECT ROW(1,'x')::ct AS r, (ROW(2,'y')::ct).a AS ra;
CREATE VIEW v_sub AS SELECT ((SELECT c FROM t2 LIMIT 1)).a AS sa;
CREATE INDEX t2_ca ON t2 (((c).a));
ALTER TABLE t2 ADD CONSTRAINT t2_chk CHECK ((c).a > 0);
ALTER TABLE t2 ADD COLUMN g int GENERATED ALWAYS AS ((c).a * 2) STORED;
ALTER TABLE t2 ADD COLUMN d text DEFAULT (ROW(1,'q')::ct).b;
CREATE FUNCTION fb() RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT (getp(1)).note; END;
CREATE FUNCTION fs() RETURNS void LANGUAGE sql BEGIN ATOMIC UPDATE t2 SET c.a = 1; END;
CREATE TABLE logt (id int, c ct);
CREATE RULE r_ins AS ON INSERT TO t2 DO ALSO INSERT INTO logt (id, c.b) VALUES (NEW.k, (NEW.c).b);
CREATE STATISTICS st_f ON ((c).a), k FROM t2;
CREATE POLICY pol_f ON t2 USING ((c).a > 0);
CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER trg_f BEFORE UPDATE ON t2 FOR EACH ROW WHEN ((NEW.c).a > 0) EXECUTE FUNCTION trf();
CREATE TABLE pt (k int, c ct) PARTITION BY RANGE (((c).a));
SELECT pg_describe_object(classid,objid,objsubid) AS obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) AS ref, deptype
FROM pg_depend
WHERE (objid >= 16384 OR refobjid >= 16384)
  AND pg_describe_object(classid,objid,objsubid) !~ 'pg_toast'
  AND pg_describe_object(refclassid,refobjid,refobjsubid) !~ 'pg_toast'
  AND NOT (refclassid = 'pg_namespace'::regclass)
ORDER BY 1,2,3;
DROP TABLE p;
ALTER TABLE p DROP COLUMN note;
ALTER TABLE p DROP COLUMN qty;
ALTER TYPE ct DROP ATTRIBUTE b;
ALTER TYPE ct DROP ATTRIBUTE a;
ALTER TABLE p ALTER COLUMN note TYPE varchar;
RESET search_path;
CREATE SCHEMA pe;
SET search_path = pe, public;
CREATE TYPE color AS ENUM ('red', 'green');
CREATE TYPE ct AS (a int, b text);
CREATE DOMAIN dpos AS int CHECK (VALUE > 0);
CREATE DOMAIN dct AS ct;
CREATE COLLATION mycoll FROM "C";
CREATE SEQUENCE sq;
CREATE TABLE t (id int, c color, x text COLLATE mycoll, d dpos, arr dpos[], cc ct);
-- consts and casts of user types
CREATE VIEW v_const AS SELECT 'red'::color AS c, '{red}'::color[] AS ca, 1::dpos AS d, ARRAY[1]::dpos[] AS da;
CREATE VIEW v_cast AS SELECT id::dpos AS d, c::text AS ct, x::varchar AS xv, cc::dct AS dc FROM t;
CREATE VIEW v_null AS SELECT NULL::color AS n, NULL::ct AS nc;
CREATE VIEW v_coll AS SELECT x COLLATE mycoll AS y, 'q' COLLATE mycoll AS q, x FROM t ORDER BY x COLLATE mycoll;
CREATE VIEW v_coll2 AS SELECT id FROM t WHERE x < 'm' COLLATE mycoll;
-- operators
CREATE FUNCTION colorlt(color, color) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1::text < $2::text';
CREATE OPERATOR <<< (LEFTARG = color, RIGHTARG = color, FUNCTION = colorlt);
CREATE VIEW v_op AS SELECT id FROM t WHERE c <<< 'green';
CREATE VIEW v_rowcmp AS SELECT id FROM t WHERE (c, id) < ('green'::color, 1);
CREATE VIEW v_distinct AS SELECT DISTINCT c FROM t;
CREATE VIEW v_group AS SELECT c, count(*) FROM t GROUP BY c;
CREATE VIEW v_order AS SELECT c FROM t ORDER BY c;
CREATE VIEW v_case AS SELECT CASE c WHEN 'red' THEN 1 END AS k, NULLIF(c, 'red') AS nf, c IS DISTINCT FROM 'red' AS dd FROM t;
CREATE VIEW v_in AS SELECT id FROM t WHERE c IN ('red', 'green');
-- sequences
CREATE VIEW v_seq AS SELECT nextval('sq') AS n, currval('pe.sq'::regclass) AS c2, nextval('sq'::text) AS n3;
CREATE TABLE ts (a int DEFAULT nextval('sq'), b bigint GENERATED ALWAYS AS IDENTITY, c serial);
-- domains
CREATE FUNCTION okf(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
CREATE DOMAIN dchk AS int DEFAULT nextval('sq') CHECK (okf(VALUE)) ;
CREATE DOMAIN dcoll AS text COLLATE mycoll;
CREATE DOMAIN darr AS color[];
CREATE TABLE tdom (a dchk, b dcoll, c darr);
-- functions
CREATE FUNCTION f_args(a color, b ct, c dpos[], OUT d color, OUT e dct) LANGUAGE sql AS 'SELECT $1, $2::dct';
CREATE FUNCTION f_tab(color) RETURNS TABLE (x ct, y dpos) LANGUAGE sql AS 'SELECT NULL::ct, 1::dpos';
CREATE FUNCTION f_setof() RETURNS SETOF ct LANGUAGE sql AS 'SELECT NULL::ct';
CREATE FUNCTION f_poly(anyelement, color DEFAULT 'red') RETURNS anyelement LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION f_rowtype(t) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION f_rettype() RETURNS t LANGUAGE sql AS 'SELECT * FROM pe.t';
CREATE PROCEDURE p_inout(INOUT a color) LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION f_sup(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION f_variadic(VARIADIC color[]) RETURNS int LANGUAGE sql AS 'SELECT 1';
-- casts
CREATE FUNCTION color2int(color) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1';
CREATE CAST (color AS int) WITH FUNCTION color2int(color);
CREATE CAST (ct AS text) WITH INOUT;
CREATE VIEW v_ucast AS SELECT c::int AS ci, cc::text AS ctext FROM t;
-- partition keys
CREATE TABLE pk1 (a int, c color, x text) PARTITION BY RANGE (okf(a), x COLLATE mycoll);
CREATE TABLE pk2 (a int, c color) PARTITION BY LIST (c);
CREATE TABLE pk2a PARTITION OF pk2 FOR VALUES IN ('red');
CREATE TABLE pk3 (a int, x text COLLATE mycoll) PARTITION BY HASH (x);
-- policies
CREATE TABLE tp (id int, c color);
CREATE POLICY pol1 ON tp USING (okf(id) AND c = 'red'::color) WITH CHECK (id IN (SELECT id FROM t));
-- triggers
CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER trg1 BEFORE UPDATE OF c, id ON tp FOR EACH ROW WHEN (okf(NEW.id) AND NEW.c <<< OLD.c) EXECUTE FUNCTION trf();
CREATE CONSTRAINT TRIGGER trg2 AFTER INSERT ON tp FROM t FOR EACH ROW EXECUTE FUNCTION trf();
CREATE VIEW vtp AS SELECT * FROM tp;
CREATE TRIGGER trg3 INSTEAD OF INSERT ON vtp FOR EACH ROW EXECUTE FUNCTION trf();
CREATE TRIGGER trg4 AFTER INSERT ON tp REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trf();
-- publications
CREATE PUBLICATION pub1 FOR TABLE tp (id, c) WHERE (id > 0 AND id < abs(-5)), t;
CREATE PUBLICATION pub2 FOR TABLES IN SCHEMA pe;
-- statistics
CREATE STATISTICS st1 ON okf(id), (c::text), x FROM t;
CREATE STATISTICS st2 (ndistinct) ON id, c FROM t;
-- indexes with opclass/collation
CREATE INDEX ix1 ON t (x COLLATE mycoll);
CREATE INDEX ix2 ON t (okf(id)) WHERE c = 'red';
-- matview, rules
CREATE MATERIALIZED VIEW mv1 AS SELECT c::int AS ci, x COLLATE mycoll AS xc FROM t;
CREATE RULE r1 AS ON UPDATE TO tp WHERE NEW.c = 'red' DO INSTEAD NOTHING;
SELECT pg_describe_object(classid,objid,objsubid) AS obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) AS ref, deptype
FROM pg_depend
WHERE (objid >= 16384 OR refobjid >= 16384)
  AND pg_describe_object(classid,objid,objsubid) !~ 'pg_toast'
  AND pg_describe_object(refclassid,refobjid,refobjsubid) !~ 'pg_toast'
  AND NOT (refclassid = 'pg_namespace'::regclass)
  AND NOT (deptype = 'i' AND pg_describe_object(classid,objid,objsubid) ~ '^type .*(\[\]|_)$')
ORDER BY 1,2,3;
RESET search_path;
CREATE SCHEMA pf; SET search_path = pf;
CREATE TABLE t (a int, b int);
CREATE TABLE lg (x int);
CREATE RULE r1 AS ON INSERT TO t DO ALSO INSERT INTO lg VALUES (1);
CREATE RULE r2 AS ON UPDATE TO t WHERE NEW.a > 0 DO ALSO INSERT INTO lg VALUES (2);
CREATE RULE r3 AS ON DELETE TO t DO INSTEAD NOTHING;
CREATE RULE r4 AS ON UPDATE TO t DO ALSO (INSERT INTO lg VALUES (OLD.b); INSERT INTO lg VALUES (3));
CREATE RULE r5 AS ON INSERT TO t WHERE NEW.b > 0 DO ALSO NOTIFY t_chan;
CREATE RULE r6 AS ON INSERT TO t DO ALSO SELECT 1;
CREATE RULE r7 AS ON INSERT TO t WHERE NEW.a = 1 DO INSTEAD NOTHING;
SELECT pg_describe_object(classid,objid,objsubid) AS obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) AS ref, deptype
FROM pg_depend WHERE classid = 'pg_rewrite'::regclass AND objid >= 16384 ORDER BY 1,2,3;
CREATE SCHEMA pg2; SET search_path = pg2;
CREATE COLLATION mycoll2 FROM "C";
CREATE TABLE t (id int, x text COLLATE mycoll2, y varchar COLLATE mycoll2);
CREATE VIEW c1 AS SELECT x FROM t;
CREATE VIEW c2 AS SELECT x::varchar AS v FROM t;
CREATE VIEW c3 AS SELECT x || 'a' AS v FROM t;
CREATE VIEW c4 AS SELECT id FROM t WHERE x = 'a';
CREATE VIEW c5 AS SELECT upper(x) AS u FROM t;
CREATE VIEW c6 AS SELECT y::text AS yt, x::name AS xn FROM t;
CREATE VIEW c7 AS SELECT 'a'::text COLLATE mycoll2 AS k;
CREATE VIEW c8 AS SELECT id FROM t ORDER BY x;
CREATE VIEW c9 AS SELECT max(x) AS m FROM t;
CREATE VIEW c10 AS SELECT (SELECT x FROM t LIMIT 1) AS s;
CREATE VIEW c11 AS SELECT x FROM t UNION SELECT 'z';
CREATE TABLE t2 (k int, x text DEFAULT 'a' COLLATE mycoll2 CHECK (x > 'a' COLLATE mycoll2));
CREATE INDEX t2i ON t2 ((x COLLATE mycoll2));
CREATE FUNCTION fd(a text DEFAULT 'q' COLLATE mycoll2) RETURNS text LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION fb() RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT 'q' COLLATE mycoll2; END;
SELECT pg_describe_object(classid,objid,objsubid) AS obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) AS ref, deptype
FROM pg_depend WHERE refclassid = 'pg_collation'::regclass AND refobjid >= 16384 ORDER BY 1,2,3;
CREATE SCHEMA ph; SET search_path = ph;
CREATE TYPE color AS ENUM ('red', 'green');
CREATE TYPE ct AS (a int, b text);
CREATE COLLATION mycoll3 FROM "C";
CREATE SEQUENCE sq3;
CREATE DOMAIN dpos AS int CHECK (VALUE > 0);
CREATE FUNCTION okf(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
CREATE FUNCTION colorlt(color, color) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1::text < $2::text';
CREATE OPERATOR <<< (LEFTARG = color, RIGHTARG = color, FUNCTION = colorlt);
CREATE FUNCTION color2int(color) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1';
CREATE CAST (color AS int) WITH FUNCTION color2int(color);
CREATE TABLE t (id int, c color, x text COLLATE mycoll3, d dpos, cc ct);
CREATE VIEW v1 AS SELECT c::int AS ci FROM t;
CREATE VIEW v2 AS SELECT id FROM t WHERE x < 'm' COLLATE mycoll3;
CREATE VIEW v3 AS SELECT (cc).a FROM t;
CREATE VIEW v4 AS SELECT nextval('sq3') AS n;
CREATE VIEW v5 AS SELECT id FROM t WHERE c <<< 'green';
CREATE TABLE pk1 (a int, x text) PARTITION BY RANGE (okf(a), x COLLATE mycoll3);
CREATE TABLE tp (id int, c color);
CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE CONSTRAINT TRIGGER trg2 AFTER INSERT ON tp FROM t FOR EACH ROW EXECUTE FUNCTION trf();
CREATE RULE r3 AS ON DELETE TO tp DO INSTEAD NOTHING;
BEGIN; DROP FUNCTION color2int(color); ROLLBACK;
BEGIN; DROP CAST (color AS int); ROLLBACK;
BEGIN; DROP COLLATION mycoll3; ROLLBACK;
BEGIN; DROP SEQUENCE sq3; ROLLBACK;
BEGIN; DROP OPERATOR <<< (color, color); ROLLBACK;
BEGIN; DROP FUNCTION okf(int); ROLLBACK;
BEGIN; DROP TYPE ct; ROLLBACK;
BEGIN; ALTER TYPE ct DROP ATTRIBUTE a; ROLLBACK;
BEGIN; ALTER TYPE ct DROP ATTRIBUTE b; ROLLBACK;
BEGIN; ALTER TYPE ct DROP ATTRIBUTE a CASCADE; SELECT count(*) FROM pg_views WHERE viewname = 'v3'; ROLLBACK;
BEGIN; DROP TABLE t; ROLLBACK;
BEGIN; DROP TABLE t CASCADE; SELECT tgname FROM pg_trigger WHERE tgname = 'trg2'; ROLLBACK;
BEGIN; DROP TABLE tp; ROLLBACK;
BEGIN; DROP COLLATION mycoll3 CASCADE; SELECT relname FROM pg_class WHERE relname IN ('pk1','v2','t') ORDER BY 1; ROLLBACK;
CREATE SCHEMA pi; SET search_path = pi;
CREATE TABLE t (id int); CREATE TABLE tp (id int);
CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE CONSTRAINT TRIGGER trg2 AFTER INSERT ON tp FROM t DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION trf();
SELECT conname, contype, condeferrable, condeferred, conrelid::regclass FROM pg_constraint WHERE conname = 'trg2';
SELECT tgname, tgconstrrelid::regclass, tgconstraint <> 0 AS hascon, tgdeferrable, tginitdeferred FROM pg_trigger WHERE tgname = 'trg2';
DROP TABLE t;
