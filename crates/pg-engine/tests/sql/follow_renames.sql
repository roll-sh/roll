-- Stored definitions follow renames and moves of what they name: views (and the column
-- aliases a USING join needs), rules, triggers, policies, constraints, defaults, generated
-- columns, indexes, statistics, partition keys, domains and SQL-standard function bodies.
create schema fr_a;
create schema fr_b;
set search_path = fr_a, public;
create table t (id int primary key, a int, b text);
create table u (id int, a int, c text);
insert into t values (1, 10, 'one'), (2, 20, 'two');
insert into u values (1, 10, 'uno'), (2, 21, 'dos');
create view vj as select t.id, u.c from t join u using (id, a);
create view vn as select * from t natural join u;
create view vx as select x.id, y.c from t x join u y on x.id = y.id;
create view vs as select t.a, s.a as sa from t, (select a from u) s where t.id = s.a;
create view vstar as select * from (select * from t) ss;
create view vcte as with w as (select id, a from t) select w.a from w where w.id > 0;
create view vq as select fr_a.t.a, t.b from fr_a.t;
create view vg as select a, count(*) from t group by a order by a;
select pg_get_viewdef('vj', true);
select pg_get_viewdef('vn', true);
select pg_get_viewdef('vstar', true);

-- columns
alter table t rename column a to aa;
alter table u rename column a to ua;
select pg_get_viewdef('vj', true);
select pg_get_viewdef('vn', true);
select pg_get_viewdef('vx', true);
select pg_get_viewdef('vs', true);
select pg_get_viewdef('vstar', true);
select pg_get_viewdef('vcte', true);
select pg_get_viewdef('vq', true);
select pg_get_viewdef('vg', true);
select * from vj order by id;
select * from vn order by id;

-- relations, and schemas
alter table t rename to tt;
alter table tt set schema fr_b;
select pg_get_viewdef('vj', true);
select pg_get_viewdef('vn', true);
select pg_get_viewdef('vs', true);
select pg_get_viewdef('vstar', true);
select pg_get_viewdef('vq', true);
select * from vstar order by id;
alter table fr_b.tt add column z int;
select * from vstar order by id;
alter table u add column b int;
select pg_get_viewdef('vn', true);
select * from vn order by id;
alter schema fr_b rename to fr_c;
select pg_get_viewdef('vq', true);
select * from vq order by a;

-- a renamed relation can't capture references meant for another
create table tx1 (x1 int, x2 int);
create table tt1 (f1 int, f2 int);
create view av1 as select * from tt1 where exists (select 1 from tx1 where tt1.f1 = tx1.x1);
create view av2 as select * from tt1 a1 where exists (select 1 from tx1 where a1.f1 = tx1.x1);
alter table tx1 rename to a1;
select pg_get_viewdef('av1', true);
select pg_get_viewdef('av2', true);
alter table tt1 rename to a2;
select pg_get_viewdef('av1', true);
select pg_get_viewdef('av2', true);
alter table a1 rename to tt1;
select pg_get_viewdef('av2', true);
insert into tt1 values (1, 2);
insert into a2 values (1, 3), (5, 6);
select * from av1;
select * from av2;
reset search_path;
drop schema fr_a cascade;
drop schema fr_c cascade;

-- what else stores expressions
create schema fr_d;
set search_path = fr_d, public;
create function f(int) returns int language sql immutable return $1 * 2;
create type mood as enum ('happy', 'sad');
create sequence s1;
create table t (id int, a int check (t.a > 0 and f(a) < 100),
  g int generated always as (f(a) + 1) stored, d text default 'x'::text);
create index ti on t (f(a)) where id > 0;
create statistics ts on (f(a)), id from t;
create table p (k int, v int) partition by range (f(k));
create table log (op text, ids int);
create rule r as on insert to t do also insert into log (op, ids) values ('ins', new.id);
create function tf() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger tg before update on t for each row when (new.a <> old.a) execute function tf();
alter table t enable row level security;
create policy pol on t using (a > 0 and f(id) > 0);
create domain dpos as int check (f(value) > 0);
create function body() returns setof int language sql
  begin atomic insert into log (op, ids) select 'x', a from t; select f(a) from t; end;
create view v as select 't'::regclass as rc, f(a) as fa, 'happy'::mood as m,
  nextval('s1') as nv, t.* from t;
alter table t rename column a to aa;
alter function f(int) rename to ff;
alter type mood rename to feeling;
alter sequence s1 rename to s2;
select pg_get_indexdef('ti'::regclass);
select pg_get_statisticsobjdef(oid) from pg_statistic_ext where stxname = 'ts';
select pg_get_partkeydef('p'::regclass);
select pg_get_ruledef(oid) from pg_rewrite where rulename = 'r';
select pg_get_triggerdef(oid) from pg_trigger where tgname = 'tg';
select qual from pg_policies where policyname = 'pol';
select pg_get_constraintdef(oid) from pg_constraint where conname = 't_a_check';
select pg_get_constraintdef(oid) from pg_constraint where contypid = 'dpos'::regtype;
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 't'::regclass order by adnum;
select pg_get_functiondef('body'::regproc);
select pg_get_viewdef('v');
alter table log rename column ids to idz;
select pg_get_ruledef(oid) from pg_rewrite where rulename = 'r';
select pg_get_functiondef('body'::regproc);
alter table t rename to t2;
select pg_get_viewdef('v');
insert into t2 (id, aa) values (1, 5);
select * from log order by op;
select rc, fa, m, id, a, g, d from v;
select * from body();
alter schema fr_d rename to fr_e;
reset search_path;
select pg_get_viewdef('fr_e.v');
select pg_get_functiondef('fr_e.body'::regproc);
drop schema fr_e cascade;

-- view columns, and what a type change can't follow
create schema fr_f;
set search_path = fr_f, public;
create table t (id int, a int, b text, c int, d int);
create view v1 as select id, a as fa from t;
create view v2 as select v1.id, v1.fa from v1 where v1.id > 0;
create table log (x int);
create rule r as on update to t do also insert into log values (old.b::int);
create function tf() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger tg before update of c on t for each row execute function tf();
create policy pol on t using (d > 0);
create function sb() returns int language sql begin atomic select a from t where id = 1; end;
alter view v1 rename column fa to fb;
select pg_get_viewdef('v2', true);
alter table t alter column a type bigint;
alter table t alter column b type varchar;
alter table t alter column c type bigint;
alter table t alter column d type bigint;
drop function sb();
drop view v2, v1;
alter table t alter column a type bigint;
reset search_path;
drop schema fr_f cascade;
