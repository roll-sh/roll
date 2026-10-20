-- Deparsing stored definitions: named-notation calls, policies with subqueries, CHECK and
-- index expressions after ALTER COLUMN TYPE, rules writing subscripted / field targets,
-- MERGE in a BEGIN ATOMIC body, whole-row references in ROW() / VALUES, FROM-clause
-- expressions, and the names USING columns get.
create function sdm_dfunc(a anyelement, b anyelement = null, flag bool = true)
  returns anyelement as $$ select case when $3 then $1 else $2 end; $$ language sql;
create table sdm_i8 (q1 int8, q2 int8);
insert into sdm_i8 values (1, 2), (5, 3);
create view sdm_dfview as select q1, q2,
  sdm_dfunc(q1, q2, flag := q1 > q2) as c3,
  sdm_dfunc(q1, flag := q1 < q2, b := q2) as c4
  from sdm_i8;
select pg_get_viewdef('sdm_dfview', true);
select pg_get_viewdef('sdm_dfview');
select * from sdm_dfview order by q1;

create table sdm_u (id int, t_id int, n text);
create table sdm_t (id int, a int);
alter table sdm_t enable row level security;
create policy p1 on sdm_t using (a > 0 and exists (select 1 from sdm_u where sdm_u.t_id = sdm_t.id));
create policy p2 on sdm_t for insert with check (a in (select t_id from sdm_u u where u.n = 'x'));
create policy p3 on sdm_t using (id = (select max(id) from sdm_u));
select policyname, qual, with_check from pg_policies where tablename = 'sdm_t' order by 1;

create table sdm_c (a numeric, b float8 check (b > 1), c int check (c > 0 and c < 100), d text);
create index sdm_ci on sdm_c ((b + 1)) where b > 2;
alter table sdm_c alter column b type numeric;
alter table sdm_c alter column c type bigint;
select conname, pg_get_constraintdef(oid) from pg_constraint where conrelid = 'sdm_c'::regclass order by 1;
select pg_get_indexdef('sdm_ci'::regclass);

create type sdm_ct as (if1 int, if2 text[]);
create table sdm_ins (f1 int, f2 int[], f3 sdm_ct, f4 sdm_ct[]);
create table sdm_ins2 (f1 bigint, f2 text);
create rule sdm_irule1 as on insert to sdm_ins2 do
  insert into sdm_ins (f3.if2[1], f3.if2[2]) values (new.f1, new.f2);
create rule sdm_irule2 as on insert to sdm_ins2 do
  insert into sdm_ins (f4[1].if1, f4[1].if2[2]) values (1, 'fool'), (new.f1, new.f2);
create rule sdm_irule3 as on insert to sdm_ins2 do
  insert into sdm_ins (f4[1].if1, f4[1].if2[2]) select new.f1, new.f2;
select rulename, pg_get_ruledef(oid, true) from pg_rewrite where rulename like 'sdm_irule%' order by 1;

create table sdm_target (id int, data text, filling int[]);
create table sdm_source (a int, b text);
create function sdm_merge() returns table (action text, a int, b text, id int, data text,
    filling int[], old_id int, old_data text, old_filling int[], new_id int, new_data text,
    new_filling int[])
  language sql
begin atomic
  merge into sdm_target t using sdm_source s on (s.a = t.id)
  when matched and (s.a + t.id) = 42 then update set data = repeat(t.data, s.a) || s.b, id = length(s.b)
  when not matched and (s.b is not null) then insert (data, id) values (s.b, s.a)
  when matched and s.a > 200 then update set filling[s.a] = t.id
  when matched and s.a > 100 then delete
  when matched then do nothing
  when not matched and s.a > 200 then insert default values
  when not matched and s.a > 0 then insert values (s.a, s.b, default)
  when not matched then insert (filling[1], id) values (s.a, s.a)
  returning with (old as o, new as n) merge_action() as action, *, o.*, n.*;
end;
select pg_get_functiondef('sdm_merge'::regproc);
create function sdm_merge2() returns void language sql
begin atomic
  merge into sdm_target t using sdm_source s on (s.a = t.id)
  when not matched then insert (data, id) values (s.a, s.a)
  when matched then update set data = s.b
  when not matched by source then delete;
