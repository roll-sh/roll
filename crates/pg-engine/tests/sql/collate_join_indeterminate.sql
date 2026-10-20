-- Joins on text columns whose implicit collations conflict: the parse is accepted, the
-- comparison has no collation. A merge join fails as it starts (EXPLAIN too); a hash join
-- fails hashing a value; a nested loop fails comparing two values.
create table cji_c1 (a text collate "C");
create table cji_c2 (a text collate "POSIX");
create table cji_c3 (a text collate "C");
explain (costs off) select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 full join cji_c2 on cji_c1.a = cji_c2.a;
set enable_mergejoin = off;
explain (costs off) select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 join cji_c2 using (a);
reset enable_mergejoin;
insert into cji_c1 values ('x'), ('y');
insert into cji_c2 values ('x'), ('z');
insert into cji_c3 values ('x');
select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 full join cji_c2 using (a) order by 1;
select pg_collation_for(a) from cji_c1 full join cji_c2 using (a);
set enable_mergejoin = off;
select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 full join cji_c2 on cji_c1.a = cji_c2.a;
select * from cji_c1 left join cji_c2 using (a);
select * from cji_c1 join cji_c3 using (a) full join cji_c2 using (a);
set enable_hashjoin = off;
select * from cji_c1 join cji_c2 on cji_c1.a = cji_c2.a;
select * from cji_c1 join cji_c2 on cji_c1.a < cji_c2.a;
reset enable_hashjoin;
reset enable_mergejoin;
-- an explicit collation decides it
select * from cji_c1 c1 full join cji_c2 c2 on c1.a = c2.a collate "C" order by 1, 2;
select * from cji_c1 full join cji_c2 using (a) where false;
explain (costs off) select * from cji_c1 full join cji_c2 using (a) where false;
create view cji_v as select * from cji_c1 full join cji_c2 using (a);
create view cji_v2 as select c1.a from cji_c1 c1 join cji_c2 c2 on c1.a = c2.a;
select * from cji_v2;
-- NULL keys are never compared or hashed
delete from cji_c1; delete from cji_c2;
insert into cji_c1 values (null);
insert into cji_c2 values (null);
select * from cji_c1 full join cji_c2 using (a);
select * from cji_c1 left join cji_c2 using (a);
