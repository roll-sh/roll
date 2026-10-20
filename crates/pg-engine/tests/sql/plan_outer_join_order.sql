-- Join order search over trees with outer joins (make_outerjoininfo / join_is_legal):
-- outer joins commute with inner joins and each other where the identities allow,
-- pushed-down WHERE clauses wait for the outer joins that null their columns, and
-- nested correlated EXISTS / NOT EXISTS become semi / anti joins.
create table oj_a (id int primary key, x int);
create table oj_b (id int primary key, a_id int, y int);
create table oj_c (id int primary key, b_id int, z int);
create table oj_d (id int, w int);
insert into oj_a select g, g % 13 from generate_series(1, 1000) g;
insert into oj_b select g, g % 1000 + 1, g % 17 from generate_series(1, 3000) g;
insert into oj_c select g, g % 3000 + 1, g % 19 from generate_series(1, 6000) g;
insert into oj_d select g % 50, g from generate_series(1, 200) g;
analyze oj_a; analyze oj_b; analyze oj_c; analyze oj_d;
explain (costs off) select * from oj_a a left join oj_b b on b.a_id = a.id left join oj_c c on c.b_id = b.id where a.x = 1;
explain (costs off) select * from oj_a a left join (oj_b b join oj_c c on c.b_id = b.id) on b.a_id = a.id where a.x = 2;
explain (costs off) select * from oj_a a left join oj_b b on b.a_id = a.id where coalesce(b.y, 0) = 3;
select count(*), sum(c.z) from oj_a a left join oj_b b on b.a_id = a.id left join oj_c c on c.b_id = b.id where a.x = 1;
select count(*) from oj_a a left join oj_b b on b.a_id = a.id where coalesce(b.y, 0) = 3;
select count(*) from oj_d d left join oj_a a on a.id = d.id left join oj_b b on b.a_id = a.id and b.y = d.w;
select count(*) from oj_a a where exists (select 1 from oj_b b where b.a_id = a.id and not exists (select 1 from oj_c c where c.b_id = b.id and c.z = 0));
