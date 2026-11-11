-- A view reads a FROM function's composite result columns by number and type, as
-- Postgres's stored query does: with the dependency that prevents it removed by hand, a
-- column dropped or retyped since fails when the view's rows are made.
create table vrd_users (userid text, seq int, email text, todrop bool, moredrop int, enabled bool);
insert into vrd_users values ('id', 1, 'email', true, 11, true);
insert into vrd_users values ('id2', 2, 'email2', true, 12, true);
alter table vrd_users drop column todrop;
create function vrd_get_users() returns setof vrd_users as
$$ select * from vrd_users order by userid; $$
language sql stable;
create view vrd_usersview as
select * from rows from(vrd_get_users(), generate_series(10, 11)) with ordinality;
select * from vrd_usersview;
alter table vrd_users add column junk text;
select * from vrd_usersview;
alter table vrd_users drop column moredrop;
begin;
delete from pg_depend where
  objid = (select oid from pg_rewrite
           where ev_class = 'vrd_usersview'::regclass and rulename = '_RETURN')
  and refobjsubid = 5
returning pg_describe_object(classid, objid, objsubid) as obj,
          pg_describe_object(refclassid, refobjid, refobjsubid) as ref,
          deptype;
alter table vrd_users drop column moredrop;
select * from vrd_usersview;
rollback;
alter table vrd_users alter column seq type numeric;
begin;
delete from pg_depend where
  objid = (select oid from pg_rewrite
           where ev_class = 'vrd_usersview'::regclass and rulename = '_RETURN')
  and refobjsubid = 2
returning pg_describe_object(classid, objid, objsubid) as obj,
          pg_describe_object(refclassid, refobjid, refobjsubid) as ref,
          deptype;
alter table vrd_users alter column seq type numeric;
select * from vrd_usersview;
rollback;
select * from vrd_usersview;
-- A plain table's column isn't read through a function's result: not checked this way.
create view vrd_direct as select userid, seq from vrd_users;
select * from vrd_direct;
