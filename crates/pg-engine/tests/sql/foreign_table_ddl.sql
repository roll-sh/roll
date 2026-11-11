-- Foreign tables: CREATE / ALTER / DROP FOREIGN TABLE, their catalog rows (relkind 'f',
-- pg_foreign_table, attfdwoptions), what a wrapper without a handler refuses, foreign
-- partitions and inheritance children, IMPORT FOREIGN SCHEMA; and SECURITY LABEL without a
-- label provider.
create foreign data wrapper ftd_fdw;
create server ftd_srv foreign data wrapper ftd_fdw;
create schema ftd_ns;
create foreign table ftd_t1 () server ftd_nosrv;
create foreign table ftd_t1 (a int primary key) server ftd_srv;
create foreign table ftd_t1 (a int, unique (a)) server ftd_srv;
create foreign table ftd_t1 (a int references ftd_t1 (a)) server ftd_srv;
create foreign table ftd_t1 (
  a int options (col 'x') not null,
  b text options (c1 'v1', c2 'v2') check (b <> ''),
  c date default current_date
) server ftd_srv options (o1 'a', "o 2" 'b');
create foreign table if not exists ftd_t1 (a int) server ftd_srv;
select c.relname, c.relkind, c.relam, c.relfilenode, c.reltoastrelid, c.relpersistence, c.relreplident, c.relhasindex
  from pg_class c where c.relname = 'ftd_t1';
select ftrelid::regclass, s.srvname, ftoptions from pg_foreign_table f join pg_foreign_server s on s.oid = f.ftserver where ftrelid = 'ftd_t1'::regclass;
select attname, attnum, attfdwoptions from pg_attribute where attrelid = 'ftd_t1'::regclass order by attnum;
select foreign_table_schema, foreign_table_name, foreign_server_name from information_schema.foreign_tables where foreign_table_name like 'ftd%';
select option_name, option_value from information_schema.foreign_table_options where foreign_table_name = 'ftd_t1' order by 1;
select pg_describe_object(classid, objid, objsubid), pg_describe_object(refclassid, refobjid, refobjsubid), deptype
  from pg_depend where objid = 'ftd_t1'::regclass and objsubid = 0 and refclassid <> 'pg_namespace'::regclass;
