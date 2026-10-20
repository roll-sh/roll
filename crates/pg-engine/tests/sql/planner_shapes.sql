-- Plan shapes: window evaluation order (and the sorts it saves, incremental ones),
-- UNION under ORDER BY merging sorted arms, DISTINCT and ORDER BY keys that are constant
-- (or that WHERE fixes) dropped, equivalence classes with a constant across joins, and
-- column qualification when a FROM-less level adds a range table entry.
set jit = off;
create table ps_w(a int, b int, c text, d varchar(10));
insert into ps_w select g % 10, g % 7, 'c'||g, 'd'||(g%5) from generate_series(1,100) g;
explain (costs off) select a, b, row_number() over (partition by a order by b), rank() over (order by b) from ps_w;
explain (costs off) select a, b, sum(b) over (partition by a), count(*) over (partition by a order by b) from ps_w;
explain (costs off) select a, row_number() over (order by a, b), sum(b) over (order by a) from ps_w;
explain (costs off) select * from ps_w order by a, b;
explain (costs off) select a, b, row_number() over (partition by a order by b) from ps_w order by a, b, c;
explain (costs off) select a from ps_w union select b from ps_w;
explain (costs off) select a, c from ps_w union select b, c from ps_w;
explain (costs off) select a from ps_w union select b from ps_w order by 1;
explain (costs off) select distinct a, 1 as one from ps_w;
explain (costs off) select distinct a, 'x' from ps_w order by a;
explain (costs off) select distinct on (a) a, b from ps_w where b = 3 order by a, b;
explain (costs off) select * from ps_w where a = 1 order by a, b;
explain (costs off) select * from ps_w w1 join ps_w w2 on w1.c = w2.d where w2.d = 'x';
explain (costs off, verbose) select * from ps_w w1 join ps_w w2 on w1.c = w2.d where w1.c = 'x';
explain (costs off, verbose) select * from ps_w where d = c;
explain (costs off, verbose) select (select max(a) from ps_w) as m;
explain (costs off, verbose) select 1 where exists (select 1 from ps_w);
explain (costs off) select (select max(a) from ps_w), (select min(b) from ps_w);
explain (costs off) select * from ps_w w1, ps_w w2 where w1.a = w2.b and w1.a = 3;
explain (costs off) select * from ps_w w1 join ps_w w2 on w1.a = w2.a join ps_w w3 on w2.a = w3.b where w3.b = 4;
explain (costs off) select * from ps_w w1 left join ps_w w2 on w1.a = w2.a where w1.a = 3;
select count(*) from ps_w w1, ps_w w2 where w1.a = w2.b and w1.a = 3;
