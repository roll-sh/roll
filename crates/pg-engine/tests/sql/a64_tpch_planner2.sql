-- Round 20, agent 64, second batch: sorts past work_mem, OR arms sharing clauses, group
-- estimates per relation, a grouped CTE's order.
create table a64_p as select g as k, g % 7 as j, g % 100 as h, 'x' || (g % 13) as t from generate_series(1, 20000) g;
create table a64_q as select g as k, g % 5 as j, g % 40 as h from generate_series(1, 5000) g;
create index on a64_q (k);
analyze a64_p, a64_q;

-- An external sort's disk costs (cost_sort past work_mem; tuplesort_merge_order).
set work_mem = '64kB';
explain select k, sum(j) from a64_p group by k;
explain select * from a64_p order by t, k;
reset work_mem;
explain select * from a64_p order by t, k;

-- A clause every arm of an OR has is taken out of it (process_duplicate_ors).
explain (costs off)
select count(*) from a64_p p, a64_q q
where (p.k = q.k and p.j = 1 and q.j = 2) or (p.k = q.k and p.j = 3 and q.h = 4)
   or (q.k = p.k and p.h = 5);
select count(*) from a64_p p, a64_q q
where (p.k = q.k and p.j = 1 and q.j = 2) or (p.k = q.k and p.j = 3 and q.h = 4)
   or (q.k = p.k and p.h = 5);
explain (costs off)
select count(*) from a64_p p, a64_q q
where (p.k = q.k and p.j = 1 and q.j = 2) or (p.k = q.k and p.j = 3 and q.h = 4)
   or (p.k = q.k and p.h = 5);
select count(*) from a64_p p, a64_q q
where (p.k = q.k and p.j = 1 and q.j = 2) or (p.k = q.k and p.j = 3 and q.h = 4)
   or (p.k = q.k and p.h = 5);
explain (costs off) select * from a64_q where (j = 1 and h = 2) or (j = 1);
explain (costs off) select * from a64_q where (j = 1 and h = 2) or (h = 2 and k = 3);

-- Groups over a join: each relation's values scaled by its own filtered rows.
explain select p.j, p.h, q.h, count(*) from a64_p p join a64_q q on p.k = q.k
where p.j < 3 group by p.j, p.h, q.h;
explain select p.j, q.h, count(*) from a64_p p join a64_q q on p.k = q.k
where q.k < 100 group by p.j, q.h;

-- A CTE grouped by its key: its scan has the grouping's order (no Sort above).
set enable_hashagg = off;
explain (costs off)
with c as materialized (select h, count(*) as n from a64_p group by h)
select c.h, c.n, q.k from c join a64_q q on q.k = c.h order by c.h;
with c as materialized (select h, count(*) as n from a64_p group by h)
select c.h, c.n, q.k from c join a64_q q on q.k = c.h order by c.h limit 5;
explain (costs off)
with c as materialized (select h, count(*) as n from a64_p group by h)
select * from c order by h;
reset enable_hashagg;

drop table a64_p, a64_q;