end;
select pg_get_functiondef('sdm_merge2'::regproc);

create type sdm_nc as (x sdm_i8);
create view sdm_v15 as select row(i)::sdm_nc from sdm_i8 i;
select pg_get_viewdef('sdm_v15', true);
create view sdm_v16 as select * from sdm_i8 i, lateral (values(i)) ss;
select pg_get_viewdef('sdm_v16', true);
create view sdm_v17 as select * from sdm_i8 i where i in (values(i));
select pg_get_viewdef('sdm_v17', true);
create table sdm_log (o sdm_v15, n sdm_v15, incr bool);
create rule sdm_updlog as on update to sdm_v15 do also
  insert into sdm_log values (old, new, row(old, old) < row(new, new));
select pg_get_ruledef(oid, true) from pg_rewrite where rulename = 'sdm_updlog';

create view sdm_v20 as select * from coalesce(1, 2) as c, collation for ('x'::text) col,
  current_date as d, localtimestamp(3) as t, cast(1 + 2 as int4) as i4, cast(1 + 2 as int8) as i8;
select pg_get_viewdef('sdm_v20', true);
create view sdm_v21 as select (select * from current_date) as cd2, (select * from current_user) as cu2;
select pg_get_viewdef('sdm_v21', true);

create table sdm_tt7 (x int, xx int, y int);
alter table sdm_tt7 drop column xx;
create table sdm_tt8 (x int, z int);
create view sdm_vv3 as select * from (values (1, 2, 3, 4, 5, 6)) v(a, b, c, x, e, f)
  union all select * from sdm_tt7 full join sdm_tt8 using (x),
  sdm_tt8 tt8x full join sdm_tt7 tt7x using (x);
select pg_get_viewdef('sdm_vv3', true);
create table sdm_tt7a (x date, y int);
create table sdm_tt8a (x timestamptz, z int);
create view sdm_vv2a as select * from (values (now(), 2, 3, now(), 5)) v(a, b, c, d, e)
  union all select * from sdm_tt7a left join sdm_tt8a using (x), sdm_tt8a tt8ax;
select pg_get_viewdef('sdm_vv2a', true);
alter table sdm_tt7 add column z int;
alter table sdm_tt8 add column z2 int;
select pg_get_viewdef('sdm_vv3', true);
create table sdm_tt11 (x int, y int);
create table sdm_tt12 (x int, z int);
create table sdm_tt13 (z int, q int);
create view sdm_vv6 as select x, y, z, q from (sdm_tt11 join sdm_tt12 using (x)) join sdm_tt13 using (z);
alter table sdm_tt11 add column z int;
select pg_get_viewdef('sdm_vv6', true);
create table sdm_tt5 (a int, b int);
create table sdm_tt6 (c int, d int);
create view sdm_vv1 as select * from (sdm_tt5 cross join sdm_tt6) j(aa, bb, cc, dd);
alter table sdm_tt5 add column c int;
select pg_get_viewdef('sdm_vv1', true);
alter table sdm_tt5 add column cc int;
select pg_get_viewdef('sdm_vv1', true);
alter table sdm_tt5 drop column c;
select pg_get_viewdef('sdm_vv1', true);

drop rule sdm_updlog on sdm_v15;
drop view sdm_vv1, sdm_vv6, sdm_vv3, sdm_vv2a, sdm_v20, sdm_v21, sdm_v16, sdm_v17, sdm_dfview, sdm_v15;
drop table sdm_log, sdm_tt5, sdm_tt6, sdm_tt11, sdm_tt12, sdm_tt13, sdm_tt7, sdm_tt8, sdm_tt7a, sdm_tt8a;
drop type sdm_nc;
drop function sdm_merge(), sdm_merge2();
drop table sdm_target, sdm_source, sdm_ins, sdm_ins2, sdm_c, sdm_t, sdm_u, sdm_i8;
drop type sdm_ct;
drop function sdm_dfunc(anyelement, anyelement, bool);
