-- Query memory in EXPLAIN ANALYZE (agent 150): the tuples a Sort or a Materialize holds
-- are those of its input's target list (CP_SMALL_TLIST: the columns read above), so a
-- Sort of one column is a datum sort, and a Materialize's tuplestore counts its tuples
-- as tuplestore.c does (a generation chunk each, a pointer array of 2048 to start).
create table a150t (unique1 int, unique2 int, twenty int, thousand int, s text, pad text);
insert into a150t select g, 9999 - g, g % 20, g % 1000, md5(g::text), repeat('p', 30) from generate_series(0, 9999) g;
analyze a150t;
set max_parallel_workers_per_gather = 0;
set enable_hashjoin = off;
set enable_memoize = off;
-- Merge joins' Sorts (a full join reads both sides to their end).
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) from a150t t1 full join a150t t2 on t1.unique1 = t2.twenty;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*), max(t2.s) from a150t t1 full join a150t t2 on t1.unique1 = t2.twenty;
explain (analyze, costs off, timing off, summary off, buffers off)
select t1.s, t2.pad from a150t t1 full join a150t t2 on t1.unique1 = t2.thousand where t1.s > 'f';
-- A nested loop's Materialize.
set enable_mergejoin = off;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*) from a150t t1 join a150t t2 on t1.unique1 = t2.thousand where t2.unique1 < 1200;
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*), max(t2.s) from a150t t1 join a150t t2 on t1.unique1 = t2.thousand where t2.unique1 < 3000 and t1.unique1 < 20;
set work_mem = '64kB';
explain (analyze, costs off, timing off, summary off, buffers off)
select count(*), max(t2.s) from a150t t1 join a150t t2 on t1.unique1 = t2.thousand where t2.unique1 < 1200;
reset work_mem;
reset enable_mergejoin;
reset enable_hashjoin;
reset enable_memoize;
drop table a150t;
