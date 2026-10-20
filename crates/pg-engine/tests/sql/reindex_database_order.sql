-- REINDEX SCHEMA / DATABASE go through the tables in the order a heap scan of pg_class
-- returns them: creation order, with a table whose pg_class row was updated other than in
-- place (ALTER TABLE ADD COLUMN / RENAME / ADD CHECK, GRANT, a first trigger, TRUNCATE,
-- a first child) moved after the rest; in-place updates (ANALYZE, CREATE INDEX) keep it.
create schema rdo;
set search_path = rdo;
create table t1 (a int primary key);
create table t2 (a int primary key);
create table t3 (a int primary key);
create table t4 (a int primary key);
create table t5 (a int primary key);
create table t6 (a int primary key, b text);
create table t7 (a int primary key);
create table t8 (a int primary key);
create table t9 (a int primary key);
create table t10 (a int primary key);
create materialized view m1 as select 1 as a;
create unique index on m1 (a);
alter table t1 add column b int;
alter table t2 rename to t2x;
alter table t3 add constraint c check (a > 0);
grant select on t4 to public;
create function rdo_tf() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger tr after insert on t5 for each row execute function rdo_tf();
truncate t7;
analyze t8;
create index on t9 (a);
alter table t10 owner to current_user;
reindex schema rdo;
select c.relname, rank() over (order by c.relfilenode) from pg_class c where c.relkind = 'i' and c.relnamespace = 'rdo'::regnamespace order by c.relfilenode;
create table t11 (a int primary key);
alter table t6 add column c int;
reindex database;
select c.relname, rank() over (order by c.relfilenode) from pg_class c where c.relkind = 'i' and c.relnamespace = 'rdo'::regnamespace order by c.relfilenode;
create table t12 (a int primary key) inherits (t8);
reindex schema rdo;
select c.relname, rank() over (order by c.relfilenode) from pg_class c where c.relkind = 'i' and c.relnamespace = 'rdo'::regnamespace order by c.relfilenode;
