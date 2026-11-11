-- Partitions read in their order under run-time pruning: the members pruned as the executor
-- starts leave their places empty (each level counting its own), a Merge Append whose first
-- member went names its keys unqualified; a window over an inheritance tree reads it in
-- its order when that costs about as much and starts sooner (add_path's fuzzy costs); a
-- UNION ALL arm's Subquery Scan kept for its window keys (resjunk), named after the
-- appendrel's parent (`*SELECT* 1_1`).
create function a12_stable_one() returns int as $$ begin return 1; end; $$ language plpgsql stable;
create table a12_pa (a int, b text, c bool) partition by list (a);
create table a12_pa_1 (b text, a int, c bool);
create table a12_pa_2 (a int, c bool, b text);
alter table a12_pa attach partition a12_pa_1 for values in (1);
alter table a12_pa attach partition a12_pa_2 for values in (2);
insert into a12_pa values (1, 'b', true);
insert into a12_pa values (2, 'c', true);
create index on a12_pa (a);
alter table a12_pa add d int;
create table a12_pa_3 partition of a12_pa for values in (3, 4) partition by range (d);
create table a12_pa_3_1 partition of a12_pa_3 for values from (minvalue) to (1);
create table a12_pa_3_2 partition of a12_pa_3 for values from (1) to (100);
create table a12_pa_3_3 partition of a12_pa_3 for values from (100) to (maxvalue);
explain (costs off)
select min(a) over (partition by a order by a) from a12_pa where a >= 2;
explain (costs off)
select a from a12_pa where a >= a12_stable_one() + 1 order by a;
explain (costs off)
select a from a12_pa where a >= a12_stable_one() + 1 and d <= a12_stable_one() order by a;
explain (costs off)
select a from a12_pa where a >= a12_stable_one() + 1 and d >= a12_stable_one() order by a;
explain (costs off)
select min(a) over (partition by a order by a) from a12_pa where a >= 2
union all
select min(a) over (partition by a order by a) from a12_pa where a >= 3;
explain (costs off)
select min(a) over (partition by a order by a) from a12_pa where a >= a12_stable_one() + 1 and d <= a12_stable_one()
union all
select min(a) over (partition by a order by a) from a12_pa where a >= a12_stable_one() + 1 and d >= a12_stable_one();
insert into a12_pa values (3, 'x', true, 0), (4, 'y', false, 50), (3, 'z', true, 500), (2, 'w', false, 1);
select a, min(a) over (partition by a order by a) from a12_pa where a >= a12_stable_one() + 1 and d <= a12_stable_one() order by 1;
select a from a12_pa where a >= a12_stable_one() + 1 and d >= a12_stable_one() order by a;
select min(a) over (partition by a order by a) from a12_pa where a >= a12_stable_one() + 1 and d <= a12_stable_one()
union all
select min(a) over (partition by a order by a) from a12_pa where a >= a12_stable_one() + 1 and d >= a12_stable_one();
drop function a12_stable_one();
