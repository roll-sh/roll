-- A CHECK constraint's ROW(...)::composite expression holds the composite's column types
-- as of when it was made: after one of them changes, using the constraint fails (when a
-- row is checked), until the type is back.
create table crt_t1 (a int, b text);
create table crt_t2 (x int, y text, check ((x, y)::crt_t1 = (1, '42')::crt_t1));
insert into crt_t2 values (1, '42');
alter table crt_t1 alter column b type varchar;
insert into crt_t2 values (1, '42');
update crt_t2 set x = 1 where false;
update crt_t2 set x = 1;
insert into crt_t2 select 1, '42' where false;
select * from crt_t2;
alter table crt_t1 alter column b type varchar(20);
insert into crt_t2 values (1, '42');
alter table crt_t1 alter column b type text;
insert into crt_t2 values (1, '42');
select count(*) from crt_t2;
-- The arguments were coerced to the column types it had then.
create table crt_t3 (a int, b varchar(5));
create table crt_t4 (x int, y text, check ((x, y)::crt_t3 = (1, 'a')::crt_t3));
insert into crt_t4 values (1, 'a');
alter table crt_t3 alter column b type text;
insert into crt_t4 values (1, 'a');
alter table crt_t3 alter column b type varchar(7);
insert into crt_t4 values (1, 'a');
-- A typmod alone doesn't matter.
create table crt_t5 (a int, b numeric(5, 2));
create table crt_t6 (x int, y numeric, check ((x, y)::crt_t5 is distinct from null::crt_t5));
alter table crt_t5 alter column b type numeric(8, 3);
insert into crt_t6 values (1, 2.5);
select * from crt_t6;
