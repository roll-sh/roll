-- Round 16, agent 26: join_collapse_limit keeping JOIN syntax's tree, self-join elimination
-- run again while it removes relations, neqjoinsel, clauseless joins of a relation without
-- join clauses.
create table a26_sj (a int unique, b int, c int unique);
insert into a26_sj values (1, null, 2), (null, 2, null), (2, 1, 1);
analyze a26_sj;
create table a26_sk (a int, b int);
create index on a26_sk (a);
set enable_hashjoin = off;
set enable_mergejoin = off;
-- (join_collapse_limit = 1: the explicit joins' tree, `(k1 ⋈ k2) ⋈ j2` once j1 is removed.)
set join_collapse_limit = 1;
set enable_seqscan = off;
explain (costs off) select 1 from
  (a26_sk k1 join a26_sk k2 on k1.a = k2.a)
  join (a26_sj j1 join a26_sj j2 on j1.a = j2.a) on j1.b = k1.b;
explain (costs off) select 1 from
  (a26_sk k1 join a26_sk k2 on k1.a = k2.a)
  join (a26_sj j1 join a26_sj j2 on j1.a = j2.a) on j2.b = k1.b;
reset enable_seqscan;
-- (A FROM list is folded within from_collapse_limit, not join_collapse_limit.)
explain (costs off) select 1 from a26_sk k1, a26_sk k2, a26_sj j1
  where k1.a = k2.a and j1.b = k1.b;
reset join_collapse_limit;
insert into a26_sk select i % 7, i % 5 from generate_series(1, 40) i;
analyze a26_sk;
select count(*) from
  (a26_sk k1 join a26_sk k2 on k1.a = k2.a)
  join (a26_sj j1 join a26_sj j2 on j1.a = j2.a) on j1.b = k1.b;
set join_collapse_limit = 1;
select count(*) from
  (a26_sk k1 join a26_sk k2 on k1.a = k2.a)
  join (a26_sj j1 join a26_sj j2 on j1.a = j2.a) on j1.b = k1.b;
reset join_collapse_limit;
-- Self-join elimination runs the group again while a pass removes a relation: c2 goes into
-- c3, then the expression clause pairs c1 with c3 on a unique expression index.
create table a26_emp (id serial primary key not null, code int);
create unique index on a26_emp ((id * id));
insert into a26_emp (code) select i from generate_series(1, 10) i;
analyze a26_emp;
explain (costs off)
select count(*) from a26_emp c1, a26_emp c2, a26_emp c3
  where c3.id = c2.id and c3.id * c2.id = c1.id * c1.id;
select count(*) from a26_emp c1, a26_emp c2, a26_emp c3
  where c3.id = c2.id and c3.id * c2.id = c1.id * c1.id;
-- neqjoinsel: one minus the negator's eqjoinsel; a relation without join clauses joins
-- every other relation at the first level (make_rels_by_clauseless_joins).
insert into a26_sj values (3, 1, 3);
create table a26_sl (a int, b int, c int);
create unique index on a26_sl (a, b);
vacuum analyze a26_sl;
explain select * from a26_sj n1, a26_sj n2 where n1.a <> n2.a;
explain select * from a26_sj n1 where exists (select 1 from a26_sj n2 where n1.a <> n2.a);
explain select * from a26_sj n1 where not exists (select 1 from a26_sj n2 where n1.a <> n2.a);
explain (costs off)
select * from (select n2.a from a26_sj n1, a26_sj n2 where n1.a <> n2.a) q0, a26_sl
  where q0.a = 1;
select * from (select n2.a from a26_sj n1, a26_sj n2 where n1.a <> n2.a) q0, a26_sl
  where q0.a = 1;
reset enable_hashjoin;
reset enable_mergejoin;
drop table a26_sj, a26_sk, a26_emp, a26_sl;
