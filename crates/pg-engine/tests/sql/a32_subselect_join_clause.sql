-- A join clause over a correlated sub-select: cost_qual_eval charges its SubPlan's runs in
-- the join's clause costs (a hash join's per probe, cost_hashjoin), and the clause isn't
-- volatile (contain_volatile_functions looks at a SubPlan's arguments), so a Memoize over
-- the parameterized inner side caches by the sub-select's value
-- (paraminfo_get_equal_hashops).
create table sjc_a (a int, b int);
create table sjc_b (a int, b int);
insert into sjc_a select g, g % 5 from generate_series(1, 30) g;
insert into sjc_b select g, g % 3 from generate_series(1, 30) g;
create index on sjc_b (a);
analyze sjc_a, sjc_b;
explain
select sjc_b.a from sjc_a join sjc_b on sjc_b.a = (select min(a) from sjc_b x where x.b = sjc_a.b);
select sjc_b.a from sjc_a join sjc_b on sjc_b.a = (select min(a) from sjc_b x where x.b = sjc_a.b) order by 1;
set enable_hashjoin = off;
explain
select sjc_b.a from sjc_a join sjc_b on sjc_b.a = (select min(a) from sjc_b x where x.b = sjc_a.b);
reset enable_hashjoin;
set enable_memoize = off;
explain (costs off)
select sjc_b.a from sjc_a join sjc_b on sjc_b.a = (select min(a) from sjc_b x where x.b = sjc_a.b);
select sjc_b.a from sjc_a join sjc_b on sjc_b.a = (select min(a) from sjc_b x where x.b = sjc_a.b) order by 1;
reset enable_memoize;
explain (costs off)
select ss.x, sjc_b.a from (select (select min(a) from sjc_b where sjc_b.b = sjc_a.b) as x from sjc_a where sjc_a.a < 8) ss
join sjc_b on sjc_b.a = ss.x;
drop table sjc_a;
drop table sjc_b;
