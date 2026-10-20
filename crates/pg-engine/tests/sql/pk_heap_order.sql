-- golden: raw-order
-- Tables with a primary key return rows in heap (physical) order, with Postgres's ctids,
-- like any other table: after updates of the key and of other columns, failed updates
-- and inserts (whose dead tuples keep their places), deletes, VACUUM, table rewrites by
-- ALTER TABLE and CLUSTER, and with a deferrable key.
create table pkh_a (id int primary key, v text);
insert into pkh_a values (5, 'a'), (1, 'b'), (3, 'c'), (2, 'd'), (4, 'e');
select ctid, * from pkh_a;
update pkh_a set id = 0 where id = 2;
update pkh_a set v = 'x' where id = 1;
select ctid, * from pkh_a;
update pkh_a set id = 5 where id = 3;
insert into pkh_a values (4, 'dup');
insert into pkh_a values (6, 'f');
select ctid, * from pkh_a;
delete from pkh_a where id in (5, 1);
vacuum pkh_a;
insert into pkh_a values (7, 'g'), (8, 'h');
select ctid, * from pkh_a;
-- The primary key's index finds rows (point lookups and ranges).
set enable_seqscan = off;
set enable_bitmapscan = off;
explain (costs off) select * from pkh_a where id = 7;
select ctid, * from pkh_a where id = 7;
select id from pkh_a where id between 3 and 7 order by id;
reset enable_seqscan;
reset enable_bitmapscan;
-- A composite key, with a referencing table.
create table pkh_p (a int, b int, c text, primary key (a, b));
create table pkh_f (a int, b int, foreign key (a, b) references pkh_p on update cascade on delete cascade);
insert into pkh_p values (1, 2, 'x'), (1, 3, 'y'), (2, 4, 'z'), (3, 6, 'w');
insert into pkh_f values (1, 2), (2, 4), (3, 6), (null, 1);
delete from pkh_p where a = 1 and b = 3;
update pkh_p set a = 1 where a = 2;
select ctid, * from pkh_p;
select ctid, * from pkh_f;
update pkh_p set a = 0 where a = 3;
select ctid, * from pkh_p;
select ctid, * from pkh_f;
-- ALTER TABLE rewrites copy the rows in heap order into a new heap.
alter table pkh_p alter column a type bigint;
alter table pkh_f alter column a type bigint;
select ctid, * from pkh_p;
select ctid, * from pkh_f;
alter table pkh_p add column d int default 1, alter column c type varchar(10);
select ctid, * from pkh_p;
-- A deferrable key's values may clash until it is checked.
create table pkh_d (id int primary key deferrable initially deferred, v text);
insert into pkh_d values (2, 'a'), (1, 'b');
begin;
insert into pkh_d values (2, 'c');
update pkh_d set id = 3 where v = 'c';
commit;
select ctid, * from pkh_d;
begin;
insert into pkh_d values (1, 'clash');
commit;
select ctid, * from pkh_d;
-- Adding a primary key to a table with rows, and dropping it.
create table pkh_g (a int, b text);
insert into pkh_g values (3, 'c'), (1, 'a'), (2, 'b');
alter table pkh_g add primary key (a);
update pkh_g set a = 4 where a = 1;
select ctid, * from pkh_g;
alter table pkh_g drop constraint pkh_g_pkey;
insert into pkh_g values (4, 'dup');
select ctid, * from pkh_g;
-- CLUSTER puts the rows in the key's order.
cluster pkh_a using pkh_a_pkey;
select ctid, * from pkh_a;
-- A bigger table: updates of the key move rows to the end of the heap.
create table pkh_big (id int primary key, v int, w text);
insert into pkh_big select g, g % 10, repeat('x', 50) from generate_series(1, 1000) g;
update pkh_big set id = id + 2000 where id % 7 = 0;
update pkh_big set v = v + 1 where id % 5 = 0;
delete from pkh_big where id % 11 = 0;
select count(*), md5(string_agg(id::text || ctid::text, ',')) from pkh_big;
vacuum pkh_big;
insert into pkh_big select g, 0, 'n' from generate_series(5000, 5100) g;
select count(*), md5(string_agg(id::text || ctid::text, ',')) from pkh_big;
select pg_relation_size('pkh_big'), pg_relation_size('pkh_big_pkey') > 0;
