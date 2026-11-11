-- pullup_replace_vars puts a copy of a pulled-up subquery's output in each place reading
-- it: a sub-select output is one SubPlan per place (make_subplan for each copy), run there.
-- SubPlans are numbered in subquery_planner's order (the target list first), each copy of
-- an AlternativeSubPlan picks plain or hashed by how often it runs there
-- (fix_alternative_subplan: an index-only scan's index condition once, its recheck and a
-- join's quals per row), and EXPLAIN prints a SubPlan where the executor first
-- initializes it (an Append's run-time pruning expressions before its members; an
-- index-only scan's index condition not at all without ANALYZE).
create table spc_i (f1 int4);
insert into spc_i values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze spc_i;
create table spc_p (id int primary key) partition by range (id);
create table spc_p0 partition of spc_p for values from (0) to (1);
create table spc_p1 partition of spc_p for values from (1) to (2);
insert into spc_p values (0), (1);
explain (costs off)
select * from
  (select exists (select 1 from spc_i tinner where f1 = touter.f1) as b
   from spc_i touter) ss,
  spc_p
where spc_p.id > ss.b::int;
explain (costs off, verbose)
select * from
  (select exists (select 1 from spc_i tinner where f1 = touter.f1) as b
   from spc_i touter) ss,
  spc_p
where spc_p.id > ss.b::int;
select * from
  (select exists (select 1 from spc_i tinner where f1 = touter.f1) as b
   from spc_i touter) ss,
  spc_p
where spc_p.id > ss.b::int - 1
order by 1, 2;

create table spc_a (a int, b int);
create table spc_b (a int, b int);
insert into spc_a select g, g % 5 from generate_series(1, 30) g;
insert into spc_b select g, g % 3 from generate_series(1, 30) g;
create index on spc_b (a);
analyze spc_a, spc_b;
-- A target list's sub-select is numbered before a qual's.
explain (verbose, costs off)
select (select max(a) from spc_b where spc_b.a = spc_a.a) from spc_a
where spc_a.b > (select max(b) from spc_b where spc_b.a = spc_a.a);
explain (verbose, costs off)
select (select max(a) from spc_b where spc_b.a = x.a)
from spc_a x join spc_a y on x.a = y.b and y.a > (select max(b) from spc_b where spc_b.a = x.b)
where x.b > (select max(b) from spc_b where spc_b.a = y.a);
-- A pulled-up output read in the target list and in a join clause (an index condition).
explain (costs off)
select ss.x, spc_b.a from (select (select max(a) from spc_b where spc_b.b = spc_a.b) as x, spc_a.a as y from spc_a) ss, spc_b
where spc_b.a > ss.x - 2 and ss.y < 4 order by 1, 2;
select ss.x, spc_b.a from (select (select max(a) from spc_b where spc_b.b = spc_a.b) as x, spc_a.a as y from spc_a) ss, spc_b
where spc_b.a > ss.x - 2 and ss.y < 4 order by 1, 2;
-- Read twice in the target list and once in a hash join's clause.
explain (verbose, costs off)
select ss.x, ss.x + 1, spc_b.a from (select (select min(a) from spc_b where spc_b.b = spc_a.b) as x from spc_a where spc_a.a < 3) ss
join spc_b on spc_b.a = ss.x;
select ss.x, ss.x + 1, spc_b.a from (select (select min(a) from spc_b where spc_b.b = spc_a.b) as x from spc_a where spc_a.a < 3) ss
join spc_b on spc_b.a = ss.x order by 1, 3;
-- A qual on a FROM subquery's sub-select output reads a copy of its own.
explain (verbose, costs off)
select ss.x, ss.a from (select (select max(a) from spc_b where spc_b.b = spc_a.b) x, a from spc_a) ss where ss.x > 5;
select ss.x from (select (select max(a) from spc_b where spc_b.b = spc_a.b) x, a from spc_a) ss where ss.x > 28 and ss.a < 10;
explain (costs off)
select * from (select a, exists (select 1 from spc_b where spc_b.a = spc_a.b) e from spc_a) s where e;
select * from (select a, exists (select 1 from spc_b where spc_b.a = spc_a.b) e from spc_a) s where e and a < 4 order by a;
-- A subquery over a join: its sub-select output copied too.
explain (costs off)
select ss.x, ss.y from (select (select max(a) from spc_b x where x.b = spc_a.b) x, spc_b.a y from spc_a join spc_b on spc_a.a = spc_b.a) ss, spc_a
where spc_a.b = ss.x % 5 and ss.y < 5;
select ss.x, ss.y from (select (select max(a) from spc_b x where x.b = spc_a.b) x, spc_b.a y from spc_a join spc_b on spc_a.a = spc_b.a) ss, spc_a
where spc_a.b = ss.x % 5 and ss.y < 5 order by 1, 2;
-- Nothing volatile is copied.
explain (costs off)
select * from (select a, (select random() > 2 from spc_b limit 1) e from spc_a) s where e;
drop table spc_p;
drop table spc_i;
drop table spc_a;
drop table spc_b;
