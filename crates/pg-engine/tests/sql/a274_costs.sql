-- agent 274: a left join to a subquery grouped on its char join key, nothing of it read, is
-- removed (the char(1) to bpchar cast is a RelabelType); hashed `= ANY` / `<> ALL` over a
-- constant array of 9 or more elements costs a hash table built once and a hash and a
-- comparison per row (convert_saop_to_hashed_saop, cost_qual_eval); NULLIF and IS DISTINCT
-- FROM cost their `=`.
create table a274k (code char primary key);
create table a274g (a char not null, num int not null, primary key (a, num));
explain select a274k.code from a274k left join (select count(1) as cnt, a274g.a from a274g group by a274g.a) g on a274k.code = g.a;
create table a274s (a int, b text, c numeric, f float8, i int) with (autovacuum_enabled = off);
insert into a274s select g, g::text, g, g / 3.0, g % 7 from generate_series(1, 5000) g;
analyze a274s;
explain select * from a274s where a in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
explain select * from a274s where b in ('1', '2', '3', '4', '5', '6', '7', '8', '9', '10');
explain select * from a274s where a not in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
explain select * from a274s where a in (1, 2, 3, 4, 5, 6, 7, 8);
explain select * from a274s where c = any ('{1,2,3,4,5,6,7,8,9,10}'::numeric[]);
explain select a, a in (10, 9, 2, 8, 3, 7, 4, 6, 5, 1) from a274s;
explain select * from a274s where nullif(f, i) = 2;
explain select * from a274s where f is distinct from i;
explain select * from a274s where f is not distinct from i;
explain select * from a274s where a is distinct from 5;
