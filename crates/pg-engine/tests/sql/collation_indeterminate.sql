-- Values whose implicit collations conflict have none: sorting them fails as the sort
-- starts (tuplesort sets up its comparisons before reading a row), hashing fails at the
-- first value hashed, min / max at their first comparison; EXPLAIN shows equalities from
-- equivalence classes with the sides relabelled.
create table ci1 (a text collate "C", x int);
create table ci2 (a text collate "POSIX", y int);
create table ci3 (a text collate "C", b text collate "POSIX");
select * from ci1 full join ci2 using (a) order by a;
select a, count(*) from ci1 full join ci2 using (a) group by a;
select distinct a from ci1 full join ci2 using (a);
select max(a) from ci1 full join ci2 using (a);
select a, b from ci1 x(a, b) union all select a, y from ci2 order by 1;
select a, b from ci1 x(a, b) union all select a, y from ci2 order by 2;
select b, count(*) from (select a, x from ci1 union all select a, y from ci2) s(b, c) group by b;
insert into ci1 values ('x', 1);
insert into ci2 values ('x', 1), ('y', 2);
select * from ci1 full join ci2 using (a) order by a;
select a, count(*) from ci1 full join ci2 using (a) group by a;
select distinct a from ci1 full join ci2 using (a);
select max(a) from ci1 full join ci2 using (a);
select min(a) from ci1 full join ci2 using (a);
select b, count(*) from (select a, x from ci1 union all select a, y from ci2) s(b, c) group by b;
insert into ci3 values ('a', 'a'), ('b', 'b');
select max(a || b) from ci3;
select max(a), min(b), max(a collate "POSIX") from ci3;
explain (costs off) select * from ci3 x where x.a = x.b;
set enable_hashjoin = off;
set enable_mergejoin = off;
explain (costs off) select * from ci1 join ci2 using (a);
explain (costs off) select * from ci1 left join ci2 using (a);
explain (costs off) select * from ci3 x join ci3 y on x.a = y.b;
reset enable_hashjoin;
reset enable_mergejoin;
