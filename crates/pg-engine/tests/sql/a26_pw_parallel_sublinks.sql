-- Round 16, agent 26: partitionwise grouping over a full join, a filter over a join costed
-- on the join's tuples, sort keys over a partitionwise join of sub-partitioned tables,
-- debug_parallel_query over parameterized nested loops, an ANY sub-select reading only an
-- outer query's values, quals over PlaceHolderVars.
create table a26_p1 (a int, b int, c varchar) partition by range (a);
create table a26_p1_1 partition of a26_p1 for values from (0) to (250);
create table a26_p1_3 partition of a26_p1 for values from (500) to (600);
create table a26_p1_2 partition of a26_p1 for values from (250) to (500);
insert into a26_p1 select i, i % 25, to_char(i, 'FM0000') from generate_series(0, 599) i where i % 2 = 0;
create table a26_p2 (a int, b int, c varchar) partition by range (b);
create table a26_p2_1 partition of a26_p2 for values from (0) to (250);
create table a26_p2_2 partition of a26_p2 for values from (250) to (500);
create table a26_p2_3 partition of a26_p2 for values from (500) to (600);
insert into a26_p2 select i % 25, i, to_char(i, 'FM0000') from generate_series(0, 599) i where i % 3 = 0;
analyze a26_p1, a26_p2;
set enable_partitionwise_join = on;
set enable_partitionwise_aggregate = on;
set enable_hashjoin = off;
-- (A Filter over a merge join costs its quals per mergejointuples, not per row out.)
explain select a, b from a26_p1_1 p1 full join a26_p2_1 p2(b, a, c) using (a, b)
  where a between 490 and 510;
explain select a, b from a26_p1_1 p1 full join a26_p2_1 p2(b, a, c) using (a, b);
-- (The partial partitionwise grouping, fuzzily as cheap but starting sooner, wins.)
explain (costs off)
select a, b from a26_p1 p1 full join a26_p2 p2(b, a, c) using (a, b)
  where a between 490 and 510 group by 1, 2 order by 1, 2;
select a, b from a26_p1 p1 full join a26_p2 p2(b, a, c) using (a, b)
  where a between 490 and 510 group by 1, 2 order by 1, 2;
reset enable_partitionwise_aggregate;
reset enable_hashjoin;
-- Sub-partitioned tables joined partitionwise at the top level only: the sort keys above
-- the Append are the parents' columns, the child join's its first partitions'.
create table a26_al (a double precision, b int, c text) partition by range (a);
create table a26_al_neg partition of a26_al for values from ('-Infinity') to (0) partition by range (b);
create table a26_al_pos partition of a26_al for values from (0) to (10.0) partition by list (c);
create table a26_al_neg_p1 partition of a26_al_neg for values from (100) to (200);
create table a26_al_neg_p2 partition of a26_al_neg for values from (200) to (300);
create table a26_al_neg_p3 partition of a26_al_neg for values from (300) to (400);
create table a26_al_pos_p1 partition of a26_al_pos for values in ('0001', '0003');
create table a26_al_pos_p2 partition of a26_al_pos for values in ('0004', '0006');
create table a26_al_pos_p3 partition of a26_al_pos for values in ('0008', '0009');
insert into a26_al_neg select -1.0, i, to_char(i % 10, 'FM0000') from generate_series(100, 399) i where i % 10 in (1, 3, 4, 6, 8, 9);
insert into a26_al_pos select 1.0, i, to_char(i % 10, 'FM0000') from generate_series(100, 399) i where i % 10 in (1, 3, 4, 6, 8, 9);
create table a26_be (a double precision, b int, c text) partition by range (a);
create table a26_be_neg partition of a26_be for values from (-10.0) to (0) partition by range (b);
create table a26_be_pos partition of a26_be for values from (0) to ('Infinity') partition by list (c);
create table a26_be_neg_p1 partition of a26_be_neg for values from (100) to (150);
create table a26_be_neg_p2 partition of a26_be_neg for values from (200) to (300);
create table a26_be_neg_p3 partition of a26_be_neg for values from (350) to (500);
create table a26_be_pos_p1 partition of a26_be_pos for values in ('0002', '0003');
create table a26_be_pos_p2 partition of a26_be_pos for values in ('0004', '0006');
create table a26_be_pos_p3 partition of a26_be_pos for values in ('0007', '0009');
insert into a26_be_neg select -1.0, i, to_char(i % 10, 'FM0000') from generate_series(100, 149) i where i % 10 in (2, 3, 4, 6, 7, 9);
insert into a26_be_neg select -1.0, i, to_char(i % 10, 'FM0000') from generate_series(200, 299) i where i % 10 in (2, 3, 4, 6, 7, 9);
insert into a26_be_neg select -1.0, i, to_char(i % 10, 'FM0000') from generate_series(350, 499) i where i % 10 in (2, 3, 4, 6, 7, 9);
insert into a26_be_pos select 1.0, i, to_char(i % 10, 'FM0000') from generate_series(100, 149) i where i % 10 in (2, 3, 4, 6, 7, 9);
insert into a26_be_pos select 1.0, i, to_char(i % 10, 'FM0000') from generate_series(200, 299) i where i % 10 in (2, 3, 4, 6, 7, 9);
insert into a26_be_pos select 1.0, i, to_char(i % 10, 'FM0000') from generate_series(350, 499) i where i % 10 in (2, 3, 4, 6, 7, 9);
analyze a26_al, a26_be;
explain (costs off)
select t1.*, t2.* from a26_al t1 join a26_be t2 on (t1.a = t2.a and t1.c = t2.c)
  where ((t1.b >= 100 and t1.b < 110) or (t1.b >= 200 and t1.b < 210))
    and ((t2.b >= 100 and t2.b < 110) or (t2.b >= 200 and t2.b < 210)) and t1.c in ('0004', '0009')
  order by t1.a, t1.b, t2.b;
