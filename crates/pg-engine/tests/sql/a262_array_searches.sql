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
