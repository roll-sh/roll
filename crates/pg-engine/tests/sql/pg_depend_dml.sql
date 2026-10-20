-- Superuser DML on pg_depend: dependency checks see the rows as they are.
CREATE TABLE pd_t (f1 text, f2 text, f3 text, f4 text);
INSERT INTO pd_t VALUES ('foo', 'bar', 'baz', '42');
ALTER TABLE pd_t DROP COLUMN f2;
CREATE FUNCTION pd_f() RETURNS SETOF pd_t LANGUAGE plpgsql AS $$
DECLARE rec1 record;
BEGIN
  FOR rec1 IN SELECT * FROM pd_t LOOP
    RETURN NEXT rec1;
  END LOOP;
END $$;
-- A view reading a function's composite result depends on the columns it reads.
CREATE VIEW pd_v AS SELECT t.* FROM pd_f() t;
SELECT pg_describe_object(classid, objid, objsubid) AS obj,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS ref, deptype
  FROM pg_depend WHERE refobjid = 'pd_t'::regclass AND classid = 'pg_rewrite'::regclass
  ORDER BY 1, 2;
ALTER TABLE pd_t DROP COLUMN f3;
ALTER TABLE pd_t ALTER COLUMN f4 TYPE integer USING f4::integer;
BEGIN;
DELETE FROM pg_depend WHERE
  objid = (SELECT oid FROM pg_rewrite WHERE ev_class = 'pd_v'::regclass AND rulename = '_RETURN')
  AND refobjsubid = 4
RETURNING pg_describe_object(classid, objid, objsubid) AS obj,
          pg_describe_object(refclassid, refobjid, refobjsubid) AS ref, deptype;
ALTER TABLE pd_t ALTER COLUMN f4 TYPE integer USING f4::integer;
SELECT count(*) FROM pg_depend WHERE refobjid = 'pd_t'::regclass AND refobjsubid = 4;
ROLLBACK;
ALTER TABLE pd_t ALTER COLUMN f4 TYPE integer USING f4::integer;
-- Rows put back by INSERT count again.
BEGIN;
CREATE TEMP TABLE pd_saved AS SELECT * FROM pg_depend WHERE
  objid = (SELECT oid FROM pg_rewrite WHERE ev_class = 'pd_v'::regclass AND rulename = '_RETURN')
  AND refobjsubid = 3;
DELETE FROM pg_depend WHERE
  objid = (SELECT oid FROM pg_rewrite WHERE ev_class = 'pd_v'::regclass AND rulename = '_RETURN')
  AND refobjsubid = 3;
INSERT INTO pg_depend SELECT * FROM pd_saved;
ALTER TABLE pd_t DROP COLUMN f3;
ROLLBACK;
-- Only the columns the view reads.
DROP VIEW pd_v;
CREATE VIEW pd_v AS SELECT t.f1, t.f4 FROM pd_f() t;
ALTER TABLE pd_t DROP COLUMN f3;
SELECT * FROM pd_v;
-- A function's result follows its table's row type.
ALTER TABLE pd_t ADD COLUMN f5 int DEFAULT 5;
SELECT * FROM pd_f();
-- Non-superusers can't.
CREATE ROLE regress_pd_user;
SET ROLE regress_pd_user;
DELETE FROM pg_depend WHERE false;
RESET ROLE;
DROP ROLE regress_pd_user;
-- Whole-row references depend on the table, not its columns.
CREATE TABLE pd_w (a int, b text, c int);
INSERT INTO pd_w VALUES (1, 'x', 2);
CREATE VIEW pd_wv1 AS SELECT t.*::text AS x FROM pd_w t;
CREATE VIEW pd_wv2 AS SELECT t AS x FROM pd_w t;
CREATE VIEW pd_wv3 AS SELECT t.a, t AS x FROM pd_w t;
CREATE VIEW pd_wv5 AS SELECT row_to_json(t) AS j FROM pd_w t;
CREATE VIEW pd_wv6 AS SELECT a FROM pd_w t WHERE t IS NOT NULL;
SELECT pg_describe_object(classid, objid, objsubid) AS obj,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS ref, deptype
  FROM pg_depend WHERE refobjid = 'pd_w'::regclass AND classid = 'pg_rewrite'::regclass ORDER BY 1, 2;
ALTER TABLE pd_w ALTER COLUMN c TYPE bigint;
ALTER TABLE pd_w ALTER COLUMN a TYPE bigint;
ALTER TABLE pd_w DROP COLUMN c;
SELECT * FROM pd_wv1;
SELECT * FROM pd_wv5;
DROP TABLE pd_w;
CREATE FUNCTION pd_wf() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM pd_w t WHERE t IS NOT NULL; END;
ALTER TABLE pd_w ALTER COLUMN b TYPE varchar(10);
SELECT pg_describe_object(classid, objid, objsubid) AS obj,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS ref, deptype
  FROM pg_depend WHERE refobjid = 'pd_w'::regclass AND classid = 'pg_proc'::regclass ORDER BY 1, 2;
