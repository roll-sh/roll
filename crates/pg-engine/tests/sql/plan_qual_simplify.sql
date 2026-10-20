-- Qual preprocessing the plan shapes depend on: AND / OR with constants simplified,
-- X = X as X IS NOT NULL, columns equated with a constant compared with it directly
-- (generate_base_implied_equalities_const), a sublink in a left join's ON clause that
-- reads only the right side joined there, and an EXISTS proven empty.
create table qs_a (id int primary key, u int unique, v int, w int);
create table qs_b (id int primary key, a_id int, t int);
insert into qs_a select g, 1000 - g, g % 10, g % 7 from generate_series(1, 500) g;
insert into qs_b select g, g % 400, g % 9 from generate_series(1, 800) g;
create index on qs_b (a_id);
analyze qs_a; analyze qs_b;
explain (costs off) select * from qs_a where id = id and v = v;
explain (costs off) select * from qs_a where id = id or v = 1;
explain (costs off) select * from qs_a where id = u and u = 42;
explain (costs off) select * from qs_a where v = 3 and id = v;
explain (costs off) select * from qs_a a join qs_b b on a.id = b.a_id and false;
explain (costs off) select * from qs_a a left join qs_b b on a.id = b.a_id and false;
explain (costs off) select * from qs_a a left join qs_b b on a.id = b.a_id and (b.t = 1 or true);
explain (costs off) select count(*) from qs_a a left join qs_b b on a.id = b.a_id and exists (select 1 from qs_a c where c.u = b.t);
explain (costs off) select * from qs_a where false and v = 1;
select count(*) from qs_a where id = id and v = v;
select * from qs_a where id = u and u = 42;
select count(*) from qs_a a left join qs_b b on a.id = b.a_id and exists (select 1 from qs_a c where c.u = b.t + 995);
select count(*), count(b.id) from qs_a a left join qs_b b on a.id = b.a_id and not exists (select 1 from qs_a c where c.id = b.t);
