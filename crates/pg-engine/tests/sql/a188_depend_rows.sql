-- pg_depend read per object (a correlated sub-select or a LATERAL over pg_depend's
-- `classid = c AND objid = o`): one object's rows at a time, as the whole catalog has them.
CREATE SCHEMA a188_dr;
CREATE DOMAIN a188_dr.posint AS int CHECK (VALUE > 0);
CREATE TYPE a188_dr.pair AS (a int, b text);
CREATE FUNCTION a188_dr.f(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
CREATE TABLE a188_dr.p (id serial PRIMARY KEY, code text UNIQUE, n a188_dr.posint DEFAULT a188_dr.f(1),
  pr a188_dr.pair, g int GENERATED ALWAYS AS (id * 2) STORED, CHECK (id > 0));
CREATE TABLE a188_dr.c (id int GENERATED ALWAYS AS IDENTITY, pid int REFERENCES a188_dr.p ON DELETE CASCADE,
  note text DEFAULT 'x');
CREATE INDEX a188_dr_c_pid ON a188_dr.c (pid) WHERE pid > 0;
CREATE INDEX a188_dr_c_expr ON a188_dr.c (a188_dr.f(pid));
CREATE VIEW a188_dr.v AS SELECT p.id, c.note FROM a188_dr.p p JOIN a188_dr.c c ON c.pid = p.id;
CREATE MATERIALIZED VIEW a188_dr.mv AS SELECT id, code FROM a188_dr.p;
CREATE SEQUENCE a188_dr.s OWNED BY a188_dr.c.note;
CREATE TABLE a188_dr.parted (k int, v text) PARTITION BY RANGE (k);
CREATE TABLE a188_dr.parted_1 PARTITION OF a188_dr.parted FOR VALUES FROM (0) TO (10);
CREATE TABLE a188_dr.kid (extra int) INHERITS (a188_dr.c);
CREATE FUNCTION a188_dr.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER a188_dr_t BEFORE INSERT ON a188_dr.c FOR EACH ROW EXECUTE FUNCTION a188_dr.trg();
CREATE RULE a188_dr_r AS ON UPDATE TO a188_dr.v DO INSTEAD NOTHING;
ALTER TABLE a188_dr.p ENABLE ROW LEVEL SECURITY;
CREATE POLICY a188_dr_pol ON a188_dr.p USING (a188_dr.f(id) > 0);
CREATE STATISTICS a188_dr.st ON id, code FROM a188_dr.p;
-- Relations: EF Core's NOT EXISTS with an InitPlan, then each relation's rows.
SELECT c.relname, c.relkind FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND d.deptype IN ('e', 'x'))
  AND c.relnamespace = 'a188_dr'::regnamespace
ORDER BY 1;
SELECT c.relname, pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS ref, d.objsubid, d.deptype
FROM pg_class c, LATERAL (SELECT * FROM pg_depend d
                          WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid) d
WHERE c.relnamespace = 'a188_dr'::regnamespace AND c.relname < 'm'
ORDER BY 1, 2, 3, 4;
SELECT c.relname, pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS ref, d.objsubid, d.deptype
FROM pg_class c, LATERAL (SELECT * FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid) d
WHERE c.relnamespace = 'a188_dr'::regnamespace AND c.relname >= 'm'
ORDER BY 1, 2, 3, 4;
-- Other kinds of objects, by their class (an outer value).
SELECT pg_describe_object(d.classid, d.objid, d.objsubid) AS obj,
       pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS ref, d.deptype
FROM (SELECT 'pg_constraint'::regclass::oid AS cls, oid FROM pg_constraint WHERE connamespace = 'a188_dr'::regnamespace
      UNION ALL SELECT 'pg_proc'::regclass, oid FROM pg_proc WHERE pronamespace = 'a188_dr'::regnamespace
      UNION ALL SELECT 'pg_type'::regclass, oid FROM pg_type WHERE typnamespace = 'a188_dr'::regnamespace AND typtype IN ('d', 'c')) o,
     LATERAL (SELECT * FROM pg_depend d WHERE d.classid = o.cls AND d.objid = o.oid) d
ORDER BY 1, 2, 3;
SELECT regexp_replace(pg_describe_object(d.classid, d.objid, d.objsubid), '(RI_ConstraintTrigger_[ac]_)[0-9]+', '\1N') AS obj,
       pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS ref, d.deptype
FROM (SELECT 'pg_trigger'::regclass::oid AS cls, t.oid FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
        WHERE c.relnamespace = 'a188_dr'::regnamespace
      UNION ALL SELECT 'pg_rewrite'::regclass, r.oid FROM pg_rewrite r JOIN pg_class c ON c.oid = r.ev_class
        WHERE c.relnamespace = 'a188_dr'::regnamespace
      UNION ALL SELECT 'pg_policy'::regclass, oid FROM pg_policy
      UNION ALL SELECT 'pg_attrdef'::regclass, a.oid FROM pg_attrdef a JOIN pg_class c ON c.oid = a.adrelid
        WHERE c.relnamespace = 'a188_dr'::regnamespace
      UNION ALL SELECT 'pg_statistic_ext'::regclass, oid FROM pg_statistic_ext WHERE stxnamespace = 'a188_dr'::regnamespace) o,
     LATERAL (SELECT * FROM pg_depend d WHERE d.classid = o.cls AND d.objid = o.oid) d
ORDER BY 1, 2, 3;
-- A NULL class or object: no row.
SELECT count(*) FROM pg_class c
WHERE c.relnamespace = 'a188_dr'::regnamespace
  AND EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = (SELECT NULL::oid) AND d.objid = c.oid);
-- Right after DDL (a catalog version no statement has read).
CREATE TABLE a188_dr.late (id int PRIMARY KEY REFERENCES a188_dr.p, s serial);
SELECT pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS ref, d.objsubid, d.deptype
FROM pg_depend d
WHERE d.objid = (SELECT 'a188_dr.late'::regclass::oid) AND d.classid = (SELECT 'pg_class'::regclass::oid)
ORDER BY 1, 2, 3;
DROP SCHEMA a188_dr CASCADE;
