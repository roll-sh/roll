-- CREATE TABLE AS / SELECT INTO / materialized views whose query fails: a failure when the
-- query is planned (folding a constant) comes before the table exists and takes no oids; a
-- materialized view is created (WITH NO DATA) before its query is planned; WITH NO DATA
-- never plans; a failure while running comes after the oids are taken.
create table gxk_a0 (x int);
create table gxk_t1 as select 1/0 as x;
create table gxk_a1 (x int);
create materialized view gxk_m1 as select 1/0 as x;
create table gxk_a2 (x int);
create table gxk_t2 as select x/0 from gxk_a0;
create table gxk_a3 (x int);
insert into gxk_a0 values (1);
create table gxk_t3 as select x/0 from gxk_a0;
create table gxk_a4 (x int);
create materialized view gxk_m2 as select x/0 from gxk_a0;
create table gxk_a5 (x int);
create table gxk_t4 as select 'abc'::int;
create table gxk_a6 (x int);
create table gxk_t5 as select 1/0 as x with no data;
create table gxk_a7 (x int);
create table gxk_t6 as select (select 1/0) as x;
create table gxk_a8 (x int);
create table gxk_t7 as select x from gxk_a0 where 1/0 = 1;
create table gxk_a9 (x int);
create table gxk_t8 as select sqrt(-1::float8);
create table gxk_a10 (x int);
select * into gxk_t9 from (select 1/0) s;
create table gxk_a11 (x int);
create table gxk_t10 as select generate_series(1, 1/0);
create table gxk_a12 (x int);
create materialized view gxk_m3 as select 1/0 as x with no data;
create table gxk_a13 (x int);
create table gxk_t11 as select 1 as x, 1/0 as y;
create table gxk_a14 (x int);
create view gxk_v1 as select 1/0 as x;
select * from gxk_v1;
create table gxk_a15 (x int);
explain (costs off) create table gxk_t12 as select 1/0 as x;
create table gxk_a16 (x int);
select relname, oid::int8 - (select oid::int8 from pg_class where relname='gxk_a0') from pg_class where relname like 'gxk_%' order by 2;
