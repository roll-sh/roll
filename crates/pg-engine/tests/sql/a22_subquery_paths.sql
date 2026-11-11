-- A FROM subquery's final paths (set_subquery_pathlist): the join search may take its
-- sorted grouping or DISTINCT (over an index scan, or its input sorted) beside the
-- cheapest hashed one.
begin;
create temp table sp_b (id int primary key, c_id int);
create temp table sp_d (a int, b int);
insert into sp_b values (0, 0), (1, null);
insert into sp_d values (1, 3), (2, 2), (3, 1);
explain (costs off)
select sp_d.* from sp_d left join (select * from sp_b group by sp_b.id, sp_b.c_id) s
  on sp_d.a = s.id;
select sp_d.*, s.* from sp_d left join (select * from sp_b group by sp_b.id, sp_b.c_id) s
  on sp_d.a = s.id;
explain (costs off)
select sp_d.* from sp_d left join (select distinct * from sp_b) s
  on sp_d.a = s.id;
select sp_d.*, s.* from sp_d left join (select distinct * from sp_b) s
  on sp_d.a = s.id;
explain (costs off)
select s.id, sp_d.b from (select * from sp_b group by sp_b.id, sp_b.c_id) s
  join sp_d on sp_d.a = s.id order by s.id;
select s.id, sp_d.b from (select * from sp_b group by sp_b.id, sp_b.c_id) s
  join sp_d on sp_d.a = s.id order by s.id;
rollback;

create table sp_t (g int, h int, v int);
insert into sp_t select i % 50, i % 7, i from generate_series(1, 5000) i;
create index on sp_t (g);
create table sp_u (g int, w int);
insert into sp_u select i % 60, i from generate_series(1, 3000) i;
analyze sp_t, sp_u;
explain (costs off)
select * from (select distinct g, h from sp_t) s join sp_u u on u.g = s.g
  where u.w < 40 order by s.g, s.h, u.w;
select * from (select distinct g, h from sp_t) s join sp_u u on u.g = s.g
  where u.w < 40 order by s.g, s.h, u.w;
set enable_hashjoin = off;
explain (costs off)
select s.g, s.n, u.w from (select g, count(*) n from sp_t group by g) s
  left join sp_u u on u.g = s.g and u.w < 70;
select s.g, s.n, u.w from (select g, count(*) n from sp_t group by g) s
  left join sp_u u on u.g = s.g and u.w < 70;
reset enable_hashjoin;
drop table sp_t, sp_u;
