-- Round 16 (agent 16): join planning details (join 576, 606, 617-619, 876-880).
create table a16_sj (a int unique, b int, c int unique);
insert into a16_sj values (1, null, 2), (null, 2, null), (2, 1, 1);
analyze a16_sj;

-- A semi join's clauses over its left side alone are that side's restrictions, and its
-- equalities make classes (ojscope NULL for JOIN_SEMI): the constant reaches the right side.
create table a16_p (a int primary key, b int);
insert into a16_p select i, i from generate_series(1, 100) i;
analyze a16_p;
explain (costs off) select * from a16_p t1 where exists (select 1 from a16_p t2 where t1.b = t2.b and t1.b = 1);
explain (costs off) select * from a16_p t1 where exists (select 1 from a16_p t2 where t1.b = t2.b and t1.a < 5);
explain (costs off) select * from a16_p t1 where t1.b in (select t2.b from a16_p t2 where t1.a < 5);
select count(*) from a16_p t1 where exists (select 1 from a16_p t2 where t1.b = t2.b and t1.a < 5);
-- (An anti join's stay join clauses.)
explain (costs off) select * from a16_p t1 where not exists (select 1 from a16_p t2 where t1.b = t2.b and t1.a < 5);
select count(*) from a16_p t1 where not exists (select 1 from a16_p t2 where t1.b = t2.b and t1.a < 5);
-- No join clause left: the semi join's right side isn't unique for it, no self-join removal.
explain (costs off) select * from a16_p t1 where exists (select 1 from a16_p t2 where t1.a = t2.a and t1.a = 1);
create table a16_pp (a int, b int) partition by range (a);
create table a16_pp1 partition of a16_pp for values from (0) to (10);
create table a16_pp2 partition of a16_pp for values from (10) to (20);
insert into a16_pp select i, i from generate_series(0, 19) i;
explain (costs off) select * from a16_pp t1 where exists (select 1 from a16_pp t2 where t1.a = t2.a and t1.a = 1);
select * from a16_pp t1 where exists (select 1 from a16_pp t2 where t1.a = t2.a and t1.a = 1);

-- Self-join elimination with LATERAL items reading the removed relation: they read the
-- kept one.
explain (verbose, costs off)
select t3.a from a16_sj t1
	join a16_sj t2 on t1.a = t2.a
	join lateral (select t1.a offset 0) t3 on true;
explain (verbose, costs off)
select t4.a from a16_sj t1
	join a16_sj t2 on t1.a = t2.a
	join lateral (select t3.a from a16_sj t3, (select t1.a) offset 0) t4 on true;
select t3.a from a16_sj t1
	join a16_sj t2 on t1.a = t2.a
	join lateral (select t1.a offset 0) t3 on true order by 1;

-- A Memoize over a LATERAL scan: the join clauses its path checks first, then its own
-- LATERAL references (paraminfo_get_equal_hashops).
explain (costs off)
select * from a16_sj t1
    join lateral
      (select * from a16_sj tablesample system(t1.b)) s
    on t1.a = s.a;

-- A merge join's sorts are disabled nodes under enable_sort = off (cost_sort): presorted
-- index scans win, merging on the leading key.
create table a16_j1 (id1 int, id2 int, primary key(id1,id2));
create table a16_j2 (id1 int, id2 int, primary key(id1,id2));
insert into a16_j1 values(1,1),(1,2);
insert into a16_j2 values(1,1),(1,2);
analyze a16_j1;
analyze a16_j2;
set enable_nestloop to 0;
set enable_hashjoin to 0;
set enable_sort to 0;
create index a16_j1_id1_idx on a16_j1 (id1) where id1 % 1000 = 1;
create index a16_j2_id1_idx on a16_j2 (id1) where id1 % 1000 = 1;
explain (costs off) select * from a16_j1
inner join a16_j2 on a16_j1.id1 = a16_j2.id1 and a16_j1.id2 = a16_j2.id2
where a16_j1.id1 % 1000 = 1 and a16_j2.id1 % 1000 = 1;
select * from a16_j1
inner join a16_j2 on a16_j1.id1 = a16_j2.id1 and a16_j1.id2 = a16_j2.id2
where a16_j1.id1 % 1000 = 1 and a16_j2.id1 % 1000 = 1;
explain (costs off) select * from a16_j1 inner join a16_j2 on a16_j1.id1 = a16_j2.id1;
reset enable_nestloop;
reset enable_hashjoin;
reset enable_sort;
