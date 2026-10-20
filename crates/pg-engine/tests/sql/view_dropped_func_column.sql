create table tt14t (f1 text, f2 text, f3 text, f4 text);
insert into tt14t values('foo', 'bar', 'baz', '42');
alter table tt14t drop column f2;
create function tt14f() returns setof tt14t as
$$
declare
    rec1 record;
begin
    for rec1 in select * from tt14t
    loop
        return next rec1;
    end loop;
end;
$$
language plpgsql;
create view tt14v as select t.* from tt14f() t;
select pg_get_viewdef('tt14v', true);
select pg_get_viewdef('tt14v', false);
begin;
delete from pg_depend where
  objid = (select oid from pg_rewrite
           where ev_class = 'tt14v'::regclass and rulename = '_RETURN')
  and refobjsubid = 3
returning pg_describe_object(classid, objid, objsubid) as obj,
          pg_describe_object(refclassid, refobjid, refobjsubid) as ref,
          deptype;
alter table tt14t drop column f3;
select pg_get_viewdef('tt14v', true);
select pg_get_viewdef('tt14v', false);
select f1, f4 from tt14v;
select * from tt14v;
rollback;
begin;
delete from pg_depend where
  objid = (select oid from pg_rewrite
           where ev_class = 'tt14v'::regclass and rulename = '_RETURN')
  and refobjsubid = 4
returning pg_describe_object(classid, objid, objsubid) as obj,
          pg_describe_object(refclassid, refobjid, refobjsubid) as ref,
          deptype;
alter table tt14t alter column f4 type integer using f4::integer;
select pg_get_viewdef('tt14v', true);
select f1, f3 from tt14v;
select * from tt14v;
rollback;
drop view tt14v;
create view tt14v as select t.f1, t.f4 from tt14f() t;
select pg_get_viewdef('tt14v', true);
alter table tt14t drop column f3;
select pg_get_viewdef('tt14v', true);
select * from tt14v;
create table vt (a int, b text, c int);
insert into vt values (1, 'one', 10);
create function vf() returns setof vt language sql as $$ select * from vt $$;
create view vv as select x.a, x.b, x.c from vf() x;
begin;
delete from pg_depend where objid = (select oid from pg_rewrite where ev_class = 'vv'::regclass and rulename = '_RETURN') and refobjsubid = 2;
alter table vt drop column b;
select pg_get_viewdef('vv', true);
select pg_get_viewdef('vv');
select a, c from vv;
select * from vv;
rollback;
