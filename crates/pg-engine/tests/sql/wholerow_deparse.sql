-- Whole-row references keep their form in stored definitions (Postgres keeps the Var):
-- `t.*`, record comparisons between them, IS [NOT] NULL on rows; and rules' multiple
-- assignments, ON CONFLICT filters; pg_get_functiondef's list-valued SET clauses.
create table wr_t (a int, b text);
insert into wr_t values (1, null), (2, 'x');
select t1.* = t2.*, t1.* <> t2.*, t1 = t2 from wr_t t1, wr_t t2 order by t1.a, t2.a;
create view wr_v1 as select t1.* <> t2.* as ne, t1 = t2 as eq, row_to_json(t1) as j,
  t1 as whole, (t1).a from wr_t t1, wr_t t2;
select pg_get_viewdef('wr_v1');
select pg_get_viewdef('wr_v1', true);
select * from wr_v1 order by j::text, ne;
create view wr_v2 as select * from wr_t t where t.* is not null and row(t.a, t.b) <> row(1, 'x');
select pg_get_viewdef('wr_v2');
create view wr_v3 as select s.x, s from (select 1 as x, 'y'::text as y) s;
create view wr_v4 as select (t.*)::text as w from wr_t t;
select pg_get_viewdef('wr_v4', true);
create view wr_v5 as select t, t.b::text from wr_t t;
select pg_get_viewdef('wr_v5', true);

create table wr_hat_data (hat_name char(10), hat_color char(10));
create unique index wr_hat_data_unique_idx on wr_hat_data (hat_name collate "C" bpchar_pattern_ops);
create table wr_hats (hat_name char(10), hat_color char(10));
create rule wr_hat_upsert as on insert to wr_hats do instead
  insert into wr_hat_data values (new.hat_name, new.hat_color)
  on conflict (hat_name collate "C" bpchar_pattern_ops) where hat_color = 'green'
  do update set hat_name = wr_hat_data.hat_name, hat_color = excluded.hat_color
  where excluded.hat_color <> 'forbidden' and wr_hat_data.* != excluded.*
  returning *;
select definition from pg_rules where tablename = 'wr_hats';

create table wr_rule_t1 (f1 int, f2 int);
create table wr_rule_dest (f1 int, f2 int[], tag text);
create rule wr_rr as on update to wr_rule_t1 do instead update wr_rule_dest trgt
  set (f2[1], f1, tag) = (select new.f2, new.f1, 'updated'::varchar)
  where trgt.f1 = new.f1 returning new.*;
select pg_get_ruledef(oid, true) from pg_rewrite where rulename = 'wr_rr';
create rule wr_rr2 as on delete to wr_rule_t1 do instead update wr_rule_dest
  set (f1, tag) = (old.f1, 'gone'), f2[2] = old.f2 where f1 = old.f1;
select pg_get_ruledef(oid) from pg_rewrite where rulename = 'wr_rr2';

create function wr_set_params() returns integer as 'select 1;' language sql
  set search_path to pg_catalog
  set extra_float_digits to 2
  set work_mem to '4MB'
  set datestyle to iso, mdy
  set local_preload_libraries to "Mixed/Case", 'c:/''a"/path', '', 'x123'
  immutable strict;
select pg_get_functiondef('wr_set_params()'::regprocedure);
select proconfig from pg_proc where proname = 'wr_set_params';

drop function wr_set_params();
drop table wr_rule_t1, wr_rule_dest;
drop table wr_hats, wr_hat_data;
drop view wr_v1, wr_v2, wr_v4, wr_v5;
drop table wr_t;
