-- Memoize (get_memoize_path, cost_memoize_rescan): a nested loop's parameterized inner
-- side cached on the outer values it reads, chosen by cost; EXPLAIN's Cache Key / Cache
-- Mode and, under ANALYZE, its hits, misses and memory.
create table mz1 (a int, n numeric, c int, x text);
create table mz2 (b int, y text, nb numeric, d int);
create table mz3 (id int primary key, z text);
insert into mz1 select g % 10, (g % 7) * 1.5, g % 3, 'x' || g from generate_series(1, 1000) g;
insert into mz2 select g, 'y' || g, g * 1.5, g % 5 from generate_series(1, 20000) g;
insert into mz3 select g, 'z' || g from generate_series(1, 20000) g;
create index on mz2 (b);
create index on mz2 (nb);
analyze mz1; analyze mz2; analyze mz3;
explain select * from mz1 join mz2 on b = a;
explain select * from mz1 join mz2 on nb = n;
explain select * from mz1 join mz3 on id = a;
explain select * from mz1 where exists (select 1 from mz3 where id = a);
explain select * from mz1, lateral (select * from mz2 where b = mz1.a limit 1) s;
explain (costs off) select * from mz1 join mz2 on b > a and b < a + 2;
explain (analyze, costs off, timing off, summary off, buffers off) select * from mz1 join mz3 on id = a;
explain (analyze, costs off, timing off, summary off, buffers off) select * from mz1 join mz2 on b = a;
select count(*), sum(b) from mz1 join mz2 on b = a;
set enable_memoize = off;
explain (costs off) select * from mz1 join mz2 on b = a;
reset enable_memoize;
