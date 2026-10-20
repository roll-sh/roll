-- golden: raw-order
-- Rows without ORDER BY come out in Postgres's physical order: sequential scans in heap
-- (TID) order after HOT and non-HOT updates, deletes, VACUUM and line pointer reuse;
-- index scans with equal keys in TID order; bitmap heap scans in TID order.
create table ro_a (id int, v text);
insert into ro_a select g, 'v' || g from generate_series(1, 10) g;
update ro_a set v = 'u' where id in (2, 5);
select * from ro_a;
delete from ro_a where id in (3, 4);
vacuum ro_a;
insert into ro_a values (100, 'n1'), (101, 'n2');
select * from ro_a;
create table ro_p (id int primary key, v text);
insert into ro_p values (5, 'a'), (1, 'b'), (3, 'c'), (2, 'd');
select * from ro_p;
update ro_p set v = 'x' where id = 1;
select * from ro_p;
begin;
update ro_p set v = 'y' where id = 3;
rollback;
select * from ro_p;
-- Heap layout under a fillfactor, then updates of an indexed column.
create table ro_t2 (id int primary key, v int, w text) with (fillfactor = 70);
insert into ro_t2 select g, 0, 'abc' from generate_series(1, 2000) g;
update ro_t2 set v = v + 1;
update ro_t2 set v = v + 1;
update ro_t2 set w = w || 'defgh' where id % 2 = 0;
select md5(string_agg(id::text, ',')) from ro_t2;
create index ro_t2_v on ro_t2 (v);
update ro_t2 set v = v + 1 where id <= 1000;
select pg_relation_size('ro_t2'), md5(string_agg(id::text, ',')) from ro_t2;
update ro_t2 set v = v + 1 where id <= 1000;
select pg_relation_size('ro_t2'), md5(string_agg(id::text, ',')) from ro_t2;
delete from ro_t2 where id % 3 = 0;
vacuum ro_t2;
insert into ro_t2 select g, 5, 'new' from generate_series(3000, 3100) g;
select pg_relation_size('ro_t2'), md5(string_agg(id::text, ',')) from ro_t2;
explain (costs off) select * from ro_t2 where v = 5;
select md5(string_agg(id::text, ',')) from ro_t2 where v = 5;
set enable_bitmapscan = off;
explain (costs off) select * from ro_t2 where v = 4;
select md5(string_agg(id::text, ',')) from ro_t2 where v = 4;
select id from ro_t2 where v = 4 order by v desc limit 5;
reset enable_bitmapscan;
-- A cursor over a table updated through it.
create table ro_c (a int, b text);
insert into ro_c values (1, 'x'), (2, 'y'), (3, 'z');
begin;
update ro_c set b = 'w' where a = 2;
rollback;
begin;
declare c cursor for select a, b from ro_c;
fetch 1 from c;
update ro_c set b = b || '!' where current of c;
fetch 1 from c;
fetch 1 from c;
commit;
select * from ro_c;
