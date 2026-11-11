-- Round 16 (agent 28): the join search's enumeration (join_search_one_level,
-- make_rels_by_clause_joins, make_rels_by_clauseless_joins): equivalence classes with a
-- constant still make their relations "relevant" to each other, an equality replaced by
-- constants is still the outer join's clause (min_lefthand), clauseless joins at level 2
-- both ways round.
create table a28_t (a int unique, b int);
insert into a28_t values (1, 2);

-- The semi join's min_lefthand has t1 (its `t1.a = t4.a`, though the class with `t4.a = 1`
-- replaced it), so the five-relation join is sized from {t1, unique(t4 t5 t6)} first.
explain (costs off)
select t1.a from a28_t t1
  left join a28_t t2 on t1.a = t2.a
       join a28_t t3 on true
where exists (select 1 from a28_t t4
                join a28_t t5 on t4.b = t5.b
                join a28_t t6 on t5.b = t6.b
              where t1.a = t4.a and t3.a = t5.a and t4.a = 1);
select t1.a from a28_t t1
  left join a28_t t2 on t1.a = t2.a
       join a28_t t3 on true
where exists (select 1 from a28_t t4
                join a28_t t5 on t4.b = t5.b
                join a28_t t6 on t5.b = t6.b
              where t1.a = t4.a and t3.a = t5.a and t4.a = 1);

create table a28_sj (a int unique, b int, c int);
insert into a28_sj values (2, 1, null), (null, 2, null), (1, 3, 1);
create table a28_sl (a int, b int, c int);
insert into a28_sl values (1, 1, 1), (2, 2, 2);

-- A class with a constant (`n2.a = 1`) over a pulled-up subquery: n2 is joined first.
explain (costs off)
select * from
  (select n2.a from a28_sj n1, a28_sj n2 where n1.a <> n2.a) q0, a28_sl
where q0.a = 1;
select * from
  (select n2.a from a28_sj n1, a28_sj n2 where n1.a <> n2.a) q0, a28_sl
where q0.a = 1;

-- Inner joins only: `x.a = 1 and x.a = y.a` relates x and y (a cartesian product of two
-- one-row scans) though no clause is left between them.
create table a28_big (id int primary key, a int, pad text);
insert into a28_big select i, i % 50, repeat('x', 20) from generate_series(1, 2000) i;
vacuum analyze a28_big;
explain
select * from a28_big x, a28_big y, a28_big z
where x.id = 1 and x.id = y.id and y.a = z.a;
select count(*) from a28_big x, a28_big y, a28_big z
where x.id = 1 and x.id = y.id and y.a = z.a;

-- Two items of an outer join without a join clause: its left side stays outside.
explain (costs off)
select t1.a from a28_sj t1 left join a28_sj t2 on t2.a = 1 where t2.a is null;
select t1.a from a28_sj t1 left join a28_sj t2 on t2.a = 1 where t2.a is null;
explain (costs off)
select * from a28_sl s right join (values (1), (2), (3)) v(x) on true;
select * from a28_sl s right join (values (1), (2), (3)) v(x) on true order by 1, 2, 3, 4;

-- A group whose outer joins are all semi joins is one join domain: the semi joins' clauses
-- are equivalence class members, so `sc.x = sb.x` joins the unique-ified side to sb.
create table a28_sa (x int, y int);
create table a28_sb (x int primary key, y int);
create table a28_sc (x int, z int);
insert into a28_sa select i, i from generate_series(1, 1000) i;
insert into a28_sb select i, i from generate_series(1, 100) i;
insert into a28_sc select i % 10, i from generate_series(1, 500) i;
create index on a28_sc (x);
analyze a28_sa;
analyze a28_sb;
analyze a28_sc;
explain select * from a28_sa join a28_sb on a28_sa.x = a28_sb.x
  where exists (select 1 from a28_sc where a28_sc.x = a28_sa.x);
explain select * from a28_sa join a28_sb on a28_sa.x = a28_sb.x
  where exists (select 1 from a28_sc where a28_sc.x = a28_sb.x and a28_sc.z > 5);
explain select * from a28_sa join a28_sb on a28_sa.y = a28_sb.y
  where a28_sa.x in (select x from a28_sc) and a28_sb.x in (select x from a28_sc);
select count(*) from a28_sa join a28_sb on a28_sa.x = a28_sb.x
  where exists (select 1 from a28_sc where a28_sc.x = a28_sb.x and a28_sc.z > 5);
select count(*) from a28_sa join a28_sb on a28_sa.y = a28_sb.y
  where a28_sa.x in (select x from a28_sc) and a28_sb.x in (select x from a28_sc);
