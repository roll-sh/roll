-- Explicit ROW casts and child-to-ancestor row conversions (ConvertRowtypeExpr), as
-- values and as stored definitions show them.
create table rc_p (a int, b int);
create table rc_c (x int) inherits (rc_p);
create table rc_d (y int) inherits (rc_c);
insert into rc_c values (1, 2, 3);
insert into rc_d values (4, 5, 6, 7);
select c::rc_p, pg_typeof(c::rc_p), (c::rc_p).b from rc_c c order by 1;
select d::rc_c::rc_p, (d::rc_c).x from rc_d d;
select row(1,2)::rc_p, pg_typeof(row(1,2)::rc_p), (row(1,2)::rc_p).a;
select row(a, b)::rc_p = '(1,2)'::rc_p from rc_p order by a;
select row(1, 2, 3)::rc_p;
select row(1)::rc_p;
create view rc_v1 as select row(a, b)::rc_p as r, rc_p as w from rc_p;
select pg_get_viewdef('rc_v1');
create view rc_v2 as select c::rc_p as conv, (c::rc_p).a, d::rc_c::rc_p as conv2 from rc_c c, rc_d d;
select pg_get_viewdef('rc_v2');
select * from rc_v2;
create function rc_f(rc_p) returns int as 'select $1.a + $1.b' language sql;
select rc_f(c) from rc_c c;
create view rc_v3 as select rc_f(c) from rc_c c;
select pg_get_viewdef('rc_v3');
-- IS [NOT] DISTINCT FROM NULL is a null test (on a composite: of the value itself)
create view rc_v4 as select a is distinct from null as x, a is not distinct from null as y, null is distinct from b as z, c is distinct from null as w, a is distinct from null::int as v from rc_c c;
select pg_get_viewdef('rc_v4');
select * from rc_v4;
select row(null, null)::rc_p is distinct from null, row(null, null)::rc_p is null, null::rc_p is distinct from null;
-- partition keys over the row
create table rc_pt (a int, b int) partition by list ((rc_pt));
create table rc_pt1 partition of rc_pt for values in ('(1,2)');
create table rc_pt2 partition of rc_pt for values in (null);
select pg_get_partition_constraintdef('rc_pt1'::regclass), pg_get_partition_constraintdef('rc_pt2'::regclass);
insert into rc_pt values (1, 2), (null, null);
insert into rc_pt values (2, 2);
select tableoid::regclass, * from rc_pt order by 1, 2;
create table rc_pr (a int, b int) partition by list ((row(a, b)::rc_pr));
create table rc_pr1 partition of rc_pr for values in ('(1,2)'::rc_pr);
select pg_get_partition_constraintdef('rc_pr1'::regclass), pg_get_partkeydef('rc_pr'::regclass), pg_get_partkeydef('rc_pt'::regclass);
insert into rc_pr values (1, 2);
insert into rc_pr values (1, 3);
select * from rc_pr where row(a, b)::rc_pr = '(1,2)'::rc_pr;
explain (costs off) select * from rc_pr where row(a, b)::rc_pr = '(1,2)'::rc_pr;
drop table rc_pt, rc_pr;
drop view rc_v1, rc_v2, rc_v3, rc_v4;
drop table rc_p cascade;
