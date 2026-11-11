-- Round 16 (agent 46): a LATERAL item's join clauses moved into its parameterized path
-- (a Subquery Scan's filter) size the join relation as set_joinrel_size_estimates does:
-- the item's own rows times the clauses' selectivity, which for a subquery is
-- cost_subqueryscan's (restriction estimators over two relations' Vars, cached in the
-- RestrictInfo); a LATERAL subquery with a LATERAL item of its own is pulled up within
-- from_collapse_limit; a Function Scan costs its arguments; equivalence class members come
-- in process_equivalence's order (the clauses as written); semi joins carry their class
-- members' widths; a whole-row-marked subquery's unread columns aren't passed up;
-- deterministic collations make classes of their own.
create table a46_i8 (q1 int8, q2 int8) with (autovacuum_enabled = false);
insert into a46_i8 values (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
vacuum a46_i8;
create table a46_big (q1 int8, q2 int8) with (autovacuum_enabled = false);
insert into a46_big select g, g % 7 from generate_series(1, 100) g;
vacuum a46_big;
create table a46_k (q1 int8, q2 int8) with (autovacuum_enabled = false);
insert into a46_k select g, g % 7 from generate_series(1, 1000) g;
vacuum a46_k;

-- LATERAL subqueries: the join relation's rows.
explain select * from a46_i8 t1, lateral (select t2.*, t1.q2 as z from a46_i8 t2 offset 0) ss where t1.q1 = ss.q2;
explain select * from a46_i8 t1, lateral (select t2.*, t1.q2 as z from a46_i8 t2 offset 0) ss where t1.q1 = ss.q2 and t1.q2 = ss.q1;
explain select * from a46_i8 t1, lateral (select t2.*, t1.q2 as z from a46_big t2 offset 0) ss where t1.q1 < ss.q2;
explain select * from a46_i8 t1, lateral (select * from a46_big t2 where t2.q1 > t1.q2 offset 0) ss where t1.q1 = ss.q2;
explain select * from a46_i8 t1, lateral (select t2.*, t1.q2 as z from a46_k t2 offset 0) ss where t1.q1 = ss.q2 and t1.q2 < ss.q1;
explain select * from a46_i8 t1, lateral (select t2.*, t1.q2 as z from a46_k t2 offset 0) ss where t1.q1 = ss.q2 or t1.q2 = ss.q1;
explain select * from a46_i8 t1, lateral generate_series(1, t1.q2::int % 1000) g where t1.q1 = g;
-- join 783's shape: the LATERAL subquery over a join with a LATERAL item of its own is
-- pulled up, t1 outside the parameterized join of the VALUES and ss2.
explain (verbose)
select * from (values (0), (1)) v(id),
lateral (select * from a46_i8 t1,
         lateral (select * from
                    (select * from a46_i8 t2
                     where (q1, random() > 0) = any (select q2, random() > 0 from a46_i8 t3
                                     where q2 = (select greatest(t1.q1,t2.q2))
                                       and (select v.id=0))
                     offset 0) ss2) ss
         where t1.q1 = ss.q2) ss0;
select * from (values (0), (1)) v(id),
lateral (select * from a46_i8 t1,
         lateral (select * from
                    (select * from a46_i8 t2
                     where (q1, random() > 0) = any (select q2, random() > 0 from a46_i8 t3
                                     where q2 = (select greatest(t1.q1,t2.q2))
                                       and (select v.id=0))
                     offset 0) ss2) ss
         where t1.q1 = ss.q2) ss0
order by 1, 2, 3, 4, 5;
-- Over from_collapse_limit: the subquery's items a join problem of their own.
begin;
set local from_collapse_limit to 2;
explain (verbose, costs off)
select * from a46_i8 t1
  left join
    (select coalesce(t2.q1 + x, 0) from a46_i8 t2,
       lateral (select t3.q1 as x from a46_i8 t3,
                  lateral (select t2.q1, t3.q1 offset 0) s))
  on true;
rollback;
-- Rows of LATERAL subqueries with LATERAL items of their own.
create table a46_i4 (f1 int4) with (autovacuum_enabled = false);
insert into a46_i4 values (0), (123456), (-123456), (2147483647), (-2147483647);
select * from a46_i4 a, lateral (select b.f1 as x, c.z as y from a46_i4 b,
  lateral (select a.f1 % 7 + b.f1 % 5 as z offset 0) c where b.f1 > 0) s
  where s.x = a.f1 order by 1, 2, 3;
select * from a46_i4 a left join lateral (select b.f1 as x, c.z as y from a46_i4 b,
  lateral (select a.f1 % 3 - b.f1 % 3 as z offset 0) c where b.f1 >= 0) s
  on s.x <> a.f1 order by 1, 2, 3;
select * from a46_i8 t1
  left join
    (select coalesce(t2.q1 + x, 0) from a46_i8 t2,
       lateral (select t3.q1 as x from a46_i8 t3,
                  lateral (select t2.q1, t3.q1 offset 0) s))
  on true order by 1, 2, 3;

-- Equivalence class members in the order the clauses as written made them.
create table a46_t1 (a int, b int) with (autovacuum_enabled = false);
create table a46_t2 (x int, y int) with (autovacuum_enabled = false);
create table a46_t3 (p int, q int) with (autovacuum_enabled = false);
insert into a46_t1 select g, g from generate_series(1, 1000) g;
insert into a46_t2 select g, g from generate_series(1, 100) g;
insert into a46_t3 select g, g from generate_series(1, 10) g;
analyze a46_t1, a46_t2, a46_t3;
explain (costs off) select * from a46_t1 t1, a46_t2 t2, a46_t3 t3 where t2.x = t1.a and t3.p = t1.a;
explain (costs off) select * from a46_t1 t1, a46_t2 t2, a46_t3 t3 where t3.p = t1.a and t2.x = t1.a;
explain (costs off) select * from a46_t1 t1 join a46_t2 t2 on t2.x = t1.a join a46_t3 t3 on t3.p = t1.a;
explain (costs off) select * from a46_t3 t3, a46_t2 t2, a46_t1 t1 where t2.x = t1.a and t3.p = t1.a;
-- Semi joins: the class's members across them, the join relation's size by the member
-- chosen, the right side's member carried in the width.
explain select * from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and t1.a in (select x from a46_t2 t2);
explain select t3.q from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and t1.a in (select x from a46_t2 t2 where y < 50);
explain select t1.b from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and exists (select 1 from a46_t2 t2 where t2.x = t3.p and t2.y = t1.b);
set enable_hashjoin = off;
explain select t3.q from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and t1.a in (select x from a46_t2 t2 where y < 50);
set enable_mergejoin = off;
explain select * from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and t1.a in (select x from a46_t2 t2);
explain (costs off) select 1 from a46_t1 t1 where exists (select 1 from a46_t2 t2, a46_t3 t3 where t2.x = t1.a and t3.p = t2.x);
explain (costs off) select 1 from a46_t1 t1 where exists (select 1 from a46_t2 t2, a46_t3 t3 where t2.x = t1.a and t3.q = t2.y);
explain (costs off) select 1 from a46_t1 t1, a46_t2 t2 where t2.x = t1.a;
reset enable_hashjoin;
reset enable_mergejoin;
select count(*) from a46_t1 t1, a46_t3 t3 where t3.p = t1.a and t1.a in (select x from a46_t2 t2 where y < 50);

-- A hash join on an expression key: the key's distinct values its table's tuples.
set enable_mergejoin = off;
set enable_nestloop = off;
explain select * from a46_t3 t1, a46_t2 t2 where t1.p = t2.x + 0;
explain select * from a46_t3 t1, a46_t2 t2 where t1.p = (t2.x + t2.y) / 2;
reset enable_mergejoin;
reset enable_nestloop;

-- A whole-row-marked subquery under a Sort or Materialize: only what is read.
create table a46_jt1 (a int, b int) with (autovacuum_enabled = false);
create table a46_jt2 (x int, y int, z int) with (autovacuum_enabled = false);
insert into a46_jt1 select g, g from generate_series(1, 10) g;
insert into a46_jt2 select g, g, g from generate_series(1, 10) g;
analyze a46_jt1, a46_jt2;
set enable_hashjoin = off;
explain (verbose, costs off) select a46_jt1.a, s.x from a46_jt1, (select * from a46_jt2 offset 0) s where a46_jt1.a = s.x for update;
set enable_mergejoin = off;
explain (verbose, costs off) select a46_jt1.a, s.x from a46_jt1, (select * from a46_jt2 offset 0) s where a46_jt1.a = s.x for update;
reset enable_hashjoin;
reset enable_mergejoin;

-- Deterministic collations: a class each.
create table a46_ca (x text) with (autovacuum_enabled = false);
create table a46_cb (x text) with (autovacuum_enabled = false);
insert into a46_ca select 'a' || g from generate_series(1, 100) g;
insert into a46_cb select 'a' || g from generate_series(1, 100) g;
analyze a46_ca, a46_cb;
explain (costs off) select * from a46_ca a, a46_cb b where a.x = b.x and a.x = 'a1' collate "C";
explain (costs off) select * from a46_ca a, a46_cb b where a.x = b.x and a.x = 'a1';
explain (costs off) select * from a46_ca a, a46_cb b where a.x = b.x collate "C" and a.x = 'a1';
explain (costs off) select * from a46_ca a, a46_cb b where a.x = b.x collate "POSIX" and b.x = 'a1' collate "C";
select * from a46_ca a, a46_cb b where a.x = b.x and a.x = 'a1' collate "C";
select * from a46_ca a, a46_cb b where a.x = b.x collate "C" and a.x = 'a1';
