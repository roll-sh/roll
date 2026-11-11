-- The oid counter never goes back: oids a rolled-back transaction or a failed statement
-- took aren't handed out again.
create table om_a (x int);
insert into om_a values (0);
begin;
create table om_b (x int);
create table om_c (x int);
rollback;
create table om_d (x int);
create table om_e as select 1 / x as y from om_a;
create table om_f (x int);
begin;
create sequence om_s;
select 1 / 0;
rollback;
create table om_g (x int);
create view om_v as select nosuch from om_a;
create view om_w as select x from om_a;
-- CREATE TABLE AS makes the table (and a materialized view its rule and new heap) before
-- the query runs.
create table om_i as select 'a'::text as t;
create table om_j (x int);
create materialized view om_m as select 'a'::text as t;
create table om_k (x int);
create materialized view om_n as select 1 / x as y from om_a;
create table om_l (x int);
create materialized view om_o as select 1 / x as y from om_a with no data;
create table om_p (x int);
select relname, oid::int8 - 'om_a'::regclass::oid::int8 as delta, reltoastrelid <> 0 as toast from pg_class where relname like 'om\_%' order by oid;
drop table om_a, om_d, om_f, om_g, om_i, om_j, om_k, om_l, om_p cascade;
