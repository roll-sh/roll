-- Round 16 (agent 16): a query level's grouping returns the GROUP BY keys its select list
-- doesn't name (resjunk), and the sorts and limits over it carry them up to the level's top;
-- a sub-select's level too (aggregates 116).
explain (verbose, costs off)
select array(select sum(x+y) s
            from generate_series(1,3) y group by y order by s)
  from generate_series(1,3) x;
select array(select sum(x+y) s
            from generate_series(1,3) y group by y order by s)
  from generate_series(1,3) x;
explain (verbose, costs off)
select sum(y) s from generate_series(1,3) y group by y order by s;
explain (verbose, costs off)
select sum(y) s from generate_series(1,3) y group by y;
explain (verbose, costs off)
select (select sum(x+y) s from generate_series(1,3) y group by y limit 1) from generate_series(1,3) x;
explain (verbose, costs off)
select x, (select count(*) from generate_series(1,3) y group by y % 2 limit 1) from generate_series(1,3) x;
create table a16_gj (a int, b int);
insert into a16_gj select i % 4, i from generate_series(1, 20) i;
analyze a16_gj;
explain (verbose, costs off)
select array(select sum(b) from a16_gj group by a order by 1) from a16_gj;
explain (verbose, costs off)
select sum(b) from a16_gj group by a order by 1 limit 2;
select sum(b) from a16_gj group by a order by 1 limit 2;
drop table a16_gj;
