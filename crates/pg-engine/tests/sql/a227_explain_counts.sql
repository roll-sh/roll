-- Agent 227: EXPLAIN ANALYZE's counts where the engine's run differs from PostgreSQL's
-- executor: a nested loop's inner scan under a Gather starts once per outer row, whichever
-- process reads that row (its loops are not multiplied by the processes); a nested loop
-- anti or semi join reads its inner side up to its first match; a plain partial aggregation
-- returns a row from each process; a Hash Right Semi Join's Hash holds the columns the join
-- reads and returns.
create table a227_c (ck int primary key, nk int, bal int, name text, pad text);
create table a227_o (ok int primary key, ck int, st char(1), pad text);
insert into a227_c select g, g % 25, (g * 37) % 1000, 'C' || g, repeat('c', 60) from generate_series(1, 3000) g;
insert into a227_o select g, (g * 7) % 2000 + 1, case when g % 3 = 0 then 'F' else 'O' end, repeat('o', 40) from generate_series(1, 12000) g;
create index a227_o_ck on a227_o (ck);
vacuum analyze a227_c, a227_o;
set parallel_setup_cost = 0;
set parallel_tuple_cost = 0;
set min_parallel_table_scan_size = 0;
set min_parallel_index_scan_size = 0;
set max_parallel_workers_per_gather = 2;
set enable_hashjoin = off;
set enable_mergejoin = off;
set enable_material = off;
set enable_memoize = off;
set enable_bitmapscan = off;
-- The inner index scan's loops: one per outer row of all the processes.
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) from a227_c c join a227_o o on o.ck = c.ck where c.bal < 300;
-- An anti join's inner index-only scan: up to the first entry of each customer's.
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) from a227_c c where c.bal < 500 and not exists (select from a227_o o where o.ck = c.ck);
-- A semi join's: the same.
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) from a227_c c where c.bal < 500 and exists (select from a227_o o where o.ck = c.ck);
reset enable_hashjoin;
reset enable_mergejoin;
reset enable_material;
reset enable_memoize;
reset enable_bitmapscan;
-- A plain partial aggregation: a row from the leader and each worker.
explain (analyze, costs off, timing off, summary off, buffers off)
select sum(bal) from a227_c where bal > 100;
reset max_parallel_workers_per_gather;
reset min_parallel_index_scan_size;
reset min_parallel_table_scan_size;
reset parallel_tuple_cost;
reset parallel_setup_cost;
-- A Hash Right Semi Join's Hash: its tuples of the columns it reads and returns.
explain (analyze, costs off, timing off, summary off, buffers off)
select c.name from a227_c c where c.nk = 3 and c.ck in (select o.ck from a227_o o where o.st = 'F');
