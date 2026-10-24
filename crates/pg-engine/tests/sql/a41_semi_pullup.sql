-- Round 16 (agent 41): a FROM subquery whose WHERE has an EXISTS / IN (a semi join, or a
-- NOT EXISTS anti join, pull_up_sublinks makes them before pull_up_subqueries) is pulled
-- up as its level had it before planning, its semi joins joining the query's join tree;
-- their equalities are class members as written (process_equivalence); a class clause
-- equates the first member each input returns (a semi join's right side, unique-ified,
-- returns its columns); check_index_only reads the scan's reltarget.
create table a41_t1 (id serial, a int);
create table a41_t2 (id serial, a int);
create table a41_t3 (id serial, a int);
create table a41_t4 (id serial, a int);
create unique index on a41_t3 using btree (a, id);
create unique index on a41_t2 using btree (id);
set enable_hashjoin to off;
set enable_mergejoin to off;

-- Two pulled-up subqueries, each with an EXISTS: one join problem of seven relations.
explain (costs off)
select * from a41_t1
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t3.id = a41_t2.id)) s1
  on a41_t1.id = s1.id
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t3.id = a41_t2.id)) s2
  on a41_t1.id = s2.id;
-- One of them; then the EXISTS's equality written the other way round.
explain (costs off)
select * from a41_t1
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t3.id = a41_t2.id)) s1
  on a41_t1.id = s1.id;
explain (costs off)
select * from a41_t1
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t2.id = a41_t3.id)) s1
  on a41_t1.id = s1.id;
insert into a41_t1 (id, a) select g, g from generate_series(1, 20) g;
insert into a41_t2 (id, a) select g, g from generate_series(1, 20, 2) g;
insert into a41_t3 (id, a) select g, g % 2 from generate_series(1, 20) g;
insert into a41_t4 (id, a) values (1, 1), (2, 2);
select * from a41_t1
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t3.id = a41_t2.id)) s1
  on a41_t1.id = s1.id
join (select a41_t2.id as id from a41_t2
      where exists (select true from a41_t3, a41_t4 where a41_t3.a = 1 and a41_t3.id = a41_t2.id)) s2
  on a41_t1.id = s2.id
order by 1;
-- A nested loop's inner side equating a column with its parameter (the outer side's
-- column) enforces a join clause: the column isn't a constant, and the ORDER BY over it
-- stays (pathkeys over a pulled-up subquery's join).
create table a41_o1 (id int, a int);
create table a41_o2 (id int, a int);
create table a41_o3 (id int, a int);
create unique index on a41_o2 using btree (id);
insert into a41_o1 select (g * 7) % 11, g from generate_series(1, 10) g;
insert into a41_o2 select g, g from generate_series(0, 10) g;
insert into a41_o3 select g, g from generate_series(0, 10) g;
explain (costs off)
select * from a41_o1
join (select a41_o2.id as id from a41_o2 join a41_o3 on a41_o3.id = a41_o2.id) s1
  on a41_o1.id = s1.id
order by 1;
select * from a41_o1
join (select a41_o2.id as id from a41_o2 join a41_o3 on a41_o3.id = a41_o2.id) s1
  on a41_o1.id = s1.id
order by 1;
explain (costs off)
select * from a41_o1
join (select a41_o2.id as id from a41_o2
      where exists (select 1 from a41_o3 where a41_o3.id = a41_o2.id)) s1
  on a41_o1.id = s1.id
order by 1;
select * from a41_o1
join (select a41_o2.id as id from a41_o2
      where exists (select 1 from a41_o3 where a41_o3.id = a41_o2.id)) s1
  on a41_o1.id = s1.id
order by 1;
reset enable_hashjoin;
reset enable_mergejoin;

create table a41_a (id int, x int);
create table a41_b (id int, y int);
create table a41_c (id int, z int);
create table a41_d (id int, w int);
insert into a41_a select g, g % 5 from generate_series(1, 30) g;
insert into a41_b select g, g % 3 from generate_series(1, 30, 2) g;
insert into a41_c select g % 10, g from generate_series(1, 20) g;
insert into a41_d select g, g % 4 from generate_series(5, 25) g;
insert into a41_a values (null, 1), (100, null);
insert into a41_c values (null, 7);
analyze a41_a; analyze a41_b; analyze a41_c; analyze a41_d;

-- Under the nullable side of a LEFT JOIN, NOT EXISTS, IN with a computed output, an outer
-- join of the subquery's own, RIGHT and FULL joins, two of them, a view.
explain (costs off)
select a41_a.id, s.id from a41_a
  left join (select a41_b.id from a41_b
             where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10 and a41_c.z > 3)) s
  on a41_a.id = s.id;
select a41_a.id, s.id from a41_a
  left join (select a41_b.id from a41_b
             where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10 and a41_c.z > 3)) s
  on a41_a.id = s.id
