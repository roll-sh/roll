-- Round 16 (agent 28): a scan's reltarget (build_base_rel_tlists, then the join clauses'
-- columns as distribute_qual_to_rels meets them: add_vars_to_targetlist). A scan returning
-- its reltarget (under a Hash, a Materialize, a locking level) lists the target list's
-- columns, then the join clauses' in that order, and keeps a column whose clause went
-- (self-join elimination, an equivalence class with a constant); a join lists each
-- relation's columns in that order.
create table a28_rt_sj (a int unique, b int, c int unique);
insert into a28_rt_sj values (1, null, 2), (null, 2, null), (2, 1, 1);
analyze a28_rt_sj;

-- Self-join elimination: the kept scan's reltarget takes the removed one's (`t3.c`, read
-- only by the removed join clause, stays).
explain (verbose, costs off)
select t1.a from a28_rt_sj t1 where t1.b in (
  select t2.b from a28_rt_sj t2 join a28_rt_sj t3 on t2.c = t3.c);
select t1.a from a28_rt_sj t1 where t1.b in (
  select t2.b from a28_rt_sj t2 join a28_rt_sj t3 on t2.c = t3.c);

-- An equality an equivalence class with a constant replaced (`t1.a = t4.a`, `t4.a = 1`):
-- t4's scan still returns t4.a; the classes' columns go up to where their clauses were.
create table a28_rt_t (a int unique, b int);
insert into a28_rt_t values (1, 2);
explain (verbose)
select t1.a from a28_rt_t t1
  left join a28_rt_t t2 on t1.a = t2.a
       join a28_rt_t t3 on true
where exists (select 1 from a28_rt_t t4
                join a28_rt_t t5 on t4.b = t5.b
                join a28_rt_t t6 on t5.b = t6.b
              where t1.a = t4.a and t3.a = t5.a and t4.a = 1);

-- The join clauses' order, not the columns': q before p.
create table a28_rt_m (p int, q int, r int);
create table a28_rt_n (x int, y int);
insert into a28_rt_m values (1, 2, 3);
insert into a28_rt_n values (2, 1);
explain (verbose, costs off)
select a28_rt_m.r from a28_rt_m join a28_rt_n on a28_rt_m.q = a28_rt_n.x and a28_rt_m.p = a28_rt_n.y;
explain (verbose, costs off)
select a28_rt_m.r from a28_rt_m join a28_rt_n on a28_rt_m.q = a28_rt_n.x and a28_rt_m.p = a28_rt_n.y
for update;
select a28_rt_m.r from a28_rt_m join a28_rt_n on a28_rt_m.q = a28_rt_n.x and a28_rt_m.p = a28_rt_n.y
for update;
begin;
set local enable_hashjoin = off;
set local enable_mergejoin = off;
explain (verbose, costs off)
select a28_rt_m.r from a28_rt_m join a28_rt_n on a28_rt_m.q = a28_rt_n.x and a28_rt_m.p = a28_rt_n.y;
-- An outer join's ON clause over one side still puts its columns in the reltarget.
explain (verbose, costs off)
select a28_rt_m.r from a28_rt_m left join a28_rt_n on a28_rt_n.y = 1 and a28_rt_m.q = a28_rt_n.x;
select a28_rt_m.r from a28_rt_m left join a28_rt_n on a28_rt_n.y = 1 and a28_rt_m.q = a28_rt_n.x;
commit;
