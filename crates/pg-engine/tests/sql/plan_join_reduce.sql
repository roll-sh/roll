-- Planner join simplifications: outer joins reduced to inner / anti joins by strict
-- quals above them (reduce_outer_joins), left joins to unique inner sides nothing reads
-- removed (remove_useless_joins), self joins on a unique key folded into one scan
-- (remove_useless_self_joins), constant-false joins proven empty, NOT IN over constants.
create table jr_a (id int primary key, v int, w int);
create table jr_b (id int primary key, a_id int, t text);
create table jr_c (k int, x int);
insert into jr_a select g, g % 10, g % 7 from generate_series(1, 200) g;
insert into jr_b select g, g % 150, 't' || g from generate_series(1, 300) g;
insert into jr_c select g % 20, g from generate_series(1, 100) g;
analyze jr_a; analyze jr_b; analyze jr_c;
explain (costs off) select * from jr_a a left join jr_b b on a.id = b.a_id where b.t = 't5';
explain (costs off) select a.* from jr_a a left join jr_b b on a.id = b.a_id where b.a_id is null;
explain (costs off) select a.* from jr_a a left join jr_b b on a.id = b.a_id and b.id > 10 where b.id is null;
explain (costs off) select * from jr_a a full join jr_c c on a.v = c.k where c.x > 5;
explain (costs off) select * from jr_a a full join jr_c c on a.v = c.k where a.w > 5 and c.x > 5;
explain (costs off) select * from jr_a a right join jr_c c on a.v = c.k where a.w = 1;
explain (costs off) select * from jr_a a left join jr_c c on a.v = c.k join jr_b b on b.id = c.x;
explain (costs off) select a.id from jr_a a left join jr_b b on a.id = b.id;
explain (costs off) select a.id from jr_a a left join (select distinct k from jr_c) c on a.v = c.k;
explain (costs off) select a1.id, a2.w from jr_a a1 join jr_a a2 on a1.id = a2.id where a1.v = 3;
explain (costs off) select * from jr_a a join jr_c c on a.v = c.k and false;
explain (costs off) select * from jr_a a left join jr_c c on a.v = c.k and false;
explain (costs off) select * from jr_a where v not in (1, 2, 3);
select count(*), sum(a.id) from jr_a a left join jr_b b on a.id = b.a_id where b.a_id is null;
select count(*) from jr_a a left join jr_b b on a.id = b.a_id and b.id > 10 where b.id is null;
select count(*), sum(c.x) from jr_a a full join jr_c c on a.v = c.k where c.x > 5;
select count(*) from jr_a a right join jr_c c on a.v = c.k where a.w = 1;
select count(*) from jr_a a left join jr_c c on a.v = c.k join jr_b b on b.id = c.x;
select count(*) from jr_a a1 join jr_a a2 on a1.id = a2.id where a1.v = 3;
