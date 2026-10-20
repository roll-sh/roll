-- reg* constants (`'t'::regclass`, …) make what they're in depend on the object they name:
-- views, defaults, constraints, index expressions, policies, trigger conditions and
-- functions with SQL bodies.
create schema drc_s;
create table drc_t (a int);
create table drc_u (a int);
create function drc_fn(int) returns int language sql as 'select 1';
create type drc_ty as (x int);
create view drc_v as select 'drc_t'::regclass as r, a from drc_u;
create view drc_v2 as select a from drc_u where 'drc_fn'::regproc is not null and 'drc_fn(int)'::regprocedure is not null;
create view drc_v3 as select 'drc_u'::regclass::oid as o, 'drc_t'::regtype as t, 'drc_ty'::regtype as ty, 'drc_s'::regnamespace as n, regclass 'drc_t' as r2;
create view drc_v4 as select 'drc_t'::text::regclass as r, 'pg_class'::regclass as c, 'int4'::regtype as i;
create table drc_d (a regclass default 'drc_t'::regclass, b int check (b > 'drc_t'::regclass::oid::int), c int);
create index drc_d_ix on drc_d ((c + ('drc_u'::regclass::oid)::int));
create policy drc_p on drc_d using ('drc_u'::regclass is not null);
create function drc_tf() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger drc_tr before update on drc_d for each row when ('drc_u'::regclass is not null) execute function drc_tf();
create function drc_sb() returns regclass return 'drc_t'::regclass;
create function drc_sb2() returns int begin atomic select 1 where 'drc_fn'::regproc is not null; end;
create function drc_def(r regclass default 'drc_u'::regclass) returns int language sql as 'select 1';
select pg_describe_object(classid, objid, objsubid) as obj, pg_describe_object(refclassid, refobjid, refobjsubid) as ref, deptype from pg_depend where refobjid in ('drc_t'::regclass, 'drc_u'::regclass, 'drc_fn'::regproc, 'drc_t'::regtype, 'drc_ty'::regtype, 'drc_s'::regnamespace) and classid <> 'pg_type'::regclass order by 1, 2;
drop table drc_t;
drop function drc_fn(int);
drop schema drc_s;
drop type drc_ty;
drop table drc_u;
begin;
drop table drc_u cascade;
select relname from pg_class where relname like 'drc\_%' order by 1;
rollback;
drop table drc_t cascade;
drop function drc_fn(int) cascade;
drop schema drc_s cascade;
drop type drc_ty cascade;
select relname from pg_class where relname like 'drc\_%' order by 1;
select proname from pg_proc where proname like 'drc\_%' order by 1;
drop table drc_u cascade;
drop function drc_tf, drc_def;
