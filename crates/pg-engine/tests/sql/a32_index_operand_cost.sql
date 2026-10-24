-- index_other_operands_eval_cost: an index condition's other operand is evaluated once per
-- scan (its cost in the index scan's startup), and a parameterized path's clauses are costed
-- as written (get_restriction_qual_cost over the ppi_clauses): a nested loop passes only the
-- outer Vars, the scan computes the rest. So a Bitmap Heap Scan rechecking `id > f1 + 1`
-- per row loses to an index scan that doesn't, while `id > f1` keeps the bitmap.
create table ioc_o (f1 int4);
insert into ioc_o values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze ioc_o;
create table ioc_p (id int primary key) partition by range (id);
create table ioc_p0 partition of ioc_p for values from (0) to (1);
create table ioc_p1 partition of ioc_p for values from (1) to (2);
insert into ioc_p values (0), (1);
create table ioc_t (id int primary key);
insert into ioc_t values (0);
explain (costs off)
select * from ioc_o, ioc_t where ioc_t.id > ioc_o.f1;
explain (costs off)
select * from ioc_o, ioc_t where ioc_t.id > ioc_o.f1 + 1;
explain (costs off)
select * from ioc_o, ioc_t where ioc_t.id > abs(ioc_o.f1) + 1;
select * from ioc_o, ioc_t where ioc_t.id > ioc_o.f1 - 1 order by 1, 2;
-- The costs themselves (an index scan's startup, a bitmap's per-row recheck).
explain
select * from ioc_o, ioc_t where ioc_t.id > abs(abs(abs(abs(ioc_o.f1)))) + 1 + 1 + 1 + 1 + 1 + 1;
set enable_bitmapscan = off;
explain
select * from ioc_o, ioc_t where ioc_t.id > abs(abs(abs(abs(ioc_o.f1)))) + 1 + 1 + 1 + 1 + 1 + 1;
set enable_indexscan = off;
set enable_bitmapscan = on;
explain
select * from ioc_o, ioc_t where ioc_t.id > abs(abs(abs(abs(ioc_o.f1)))) + 1 + 1 + 1 + 1 + 1 + 1;
reset enable_indexscan;
-- An appendrel's members: each one's cheapest path of the parameterization, a bitmap heap
-- scan among them (get_cheapest_parameterized_child_path).
explain (costs off)
select * from ioc_o, ioc_p where ioc_p.id > ioc_o.f1;
explain (costs off)
select * from ioc_o, ioc_p where ioc_p.id > ioc_o.f1 + 1;
select * from ioc_o, ioc_p where ioc_p.id > ioc_o.f1 order by 1, 2;
select * from ioc_o, ioc_p where ioc_p.id >= ioc_o.f1 - 1 order by 1, 2;
drop table ioc_o;
drop table ioc_p;
drop table ioc_t;
