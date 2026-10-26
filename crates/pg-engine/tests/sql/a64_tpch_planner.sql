-- Round 20, agent 64: TPC-H plan causes (planned partitions of a hashed aggregation, a group
-- key over an aggregate's output, join clauses' parallel safety).
create table a64_t as select g as k, g % 7 as j, g % 100 as h from generate_series(1, 20000) g;
create table a64_u as select g as k, g % 5 as j from generate_series(1, 5000) g;
analyze a64_t, a64_u;

-- A hashed aggregation expected to outgrow hash_mem plans its spill partitions.
set work_mem = '64kB';
set enable_sort = off;
explain select k, sum(j) from a64_t group by k;
explain (format json) select k, sum(j) from a64_t group by k;
explain select h, count(*) from a64_t group by h;
select count(*) from (select k, sum(j) from a64_t group by k) s;
reset work_mem;

-- A grouping keyed on an aggregate's output shows the aggregate as the child computes it
-- (hashed: the subquery scan has the aggregation's target list and goes).
explain (costs off) select c, count(*) from (select h, count(*) as c from a64_t group by h) s group by c;
select c, count(*) from (select h, count(*) as c from a64_t group by h) s group by c order by c;
reset enable_sort;

-- A join whose condition reads a correlated sub-select has no partial paths.
set parallel_setup_cost = 0;
set parallel_tuple_cost = 0;
set min_parallel_table_scan_size = 0;
explain (costs off)
select count(*) from a64_t a join a64_u b on a.k = b.k
  and a.j < (select max(c.j) from a64_t c where c.h = b.j);
select count(*) from a64_t a join a64_u b on a.k = b.k
  and a.j < (select max(c.j) from a64_t c where c.h = b.j);
reset parallel_setup_cost;
reset parallel_tuple_cost;
reset min_parallel_table_scan_size;

drop table a64_t, a64_u;