select t1.*, t2.* from a26_al t1 join a26_be t2 on (t1.a = t2.a and t1.c = t2.c)
  where ((t1.b >= 100 and t1.b < 110) or (t1.b >= 200 and t1.b < 210))
    and ((t2.b >= 100 and t2.b < 110) or (t2.b >= 200 and t2.b < 210)) and t1.c in ('0004', '0009')
  order by t1.a, t1.b, t2.b;
reset enable_partitionwise_join;
-- debug_parallel_query: a nested loop's parameters are set in the process running it, no
-- hazard: the plan runs under a single-copy Gather.
create table a26_h (a int, b int, c text) partition by hash (c);
create table a26_h_1 partition of a26_h for values with (modulus 3, remainder 0);
create table a26_h_2 partition of a26_h for values with (modulus 3, remainder 1);
create table a26_h_3 partition of a26_h for values with (modulus 3, remainder 2);
insert into a26_h select i, i % 25, to_char(i % 12, 'FM0000') from generate_series(0, 599, 2) i;
create index a26_h_c on a26_h (c);
analyze a26_h;
set max_parallel_workers_per_gather = 1;
set debug_parallel_query = on;
set enable_partitionwise_join = on;
explain (costs off) select * from a26_h p1 join a26_h p2 using (c) limit 100;
set enable_memoize = off;
set enable_material = off;
set enable_hashjoin = off;
set enable_mergejoin = off;
set enable_seqscan = off;
explain (costs off) select * from a26_h_1 p1 join a26_h_1 p2 using (c);
reset enable_memoize;
reset enable_material;
reset enable_hashjoin;
reset enable_mergejoin;
reset enable_seqscan;
reset enable_partitionwise_join;
reset debug_parallel_query;
reset max_parallel_workers_per_gather;
-- An ANY sub-select (not a simple one: OFFSET 0) reading only an outer query's value
-- joins as a subquery of its own (convert_ANY_sublink_to_join: only the parent query's
-- Vars bar it).
create table a26_t (unique1 int, unique2 int);
insert into a26_t select i, 9999 - i from generate_series(0, 9999) i;
create unique index a26_t_u2 on a26_t (unique2);
create table a26_i (f1 int);
insert into a26_i values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze a26_t, a26_i;
explain (verbose, costs off)
select * from (values (0, 9998), (1, 1000)) v(id, x),
  lateral (select f1 from a26_i where f1 = any (select unique1 from a26_t where unique2 = v.x offset 0)) ss;
select * from (values (0, 9998), (1, 1000)) v(id, x),
  lateral (select f1 from a26_i where f1 = any (select unique1 from a26_t where unique2 = v.x offset 0)) ss;
select * from (values (0, 9998), (1, 1000), (2, 3), (3, 3)) v(id, x),
  lateral (select unique1 from a26_t t where t.unique2 = any
    (select unique2 from a26_t where unique2 < v.x order by unique2 limit 2)) ss order by 1, 3;
select v.id, (select count(*) from a26_i i where i.f1 = any
    (select unique1 from a26_t where unique2 = v.x offset 0))
  from (values (0, 9998), (1, 1000), (2, 5)) v(id, x) order by 1;
-- A qual over a PlaceHolderVar (a pulled-up subquery's constant output an outer join nulled
-- as written, the join then reduced) is a restriction of its relation: a Filter.
create table a26_e (id int primary key, code int);
insert into a26_e values (1, 1), (2, 2);
analyze a26_e;
explain (verbose, costs off)
select 1 from a26_e t1 left join (select 1 as x, * from a26_e t2) s1 on true where s1.x = 1;
explain (verbose, costs off)
select 1 from a26_e t1 left join (select 1 as x, * from a26_e t2) s1 on true where s1.x is not null;
select 1 from a26_e t1 left join (select 1 as x, * from a26_e t2) s1 on true where s1.x = 1;
drop table a26_p1, a26_p2, a26_al, a26_be, a26_h, a26_t, a26_i, a26_e;
