-- EXPLAIN VERBOSE Output lists as Postgres builds target lists: physical ones for plain
-- scans where allowed, just the needed columns under a Hash or a Sort (CP_SMALL_TLIST), the
-- statement's resjunk sort and window columns at its top, none for Append.
create table evo_1 (a int, b int, c text, d numeric);
create table evo_2 (x int, y int, z text);
insert into evo_1 select g, g % 10, 'c' || g, g * 1.5 from generate_series(1, 1000) g;
insert into evo_2 select g, g % 7, 'z' || g from generate_series(1, 500) g;
create index on evo_1 (b);
analyze evo_1; analyze evo_2;
explain (verbose, costs off) select a from evo_1;
explain (verbose, costs off) select * from evo_1;
explain (verbose, costs off) select a + 1, c from evo_1 where b = 3;
explain (verbose, costs off) select a, z from evo_1 join evo_2 on a = x;
explain (verbose, costs off) select a, z from evo_1 join evo_2 on a = x where y > 3;
explain (verbose, costs off) select b, count(*) from evo_1 group by b;
explain (verbose, costs off) select b, sum(d) from evo_1 where a < 100 group by b order by b;
explain (verbose, costs off) select distinct c from evo_1;
explain (verbose, costs off) select a from evo_1 order by c limit 3;
explain (verbose, costs off) select a, (select max(x) from evo_2 where y = b) from evo_1;
explain (verbose, costs off) select a from evo_1 union all select x from evo_2;
explain (verbose, costs off) select a from evo_1 where exists (select 1 from evo_2 where x = a);
explain (verbose, costs off) select v1.a, v2.z from evo_1 v1 left join evo_2 v2 on v1.b = v2.y and v2.x < 10;
explain (verbose, costs off) select count(*) from evo_1;
explain (verbose, costs off) select a, row_number() over (partition by b order by a) from evo_1;
