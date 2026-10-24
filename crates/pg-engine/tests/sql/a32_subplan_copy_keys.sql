-- A pulled-up subquery's sub-select output is copied into each place reading it
-- (pullup_replace_vars, make_subplan per copy): the copy in a join clause or a WHERE clause
-- is never equal() to the target list's, so it puts the output in no equivalence class
-- with the sort key (get_eclass_for_sort_expr): ORDER BY it and the column it is joined to
-- keeps both keys. And each copy runs where it is read: a projection reading the output
-- twice evaluates it twice (two SubPlans in EXPLAIN ANALYZE, the function called twice).
create table spk_a (a int, b int);
create table spk_b (a int, b int);
insert into spk_a select g, g % 5 from generate_series(1, 30) g;
insert into spk_b select g, g % 3 from generate_series(1, 30) g;
create index on spk_b (a);
analyze spk_a, spk_b;
explain (costs off)
select ss.x, spk_b.a, spk_b.b from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x from spk_a where spk_a.a < 8) ss
join spk_b on spk_b.a = ss.x order by ss.x, spk_b.a;
select ss.x, spk_b.a, spk_b.b from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x from spk_a where spk_a.a < 8) ss
join spk_b on spk_b.a = ss.x order by ss.x, spk_b.a;
explain (costs off)
select ss.x, spk_b.a from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x from spk_a where spk_a.a < 8) ss
join spk_b on spk_b.a = ss.x order by spk_b.a desc, ss.x;
select ss.x, spk_b.a from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x from spk_a where spk_a.a < 8) ss
join spk_b on spk_b.a = ss.x order by spk_b.a desc, ss.x;
-- A WHERE equality with a constant reads a copy too: the sort key stays.
explain (costs off)
select ss.x, ss.a from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x, a from spk_a) ss
where ss.x = 2 order by ss.x, ss.a;
-- A subquery not pulled up: its output is one Var, the second key redundant.
explain (costs off)
select ss.x, spk_b.a from (select (select min(a) from spk_b where spk_b.b = spk_a.b) as x from spk_a where spk_a.a < 8 offset 0) ss
join spk_b on spk_b.a = ss.x order by ss.x, spk_b.a;
-- Two copies in a projection, each run.
create function spk_noisy(int) returns int stable language plpgsql as $$
begin raise notice 'noisy %', $1; return $1; end $$;
explain (analyze, costs off, timing off, summary off, buffers off)
select ss.x, ss.x * 2 from (select (select min(a) from spk_b) x from spk_a where a < 3) ss;
select ss.x, ss.x + 1 from (select (select spk_noisy(max(a)) from spk_b where spk_b.b = spk_a.b) x from spk_a where a < 4) ss;
select ss.x, ss.x * 2 from (select (select spk_noisy(min(a)) from spk_b) x from spk_a where a < 3) ss;
-- Nothing volatile is pulled up: one value per row.
select count(*) filter (where x <> y) from (select ss.x x, ss.x y from (select (select random() + a) x from spk_a) ss) s2;
drop function spk_noisy(int);
drop table spk_a;
drop table spk_b;
