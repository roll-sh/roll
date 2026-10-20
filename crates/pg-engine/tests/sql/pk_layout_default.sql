-- golden: raw-order
-- Storage parameters don't change a primary-keyed table's layout: its rows stay in heap
-- (physical) order with Postgres's ctids through ALTER TABLE SET / RESET, a key added to
-- or dropped from a table with rows, partitions made, attached and detached, and
-- inheritance. (roll's own `roll_pk_clustered` isn't Postgres's: tests/pk_clustered.rs.)
create table pkl_a (id int primary key, v text) with (fillfactor = 70, autovacuum_enabled = off);
insert into pkl_a values (5, 'a'), (1, 'b'), (3, 'c'), (2, 'd'), (4, 'e');
update pkl_a set id = 0 where id = 2;
select ctid, * from pkl_a;
select relname, reloptions from pg_class where relname in ('pkl_a', 'pkl_a_pkey') order by 1;
alter table pkl_a set (fillfactor = 50, toast_tuple_target = 200);
alter table pkl_a reset (autovacuum_enabled);
select ctid, * from pkl_a;
select reloptions from pg_class where relname = 'pkl_a';
insert into pkl_a values (9, 'i'), (7, 'g');
update pkl_a set v = 'b2' where id = 1;
select ctid, * from pkl_a;
-- Unknown parameters, bad values, and partitioned tables are refused as before.
alter table pkl_a set (pk_clustered = true);
alter table pkl_a set (roll_pk = on);
alter table pkl_a set (fillfactor = 5);
alter table pkl_a set (autovacuum_enabled = maybe);
create table pkl_bad (id int primary key) with (clustered = true);
create table pkl_p (k int primary key, v text) partition by range (k) with (fillfactor = 50);
create table pkl_p (k int primary key, v text) partition by range (k);
alter table pkl_p set (fillfactor = 50);
alter table pkl_p set (parallel_workers = 2);
alter table pkl_p reset (fillfactor);
create table pkl_p1 partition of pkl_p for values from (0) to (10) with (fillfactor = 60);
create table pkl_p2 partition of pkl_p for values from (10) to (100);
insert into pkl_p values (15, 'a'), (5, 'b'), (1, 'c'), (12, 'd'), (11, 'e');
update pkl_p set k = 50 where k = 1;
update pkl_p set v = 'x' where k = 12;
select tableoid::regclass, ctid, * from pkl_p;
select relname, reloptions from pg_class where relname like 'pkl_p%' and relkind in ('r', 'p') order by 1;
alter table pkl_p2 set (fillfactor = 40);
insert into pkl_p values (13, 'f');
select tableoid::regclass, ctid, * from pkl_p;
-- A table attached with rows: its key comes from the partitioned table.
create table pkl_p3 (k int not null, v text) with (fillfactor = 80);
insert into pkl_p3 values (150, 'z'), (120, 'y'), (130, 'x');
alter table pkl_p attach partition pkl_p3 for values from (100) to (200);
insert into pkl_p values (120, 'dup');
select ctid, * from pkl_p3;
alter table pkl_p detach partition pkl_p3;
insert into pkl_p3 values (110, 'w');
select ctid, * from pkl_p3;
-- A key added to a table with rows and storage parameters, and dropped again.
create table pkl_g (a int, b text) with (fillfactor = 90);
insert into pkl_g values (3, 'c'), (1, 'a'), (2, 'b');
alter table pkl_g add primary key (a), set (fillfactor = 85);
update pkl_g set a = 4 where a = 1;
select ctid, * from pkl_g;
alter table pkl_g drop constraint pkl_g_pkey;
insert into pkl_g values (4, 'dup');
select ctid, * from pkl_g;
select reloptions from pg_class where relname = 'pkl_g';
-- Inheritance: a parent's parameters aren't its child's.
create table pkl_ip (id int primary key, v text) with (fillfactor = 70);
create table pkl_ic (extra int) inherits (pkl_ip);
alter table pkl_ic add primary key (id);
insert into pkl_ip values (2, 'p2'), (1, 'p1');
insert into pkl_ic values (4, 'c4', 1), (3, 'c3', 2);
update pkl_ip set v = v || '!' where id in (1, 3);
select tableoid::regclass, ctid, * from pkl_ip;
select relname, reloptions from pg_class where relname in ('pkl_ip', 'pkl_ic') order by 1;
-- LIKE takes no storage parameters; CREATE TABLE AS its own.
create table pkl_like (like pkl_a including all);
select reloptions from pg_class where relname = 'pkl_like';
create table pkl_ctas with (fillfactor = 60) as select * from pkl_a;
alter table pkl_ctas add primary key (id);
update pkl_ctas set id = 10 where id = 0;
select ctid, * from pkl_ctas;
select reloptions from pg_class where relname = 'pkl_ctas';
-- Indexes' storage parameters; the key's index finds rows.
create index pkl_a_v on pkl_a (v) with (fillfactor = 70);
alter index pkl_a_pkey set (fillfactor = 60);
select relname, reloptions from pg_class where relname in ('pkl_a_v', 'pkl_a_pkey') order by 1;
set enable_seqscan = off;
set enable_bitmapscan = off;
select ctid, * from pkl_a where id = 9;
select id from pkl_a where v = 'g';
select id from pkl_a where id between 1 and 7 order by id;
reset enable_seqscan;
reset enable_bitmapscan;
vacuum full pkl_a;
select ctid, * from pkl_a;
