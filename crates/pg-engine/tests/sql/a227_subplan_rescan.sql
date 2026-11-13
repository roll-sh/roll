-- Agent 227: correlated SubPlans whose tree is kept through the statement and rescanned for
-- each outer row (an Aggregate, a projection over it, an index scan with the outer row's
-- values as its keys): every outer row's result is its own, also when a run stops at its
-- first row (EXISTS, ANY), finds no row, or the same SubPlan runs inside another.
create table a227_sp (k int, s int, q int, d date);
create index a227_sp_ks on a227_sp (k, s);
insert into a227_sp select g % 50, g % 7, g, date '2024-01-01' + (g % 90) from generate_series(1, 4000) g;
create table a227_so (k int, s int, lim int);
insert into a227_so select g % 60, g % 9, g * 3 from generate_series(1, 300) g;
analyze a227_sp, a227_so;
set enable_hashjoin = off;
set enable_mergejoin = off;
set enable_bitmapscan = off;
set enable_seqscan = off;
set max_parallel_workers_per_gather = 0;
explain (costs off)
select o.k, o.s, (select 0.5 * sum(p.q) from a227_sp p where p.k = o.k and p.s = o.s and p.d >= date '2024-02-01') from a227_so o;
select o.k, o.s, (select 0.5 * sum(p.q) from a227_sp p where p.k = o.k and p.s = o.s and p.d >= date '2024-02-01') as half
  from a227_so o order by o.k, o.s, half limit 40;
select count(*), sum(c) from (select (select count(*) from a227_sp p where p.k = o.k and p.s = o.s) as c from a227_so o) t;
select o.k, o.s, o.lim from a227_so o
  where o.lim > (select max(p.q) from a227_sp p where p.k = o.k and p.s = o.s) order by 1, 2, 3 limit 20;
select count(*) from a227_so o where exists (select 1 from a227_sp p where p.k = o.k and p.s = o.s and p.q > o.lim);
select count(*) from a227_so o where o.lim > all (select p.q from a227_sp p where p.k = o.k and p.s = o.s);
select o.k, (select min(p.q) + (select max(p2.q) from a227_sp p2 where p2.k = o.k and p2.s = p.s)
               from a227_sp p where p.k = o.k and p.s = o.s)
  from a227_so o order by 1, 2 limit 15;
reset enable_hashjoin;
reset enable_mergejoin;
reset enable_bitmapscan;
reset enable_seqscan;
reset max_parallel_workers_per_gather;
