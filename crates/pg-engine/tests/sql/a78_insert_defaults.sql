-- Round 21, agent 78, second batch: an INSERT's new row has every column (its defaults'
-- widths and costs in the source); a correlated NOT IN inside an IN's sub-select (rows).
create table a78_w (a int, b int default 5, c text, d serial, e numeric(10,2), f char(22));
create table a78_src as select g as k from generate_series(1, 100) g;
analyze a78_src;
explain insert into a78_w (a) values (1);
explain insert into a78_w (a) values (1), (2);
explain insert into a78_w (a) select 1;
explain insert into a78_w (c, a) select 'x', g from generate_series(1, 10) g;
explain insert into a78_w default values;
explain insert into a78_w (a) values (1) returning *;
explain insert into a78_w (a) select k from a78_src;
explain insert into a78_w values (1, 2, 'x', 3, 4.5, 'y');
explain (verbose, costs off) insert into a78_w (a, c) values (1, 'z');
insert into a78_w (a) select k from a78_src where k < 4;
select a, b, c, d, e, f from a78_w order by a;

-- `x IN (SELECT y ... WHERE y NOT IN (SELECT z ... WHERE z < outer.x))`: Postgres makes the
-- IN a semi join with the NOT IN its Join Filter, the engine keeps a SubPlan; the rows.
create table a78_a (x int);
create table a78_b (y int);
create table a78_d (z int);
insert into a78_a select g from generate_series(1, 10) g;
insert into a78_b select g from generate_series(1, 5) g;
insert into a78_d select g from generate_series(1, 3) g;
analyze a78_a, a78_b, a78_d;
select * from a78_a a where a.x in (select b.y from a78_b b
  where b.y not in (select d.z from a78_d d where d.z < a.x)) order by 1;
select * from a78_a a where a.x in (select b.y from a78_b b
  where b.y not in (select d.z from a78_d d where d.z < a.x - 2)) order by 1;
select * from a78_a a where a.x in (select b.y from a78_b b
  where exists (select 1 from a78_d d where d.z = b.y and d.z < a.x)) order by 1;

drop table a78_w, a78_src, a78_a, a78_b, a78_d;