order by 1, 2;
explain (costs off)
select a41_a.id, s.id from a41_a
  left join (select a41_b.id from a41_b
             where not exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id;
select a41_a.id, s.id from a41_a
  left join (select a41_b.id from a41_b
             where not exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id
order by 1, 2;
explain (costs off)
select a41_a.id, s.id from a41_a
  join (select a41_b.id, a41_b.y from a41_b
        where not exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id;
select a41_a.id, s.id from a41_a
  join (select a41_b.id, a41_b.y from a41_b
        where not exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id
order by 1, 2;
explain (costs off)
select a41_a.id, s.k from a41_a
  join (select a41_b.id, a41_b.y + 1 as k from a41_b
        where a41_b.id in (select a41_d.id from a41_d where a41_d.w <> 2)) s
  on a41_a.id = s.id;
select a41_a.id, s.k from a41_a
  join (select a41_b.id, a41_b.y + 1 as k from a41_b
        where a41_b.id in (select a41_d.id from a41_d where a41_d.w <> 2)) s
  on a41_a.id = s.id
order by 1, 2;
explain (costs off)
select a41_a.id, s.id, s.w from a41_a
  join (select a41_b.id, a41_d.w from a41_b left join a41_d on a41_d.id = a41_b.id
        where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id;
select a41_a.id, s.id, s.w from a41_a
  join (select a41_b.id, a41_d.w from a41_b left join a41_d on a41_d.id = a41_b.id
        where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id
order by 1, 2, 3;
select a41_a.id, s.id from a41_a
  join (select a41_b.id, a41_d.id as did from a41_b left join a41_d on a41_d.id = a41_b.id
        where not exists (select 1 from a41_c where a41_c.z = a41_d.w)) s
  on a41_a.id = s.id
order by 1, 2;
select a41_a.id, s.id from a41_a
  right join (select a41_b.id from a41_b
              where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id + 1
order by 1, 2;
select a41_a.id, s.id from a41_a
  full join (select a41_b.id from a41_b
             where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
  on a41_a.id = s.id
order by 1, 2;
explain (costs off)
select a41_a.id from a41_a
  join (select a41_b.id from a41_b
        where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s on a41_a.id = s.id
  join (select a41_d.id from a41_d
        where exists (select 1 from a41_c where a41_c.z = a41_d.id)) t on a41_a.id = t.id;
select a41_a.id from a41_a
  join (select a41_b.id from a41_b
        where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s on a41_a.id = s.id
  join (select a41_d.id from a41_d
        where exists (select 1 from a41_c where a41_c.z = a41_d.id)) t on a41_a.id = t.id
order by 1;
create view a41_v as select a41_b.id, a41_b.y from a41_b
  where exists (select 1 from a41_c where a41_c.id = a41_b.y);
explain (costs off)
select a41_a.id, a41_v.y from a41_a, a41_v where a41_a.id = a41_v.id and a41_a.x < 3;
select a41_a.id, a41_v.y from a41_a, a41_v where a41_a.id = a41_v.id and a41_a.x < 3 order by 1, 2;
select count(*) from a41_a left join a41_v on a41_a.x = a41_v.y;
-- Under an EXISTS of the query, under a LATERAL reference, correlated to the query.
select a41_a.id from a41_a
where exists (select 1 from (select a41_b.id from a41_b
                             where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s
              where s.id = a41_a.id)
order by 1;
select a41_a.id, l.n from a41_a
  join (select a41_b.id from a41_b
        where exists (select 1 from a41_c where a41_c.id = a41_b.id % 10)) s on a41_a.id = s.id,
  lateral (select count(*) n from a41_d where a41_d.id > s.id) l
order by 1, 2;
select a41_a.id from a41_a
where a41_a.x in (select s.y from (select a41_b.y from a41_b
                                   where exists (select 1 from a41_c
                                                 where a41_c.id = a41_b.id % 10 and a41_c.z > a41_a.id)) s)
order by 1;

-- After self-join elimination the kept scan's reltarget has the removed scan's columns:
-- `tr.y` is still needed, so no index-only scan.
create table a41_phv (x int, y int primary key);
create index a41_phv_idx on a41_phv (x);
insert into a41_phv (x, y) select gs, gs from generate_series(1, 100) as gs;
vacuum analyze a41_phv;
set enable_hashjoin to off;
set enable_mergejoin to off;
explain (costs off, verbose)
select 1 from a41_phv t1 left join
  (select 1 extra, x, y from a41_phv tl) t3 join
    (select y from a41_phv tr) t4
  on t4.y = t3.y
on true where t3.extra is not null and t3.x = t1.x % 2;
select count(*) from a41_phv t1 left join
  (select 1 extra, x, y from a41_phv tl) t3 join
    (select y from a41_phv tr) t4
  on t4.y = t3.y
on true where t3.extra is not null and t3.x = t1.x % 2;
reset enable_hashjoin;
reset enable_mergejoin;
