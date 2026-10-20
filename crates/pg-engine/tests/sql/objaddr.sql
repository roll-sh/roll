-- pg_identify_object / pg_identify_object_as_address / pg_get_object_address
CREATE SCHEMA oa;
CREATE TYPE oa.en AS ENUM ('x');
CREATE TYPE oa.co AS (a int, "B c" text);
CREATE DOMAIN oa.dm AS int CONSTRAINT dm_pos CHECK (VALUE > 0) NOT NULL;
CREATE TYPE oa.rng AS RANGE (subtype = int4);
CREATE SEQUENCE oa.sq;
CREATE TABLE oa.t (id serial PRIMARY KEY, v int DEFAULT 1 CHECK (v > 0), "Odd" text UNIQUE);
CREATE INDEX t_v ON oa.t (v);
CREATE VIEW oa.v AS SELECT id FROM oa.t;
CREATE MATERIALIZED VIEW oa.mv AS SELECT 1 AS one;
CREATE FUNCTION oa.f(int, text) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE PROCEDURE oa.p(a int) LANGUAGE sql AS '';
CREATE AGGREGATE oa.ag(int) (sfunc = int4pl, stype = int);
CREATE FUNCTION oa.tf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER trg BEFORE INSERT ON oa.t FOR EACH ROW EXECUTE FUNCTION oa.tf();
CREATE POLICY pol ON oa.t USING (true);
CREATE STATISTICS oa.st ON id, v FROM oa.t;
CREATE OPERATOR oa.### (LEFTARG = int, RIGHTARG = int, FUNCTION = int4pl);
CREATE TABLE oa.child (w int REFERENCES oa.t);
CREATE ROLE oa_role;
WITH objs(c, o, s) AS (VALUES
  ('pg_namespace'::regclass, 'oa'::regnamespace::oid, 0),
  ('pg_type'::regclass, 'oa.en'::regtype::oid, 0),
  ('pg_type'::regclass, 'oa.en[]'::regtype::oid, 0),
  ('pg_type'::regclass, 'oa.co'::regtype::oid, 0),
  ('pg_class'::regclass, 'oa.co'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.co'::regclass::oid, 2),
  ('pg_type'::regclass, 'oa.dm'::regtype::oid, 0),
  ('pg_type'::regclass, 'oa.rng'::regtype::oid, 0),
  ('pg_type'::regclass, 'int4'::regtype::oid, 0),
  ('pg_type'::regclass, 'int4[]'::regtype::oid, 0),
  ('pg_class'::regclass, 'oa.sq'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.t'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.t'::regclass::oid, 1),
  ('pg_class'::regclass, 'oa.t'::regclass::oid, 3),
  ('pg_class'::regclass, 'oa.t_v'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.t_pkey'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.v'::regclass::oid, 0),
  ('pg_class'::regclass, 'oa.mv'::regclass::oid, 0),
  ('pg_class'::regclass, 'pg_class'::regclass::oid, 0),
  ('pg_proc'::regclass, 'oa.f'::regproc::oid, 0),
  ('pg_proc'::regclass, 'oa.p'::regproc::oid, 0),
  ('pg_proc'::regclass, 'oa.ag'::regproc::oid, 0),
  ('pg_proc'::regclass, 'lower(text)'::regprocedure::oid, 0),
  ('pg_namespace'::regclass, 'public'::regnamespace::oid, 0),
  ('pg_namespace'::regclass, 'pg_catalog'::regnamespace::oid, 0),
  ('pg_authid'::regclass, 'oa_role'::regrole::oid, 0),
  ('pg_operator'::regclass, 'oa.###(int,int)'::regoperator::oid, 0),
  ('pg_language'::regclass, (SELECT oid FROM pg_language WHERE lanname = 'plpgsql'), 0),
  ('pg_extension'::regclass, (SELECT oid FROM pg_extension WHERE extname = 'plpgsql'), 0),
  ('pg_class'::regclass, 0, 0),
  ('pg_proc'::regclass, 0, 0))
SELECT (pg_identify_object(c, o, s)).*, (pg_identify_object_as_address(c, o, s)).* FROM objs;
SELECT (pg_identify_object(classid, objid, objsubid)).*, (pg_identify_object_as_address(classid, objid, objsubid)).*
  FROM pg_depend WHERE classid IN ('pg_constraint'::regclass, 'pg_attrdef'::regclass, 'pg_rewrite'::regclass, 'pg_trigger'::regclass, 'pg_policy'::regclass, 'pg_statistic_ext'::regclass)
  AND objid >= 16384 AND refobjid IN (SELECT oid FROM pg_class WHERE relnamespace = 'oa'::regnamespace UNION SELECT oid FROM pg_type WHERE typnamespace = 'oa'::regnamespace)
  AND (pg_identify_object(classid, objid, objsubid)).identity !~ 'RI_ConstraintTrigger' GROUP BY 1, 2, 3, 4, 5, 6, 7 ORDER BY 1, 4;
SELECT * FROM pg_identify_object('pg_cast'::regclass, (SELECT oid FROM pg_cast WHERE castsource = 'int4'::regtype AND casttarget = 'int8'::regtype), 0);
SELECT * FROM pg_identify_object_as_address('pg_cast'::regclass, (SELECT oid FROM pg_cast WHERE castsource = 'int4'::regtype AND casttarget = 'int8'::regtype), 0);
-- and back
SELECT pg_describe_object(a.classid, a.objid, a.objsubid) FROM (VALUES
  ('table', '{oa,t}'::text[], '{}'::text[]),
  ('table column', '{oa,t,v}', '{}'),
  ('index', '{oa,t_v}', '{}'),
  ('sequence', '{oa,sq}', '{}'),
  ('view', '{oa,v}', '{}'),
  ('materialized view', '{oa,mv}', '{}'),
  ('type', '{oa.en}', '{}'),
  ('type', '{int4}', '{}'),
  ('type', '{oa.dm}', '{}'),
  ('function', '{oa,f}', '{int4,text}'),
  ('procedure', '{oa,p}', '{int4}'),
  ('aggregate', '{oa,ag}', '{int4}'),
  ('schema', '{oa}', '{}'),
  ('role', '{oa_role}', '{}'),
  ('operator', '{oa,###}', '{int4,int4}'),
  ('table constraint', '{oa,t,t_pkey}', '{}'),
  ('domain constraint', '{oa.dm}', '{dm_pos}'),
  ('trigger', '{oa,t,trg}', '{}'),
  ('policy', '{oa,t,pol}', '{}'),
  ('default value', '{oa,t,v}', '{}'),
  ('rule', '{oa,v,_RETURN}', '{}'),
  ('statistics object', '{oa,st}', '{}'),
  ('language', '{plpgsql}', '{}'),
  ('extension', '{plpgsql}', '{}'),
  ('cast', '{int4}', '{int8}')) AS x(ty, n, ar), pg_get_object_address(x.ty, x.n, x.ar) a;
SELECT * FROM pg_get_object_address('composite type', '{oa,co}', '{}');
SELECT * FROM pg_get_object_address('table', '{oa,nope}', '{}');
SELECT * FROM pg_get_object_address('nonsense', '{oa,t}', '{}');
SELECT * FROM pg_get_object_address('function', '{oa,f}', '{int4}');
SELECT * FROM pg_get_object_address('table column', '{t}', '{}');
SELECT * FROM pg_get_object_address('schema', '{a,b}', '{}');
DROP SCHEMA oa CASCADE;
DROP ROLE oa_role;
