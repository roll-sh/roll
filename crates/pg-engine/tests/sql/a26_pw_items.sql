-- Round 16, agent 26: partitionwise joins of some of a join group's items (Postgres's search
-- gives every join relation its partitionwise paths), the topmost relation's items' joins
-- made partitionwise for it, self-join elimination over PlaceHolderVars.
create table a26_r1 (a int, b int, c varchar) partition by range (a);
create table a26_r1_1 partition of a26_r1 for values from (0) to (250);
create table a26_r1_3 partition of a26_r1 for values from (500) to (600);
create table a26_r1_2 partition of a26_r1 for values from (250) to (500);
insert into a26_r1 select i, i % 25, to_char(i, 'FM0000') from generate_series(0, 599) i where i % 2 = 0;
create index on a26_r1_1 (a);
create index on a26_r1_2 (a);
create index on a26_r1_3 (a);
create table a26_r2 (a int, b int, c varchar) partition by range (b);
create table a26_r2_1 partition of a26_r2 for values from (0) to (250);
create table a26_r2_2 partition of a26_r2 for values from (250) to (500);
create table a26_r2_3 partition of a26_r2 for values from (500) to (600);
insert into a26_r2 select i % 25, i, to_char(i, 'FM0000') from generate_series(0, 599) i where i % 3 = 0;
create table a26_r4 (a int, b int, c text) partition by range (a);
create table a26_r4_1 partition of a26_r4 for values from (0) to (300);
create table a26_r4_2 partition of a26_r4 for values from (300) to (500);
create table a26_r4_3 partition of a26_r4 for values from (500) to (600);
insert into a26_r4 select i, i, to_char(i, 'FM0000') from generate_series(0, 599, 2) i;
analyze a26_r1, a26_r2, a26_r4;
set enable_partitionwise_join = on;
-- (a26_r4's bounds aren't the others': t1 ⋈ t3 joins partitionwise under the hash join.)
explain (costs off)
select t1.a, t1.c, t2.b, t2.c from a26_r1 t1, a26_r4 t2, a26_r2 t3 where t1.a = t2.a and t1.a = t3.b;
select count(*), sum(t1.a), sum(t2.b) from a26_r1 t1, a26_r4 t2, a26_r2 t3 where t1.a = t2.a and t1.a = t3.b;
-- The topmost relation: a FULL JOIN of a LEFT JOIN and a third table over merged list
-- bounds, joined partitionwise though the LEFT JOIN alone is cheaper joined plainly.
create table a26_l1 (a int, b int, c text) partition by list (c);
create table a26_l1_1 partition of a26_l1 for values in ('0000', '0001', '0002');
create table a26_l1_2 partition of a26_l1 for values in ('0003', '0004');
insert into a26_l1 select i, i, to_char(i % 5, 'FM0000') from generate_series(0, 24) i;
create table a26_l2 (a int, b int, c text) partition by list (c);
create table a26_l2_1 partition of a26_l2 for values in ('0002');
create table a26_l2_2 partition of a26_l2 for values in ('0003', '0004');
insert into a26_l2 select i, i, to_char(i % 5, 'FM0000') from generate_series(0, 24) i where i % 5 in (2, 3, 4);
create table a26_l3 (a int, b int, c text) partition by list (c);
create table a26_l3_1 partition of a26_l3 for values in ('0001');
create table a26_l3_2 partition of a26_l3 for values in ('0003', '0004');
insert into a26_l3 select i, i, to_char(i % 5, 'FM0000') from generate_series(0, 24) i where i % 5 in (1, 3, 4);
analyze a26_l1, a26_l2, a26_l3;
explain (costs off)
select t1.a, t1.c, t2.a, t2.c, t3.a, t3.c from (a26_l1 t1 left join a26_l2 t2 on (t1.c = t2.c))
  full join a26_l3 t3 on (t1.c = t3.c)
  where coalesce(t1.a, 0) % 5 != 3 and coalesce(t1.a, 0) % 5 != 4 order by t1.c, t1.a, t2.a, t3.a;
select t1.a, t1.c, t2.a, t2.c, t3.a, t3.c from (a26_l1 t1 left join a26_l2 t2 on (t1.c = t2.c))
  full join a26_l3 t3 on (t1.c = t3.c)
  where coalesce(t1.a, 0) % 5 != 3 and coalesce(t1.a, 0) % 5 != 4 order by t1.c, t1.a, t2.a, t3.a;
reset enable_partitionwise_join;
-- Self-join elimination with a PlaceHolderVar of the removed relation: it is the kept one's.
create table a26_e (id int primary key, code int);
insert into a26_e select i, i % 3 from generate_series(1, 6) i;
analyze a26_e;
set enable_hashjoin = off;
set enable_mergejoin = off;
explain (costs off)
select 1 from a26_e t1 left join
    ((select 1 as x, * from a26_e t2) s1 inner join (select * from a26_e t3) s2 on s1.id = s2.id)
    on true
  where s1.x = 1;
select count(*) from a26_e t1 left join
    ((select 1 as x, * from a26_e t2) s1 inner join (select * from a26_e t3) s2 on s1.id = s2.id)
    on true
  where s1.x = 1;
select s1.x, s2.id from a26_e t1 left join
    ((select 2 as x, * from a26_e t2) s1 inner join (select * from a26_e t3) s2 on s1.id = s2.id)
    on true
  where s1.x = 2 order by 2 limit 3;
reset enable_hashjoin;
reset enable_mergejoin;
drop table a26_r1, a26_r2, a26_r4, a26_l1, a26_l2, a26_l3, a26_e;
