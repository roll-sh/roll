-- SubPlan numbers in the order Postgres makes them: a level's own expressions' sub-selects
-- (subquery_planner's preprocessing), then its FROM subqueries planned apart
-- (query_planner's set_subquery_pathlist), then its MIN / MAX InitPlans (made at createplan
-- time by create_minmaxagg_plan). A MIN / MAX InitPlan's quals read the level's InitPlans,
-- attached to the level's top; a HAVING over MIN / MAX aggregates reads their Params, the
-- Result's One-Time Filter.
create table spn_a (a int, b int);
create table spn_b (a int, b int);
insert into spn_a select g, g % 5 from generate_series(1, 30) g;
insert into spn_b select g, g % 3 from generate_series(1, 30) g;
create index on spn_b (a);
analyze spn_a, spn_b;
explain (costs off)
select (select max(b) from spn_b where spn_b.a = ss.a) from
  (select a, (select min(b) from spn_b where spn_b.a = spn_a.b) m from spn_a offset 0) ss
where ss.m > (select min(a) from spn_b x where x.b = ss.a);
select (select max(b) from spn_b where spn_b.a = ss.a) from
  (select a, (select min(b) from spn_b where spn_b.a = spn_a.b) m from spn_a offset 0) ss
where ss.m > (select min(a) from spn_b x where x.b = ss.a);
explain (costs off)
select x.c, (select max(b) from spn_b) from (select a, (select count(*) from spn_b where b = spn_a.b) c from spn_a offset 0) x;
create index on spn_a (a);
explain (costs off)
select max(a), (select count(*) from spn_b where b = 1) from spn_a;
explain (costs off)
select (select count(*) from spn_b where b = 1), max(a) from spn_a;
explain (costs off)
select max(a) from spn_a having max(a) > (select count(*) from spn_b where b = 1);
select max(a) from spn_a having max(a) > (select count(*) from spn_b where b = 1);
select max(a) from spn_a having max(a) > (select count(*) + 100 from spn_b where b = 1);
explain (costs off)
select max(a) from spn_a where b > (select count(*) from spn_b where b = 1);
select max(a) from spn_a where b > (select count(*) from spn_b where b = 1);
drop table spn_a;
drop table spn_b;
