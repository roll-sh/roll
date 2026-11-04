-- Tuplestores in EXPLAIN ANALYZE (agent 164): a WindowAgg's tuplestore holds a partition's
-- rows only until no read pointer can reach them again (tuplestore_trim after each row,
-- tuplestore_clear between partitions), so its "Storage" depends on the functions' marks,
-- the frame's pointers and work_mem; a CTE's tuplestore and a Recursive Union's two tables
-- are counted as tuplestore.c counts them, on disk past work_mem.
create table a164w (g int, p int, q int, s text);
insert into a164w select g, g % 10, g % 7, md5(g::text) from generate_series(1, 20000) g;
analyze a164w;
set max_parallel_workers_per_gather = 0;
-- Marks at the current row, a running aggregate, a peer-group frame tail.
explain (analyze, costs off, timing off, summary off, buffers off)
select row_number() over (partition by p order by g) rn, sum(g) over (partition by p order by g) cs from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select rank() over (partition by p order by q) rn from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select sum(g) over (partition by p order by q) cs from a164w;
-- Functions that read the whole partition or keep its start.
explain (analyze, costs off, timing off, summary off, buffers off)
select ntile(3) over (partition by p order by g) rn from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select percent_rank() over (partition by p order by g) rn from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select sum(g) over (partition by p) rn from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select first_value(s) over (partition by p order by g) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select last_value(s) over (partition by p order by q) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select nth_value(g, 3) over (partition by p order by g) v from a164w;
-- Offsets: lag / lead, moving frames.
explain (analyze, costs off, timing off, summary off, buffers off)
select lag(g) over (partition by p order by g) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select lead(g, 5) over (partition by p order by g) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select sum(g) over (partition by p order by g rows between 3 preceding and 2 following) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) over (partition by p order by q range between current row and unbounded following) v from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select sum(g) over (partition by p order by g rows between 5 preceding and current row exclude current row) v from a164w;
-- No partitions: one partition of every row.
explain (analyze, costs off, timing off, summary off, buffers off)
select row_number() over (order by g) rn from a164w;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) over () rn from a164w where g <= 3000;
-- On disk past work_mem (a table whose sorts merge in one pass at 64 kB).
create table a164v (g int, p int, s text);
insert into a164v select g, g % 2, md5(g::text) from generate_series(1, 4000) g;
analyze a164v;
set work_mem = '64kB';
explain (analyze, costs off, timing off, summary off, buffers off)
select ntile(3) over (partition by p order by g) rn from a164v;
explain (analyze, costs off, timing off, summary off, buffers off)
select row_number() over (partition by p order by g) rn, sum(g) over (partition by p order by g) cs from a164v;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) over () rn from a164v where g <= 3000;
select sum(rn), count(*) from (select ntile(3) over (partition by p order by g) rn from a164v) x;
set work_mem = '256kB';
explain (analyze, costs off, timing off, summary off, buffers off)
select ntile(3) over (partition by p order by g) rn from a164v;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) over () rn from a164v where g <= 3000;
-- A CTE's tuplestore, read by two scans, in memory and on disk.
reset work_mem;
explain (analyze, costs off, timing off, summary off, buffers off)
with c as materialized (select * from a164w) select count(*) from c a, c b where a.g = b.g and a.g < 10;
explain (analyze, costs off, timing off, summary off, buffers off)
with c as materialized (select g, s from a164w where g <= 500) select count(*) from c a join c b on a.g = b.g + 1;
set work_mem = '64kB';
explain (analyze, costs off, timing off, summary off, buffers off)
with c as materialized (select * from a164w) select count(*) from c a, c b where a.g = b.g and a.g < 10;
with c as materialized (select g, s from a164w) select count(*), sum(g), md5(string_agg(s, ',')) from c;
with c as materialized (select g, s from a164w) select a.g, a.s from c a, c b where a.g = b.g + 19990 order by a.g;
with c as materialized (select g, s from a164w) select (select count(*) from c), count(*), min(s) from c where g > 10000;
-- A LIMIT reads part of it.
with c as materialized (select g, s from a164w) select g, s from c limit 3;
set work_mem = '1MB';
explain (analyze, costs off, timing off, summary off, buffers off)
with c as materialized (select * from a164w) select count(*) from c a, c b where a.g = b.g and a.g < 10;
-- Recursive unions: the working and intermediate tables.
reset work_mem;
explain (analyze, costs off, timing off, summary off, buffers off)
with recursive r(n, s) as (select g, s from a164w where g <= 2000 union all select n + 2000, s from r where n < 16000) select count(*) from r;
explain (analyze, costs off, timing off, summary off, buffers off)
with recursive r(n) as (select 1 union select n % 500 + 1 from r) select count(*) from r;
set work_mem = '64kB';
with recursive r(n, s) as (select g, s from a164w where g <= 2000 union all select n + 2000, s from r where n < 16000) select count(*), sum(n), md5(string_agg(s, ',')) from r;
reset work_mem;
drop table a164w, a164v;