-- What needs the wrapper's handler.
select * from ftd_t1;
insert into ftd_t1 values (1, 'x');
update ftd_t1 set a = 2;
delete from ftd_t1;
truncate ftd_t1;
copy ftd_t1 to stdout;
create view ftd_v as select * from ftd_t1;
select * from ftd_v;
drop view ftd_v;
create index ftd_i on ftd_t1 (a);
vacuum ftd_t1;
analyze ftd_t1;
vacuum analyze ftd_t1;
-- ALTER FOREIGN TABLE.
alter foreign table ftd_t1 add column d int options (p 'q');
alter foreign table ftd_t1 add column e int primary key;
alter foreign table ftd_t1 add constraint ftd_c check (a > 0) not valid;
alter foreign table ftd_t1 add constraint ftd_u unique (a);
alter foreign table ftd_t1 alter constraint ftd_c deferrable;
alter foreign table ftd_t1 alter column d options (set p 'r', add s 't');
alter foreign table ftd_t1 alter column d options (drop nope);
alter foreign table ftd_t1 alter column xmin options (add p 'v');
alter foreign table ftd_t1 alter column nope options (add p 'v');
alter foreign table ftd_t1 options (drop o1, set "o 2" 'c', add o3 'd');
alter foreign table ftd_t1 alter column d type text using '0';
alter foreign table ftd_t1 alter column d type text;
alter foreign table ftd_t1 set (fillfactor = 10);
alter foreign table ftd_t1 cluster on nope;
alter foreign table ftd_t1 set logged;
alter table ftd_t1 options (set o3 'e');
alter table ftd_t1 alter column d set statistics 100;
create table ftd_plain (a int);
alter foreign table ftd_plain add column b int;
alter table ftd_plain options (add x 'y');
select attname, attfdwoptions, attstattarget from pg_attribute where attrelid = 'ftd_t1'::regclass and attnum > 0 order by attnum;
select ftoptions from pg_foreign_table where ftrelid = 'ftd_t1'::regclass;
select conname, contype, convalidated from pg_constraint where conrelid = 'ftd_t1'::regclass order by 1;
create table ftd_uses (x ftd_t1);
alter foreign table ftd_t1 alter column d type int;
drop table ftd_uses;
alter foreign table ftd_t1 rename column d to dd;
alter foreign table ftd_t1 set schema ftd_ns;
alter foreign table ftd_ns.ftd_t1 rename to ftd_t2;
alter foreign table ftd_plain rename to ftd_x;
alter foreign table if exists ftd_nope add column z int;
select relname, relnamespace::regnamespace from pg_class where relname like 'ftd_t%';
-- Triggers.
create function ftd_trig() returns trigger language plpgsql as $$ begin return null; end $$;
create trigger ftd_tr1 before insert on ftd_ns.ftd_t2 for each row execute function ftd_trig();
create trigger ftd_tr2 instead of insert on ftd_ns.ftd_t2 for each row execute function ftd_trig();
create constraint trigger ftd_tr3 after insert on ftd_ns.ftd_t2 for each row execute function ftd_trig();
create trigger ftd_tr4 after insert on ftd_ns.ftd_t2 referencing new table as n for each statement execute function ftd_trig();
-- Foreign partitions.
create table ftd_lt (a int) partition by range (a);
create index on ftd_lt (a);
create foreign table ftd_p1 partition of ftd_lt for values from (0) to (10) server ftd_srv;
create foreign table ftd_p2 (a int) server ftd_srv;
alter table ftd_lt attach partition ftd_p2 for values from (10) to (20);
create unique index on ftd_lt (a);
alter table ftd_lt add primary key (a);
select relname, relkind from pg_class where relname like 'ftd_lt%' or relname like 'ftd_p_' order by 1;
insert into ftd_lt values (5);
select * from ftd_lt;
select * from only ftd_lt;
drop foreign table ftd_p1, ftd_p2;
create unique index ftd_lt_u on ftd_lt (a);
create foreign table ftd_p1 partition of ftd_lt for values from (0) to (10) server ftd_srv;
create foreign table ftd_p3 (a int) server ftd_srv;
alter table ftd_lt attach partition ftd_p3 for values from (10) to (20);
drop table ftd_lt;
-- Inheritance.
create table ftd_parent (a int, b text);
create foreign table ftd_child () inherits (ftd_parent) server ftd_srv;
select * from ftd_parent;
select * from only ftd_parent;
alter table ftd_parent add constraint ftd_pc check (a > 0) not valid;
alter table ftd_parent validate constraint ftd_pc;
select conrelid::regclass, conname, convalidated from pg_constraint where conname = 'ftd_pc' order by 1;
alter table ftd_parent alter column b type varchar(10) using b::varchar;
drop table ftd_parent;
drop table ftd_parent cascade;
-- IMPORT FOREIGN SCHEMA.
import foreign schema remote from server ftd_srv into ftd_ns;
import foreign schema remote from server ftd_nosrv into ftd_ns;
import foreign schema remote from server ftd_srv into ftd_nope;
-- DROP.
drop table ftd_ns.ftd_t2;
drop foreign table ftd_x;
drop foreign table ftd_nope;
drop foreign table if exists ftd_nope;
drop server ftd_srv;
drop server ftd_srv cascade;
select count(*) from pg_foreign_table;
-- SECURITY LABEL.
security label on table ftd_x is 'x';
security label for dummy on table ftd_x is 'x';
security label on role postgres is 'x';
drop table ftd_x;
drop function ftd_trig();
drop schema ftd_ns;
drop foreign data wrapper ftd_fdw;
