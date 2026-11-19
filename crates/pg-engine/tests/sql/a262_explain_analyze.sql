-- EXPLAIN ANALYZE's Index Searches of an index scan over an array's values (= ANY): one
-- primitive index scan per run of values whose matches begin on the leaf page the scan is
-- on (_bt_advance_array_keys), a new one for a value on another leaf.
create table sa (a int, b int);
insert into sa select g, g % 10 from generate_series(1, 20000) g;
create index sa_a on sa (a);
vacuum analyze sa;
set enable_seqscan = off;
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{1,2}');
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{1,400}');
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{1,2,5000,5001,19999}');
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{3,1,2,30000}');
set enable_indexscan = off;
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{1,2}');
explain (analyze, costs off, timing off, summary off, buffers off) select * from sa where a = any ('{1,2,5000,5001,19999}');
drop table sa;
reset enable_seqscan;
reset enable_indexscan;
-- An Incremental Sort under a LIMIT sorts each group bounded (cost_incremental_sort's
-- limit_tuples): its costs follow the limit.
create table ti (a int, b int);
insert into ti select g / 10, g % 7 from generate_series(1, 10000) g;
create index ti_a on ti (a);
create table tj (a int, b int);
insert into tj select g / 1000, g % 7 from generate_series(1, 10000) g;
create index tj_a on tj (a);
analyze ti;
analyze tj;
explain select * from ti order by a, b limit 10;
explain select * from tj order by a, b limit 10;
explain select * from tj order by a, b limit 10 offset 5;
drop table ti, tj;
