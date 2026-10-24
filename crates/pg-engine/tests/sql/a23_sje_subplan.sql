-- Round 16 (agent 23): self-join elimination by range table index, a pulled-up subquery's
-- scan returning its physical target list, equivalence classes with outer values in
-- sub-selects that stay SubPlans (join 612, 632, 654, 655).
create table a23_sj (a int unique, b int, c int unique);
insert into a23_sj values (1, null, 2), (null, 2, null), (2, 1, 1), (42, 3, 4), (3, 3, 3);
analyze a23_sj;
create table a23_i4 (f1 int4);
insert into a23_i4 values (0), (1), (2), (3), (42);
analyze a23_i4;

-- remove_self_joins_one_group: of two scans of one table, the one with the lower range
-- table index goes (a pulled-up subquery's relations come after the query's own).
explain (costs off)
select t4.* from (select t1.*, t2.a as a1 from a23_sj t1, a23_sj t2 where t1.b = t2.b) as t3
  join a23_sj t4 on (t4.a = t3.a) where t3.a1 = 42;
select t4.* from (select t1.*, t2.a as a1 from a23_sj t1, a23_sj t2 where t1.b = t2.b) as t3
  join a23_sj t4 on (t4.a = t3.a) where t3.a1 = 42;
explain (costs off)
select * from a23_sj t4 join (select t1.* from a23_sj t1, a23_i4 i where t1.b = i.f1) s on s.a = t4.a;
select * from a23_sj t4 join (select t1.* from a23_sj t1, a23_i4 i where t1.b = i.f1) s on s.a = t4.a order by 1;

-- A pulled-up subquery's selection of a scan's columns is no node: the scan a join reads as
-- it is returns its physical target list.
explain (verbose, costs off)
select 1 from (select x.* from a23_sj x, a23_sj y where x.a = y.a) q, lateral generate_series(1, q.a) gs(i);
explain (verbose, costs off)
select 1 from (select y.* from a23_sj x, a23_sj y where x.a = y.a) q, lateral generate_series(1, q.a) gs(i);
select count(*) from (select y.* from a23_sj x, a23_sj y where x.a = y.a) q, lateral generate_series(1, q.a) gs(i);
explain (verbose, costs off)
select q.b, gs.i from (select y.b, y.a from a23_sj y) q, lateral generate_series(1, q.a) gs(i);
select q.b, gs.i from (select y.b, y.a from a23_sj y) q, lateral generate_series(1, q.a) gs(i) order by 1, 2;
-- (Under a Hash or Materialize: the small target list still.)
explain (verbose, costs off)
select q.b from (select y.b, y.a from a23_sj y) q join a23_i4 i on q.a = i.f1;

-- A sub-select's class with outer values: compared with its last pseudo-constant (its
-- first Const), the others a gating qual.
explain (costs off)
select t1.a, (select a from a23_sj where a = t2.a and a = t1.a) from a23_sj t1, a23_sj t2 where t1.a = t2.a;
select t1.a, (select a from a23_sj where a = t2.a and a = t1.a) from a23_sj t1, a23_sj t2 where t1.a = t2.a order by 1;
explain (costs off)
select (select a from a23_sj where a = t1.a and a = t1.b) from a23_sj t1;
select (select a from a23_sj where a = t1.a and a = t1.b) from a23_sj t1;
explain (costs off)
select (select a from a23_sj where a = t1.a and a = 3) from a23_sj t1;
select (select a from a23_sj where a = t1.a and a = 3) from a23_sj t1;
explain (costs off)
select (select a from a23_sj where a = t1.a and a = t1.b and a = t1.c) from a23_sj t1;
select (select a from a23_sj where a = t1.a and a = t1.b and a = t1.c) from a23_sj t1;
explain (costs off)
select (select a from a23_sj where a = t1.a and b = t1.b and a = t1.b) from a23_sj t1;
select (select a from a23_sj where a = t1.a and b = t1.b and a = t1.b) from a23_sj t1;
explain (costs off)
select (select a from a23_sj where a = t1.a and a = t1.b limit 1) from a23_sj t1;
select (select a from a23_sj where a = t1.a and a = t1.b limit 1) from a23_sj t1;
explain (costs off)
select * from a23_sj t1 where exists (select 1 from a23_sj where a = t1.a and a = t1.b offset 0);
select * from a23_sj t1 where exists (select 1 from a23_sj where a = t1.a and a = t1.b offset 0);
drop table a23_sj, a23_i4;
