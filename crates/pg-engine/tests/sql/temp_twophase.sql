-- For partitioned temp tables, ON COMMIT actions ignore storage-less
-- partitioned tables.
begin;
create temp table temp_parted_oncommit (a int)
  partition by list (a) on commit delete rows;
create temp table temp_parted_oncommit_1
  partition of temp_parted_oncommit
  for values in (1) on commit delete rows;
insert into temp_parted_oncommit values (1);
commit;
-- partitions are emptied by the previous commit
select * from temp_parted_oncommit;
drop table temp_parted_oncommit;

-- Check dependencies between ON COMMIT actions with a partitioned
-- table and its partitions.  Using ON COMMIT DROP on a parent removes
-- the whole set.
begin;
create temp table temp_parted_oncommit_test (a int)
  partition by list (a) on commit drop;
create temp table temp_parted_oncommit_test1
  partition of temp_parted_oncommit_test
  for values in (1) on commit delete rows;
create temp table temp_parted_oncommit_test2
  partition of temp_parted_oncommit_test
  for values in (2) on commit drop;
insert into temp_parted_oncommit_test values (1), (2);
commit;
-- no relations remain in this case.
select relname from pg_class where relname ~ '^temp_parted_oncommit_test';
-- Using ON COMMIT DELETE on a partitioned table does not remove
-- all rows if partitions preserve their data.
begin;
create temp table temp_parted_oncommit_test (a int)
  partition by list (a) on commit delete rows;
create temp table temp_parted_oncommit_test1
  partition of temp_parted_oncommit_test
  for values in (1) on commit preserve rows;
create temp table temp_parted_oncommit_test2
  partition of temp_parted_oncommit_test
  for values in (2) on commit drop;
insert into temp_parted_oncommit_test values (1), (2);
commit;
-- Data from the remaining partition is still here as its rows are
-- preserved.
select * from temp_parted_oncommit_test;
-- two relations remain in this case.
select relname from pg_class where relname ~ '^temp_parted_oncommit_test'
  order by relname;
drop table temp_parted_oncommit_test;

-- Check dependencies between ON COMMIT actions with inheritance trees.
-- Using ON COMMIT DROP on a parent removes the whole set.
begin;
create temp table temp_inh_oncommit_test (a int) on commit drop;
create temp table temp_inh_oncommit_test1 ()
  inherits(temp_inh_oncommit_test) on commit delete rows;
insert into temp_inh_oncommit_test1 values (1);
commit;
-- no relations remain in this case
select relname from pg_class where relname ~ '^temp_inh_oncommit_test';
-- Data on the parent is removed, and the child goes away.
begin;
create temp table temp_inh_oncommit_test (a int) on commit delete rows;
create temp table temp_inh_oncommit_test1 ()
  inherits(temp_inh_oncommit_test) on commit drop;
insert into temp_inh_oncommit_test1 values (1);
insert into temp_inh_oncommit_test values (1);
commit;
select * from temp_inh_oncommit_test;
-- one relation remains
select relname from pg_class where relname ~ '^temp_inh_oncommit_test';
drop table temp_inh_oncommit_test;

-- Tests with two-phase commit
-- Transactions creating objects in a temporary namespace cannot be used
-- with two-phase commit.

-- These cases generate errors about temporary namespace.
-- Function creation
begin;
create function pg_temp.twophase_func() returns void as
  $$ select '2pc_func'::text $$ language sql;
prepare transaction 'twophase_func';
-- Function drop
create function pg_temp.twophase_func() returns void as
  $$ select '2pc_func'::text $$ language sql;
begin;
drop function pg_temp.twophase_func();
prepare transaction 'twophase_func';
-- Operator creation
begin;
create operator pg_temp.@@ (leftarg = int4, rightarg = int4, procedure = int4mi);
prepare transaction 'twophase_operator';

-- These generate errors about temporary tables.
begin;
create type pg_temp.twophase_type as (a int);
prepare transaction 'twophase_type';
begin;
create view pg_temp.twophase_view as select 1;
prepare transaction 'twophase_view';
begin;
create sequence pg_temp.twophase_seq;
prepare transaction 'twophase_sequence';

-- Temporary tables cannot be used with two-phase commit.
create temp table twophase_tab (a int);
begin;
select a from twophase_tab;
prepare transaction 'twophase_tab';
begin;
insert into twophase_tab values (1);
prepare transaction 'twophase_tab';
begin;
lock twophase_tab in access exclusive mode;
prepare transaction 'twophase_tab';
begin;
drop table twophase_tab;
prepare transaction 'twophase_tab';

-- Corner case: current_schema may create a temporary schema if namespace
-- creation is pending, so check after that.  First reset the connection
-- to remove the temporary namespace.
SET search_path TO 'pg_temp';
BEGIN;
SELECT current_schema() ~ 'pg_temp' AS is_temp_schema;
PREPARE TRANSACTION 'twophase_search';


-- Tests to verify we recover correctly from exhausting buffer pins and
-- related matters.

-- use lower possible buffer limit to make the test cheaper
SET temp_buffers = 100;

CREATE TEMPORARY TABLE test_temp(a int not null unique, b TEXT not null, cnt int not null);
INSERT INTO test_temp SELECT generate_series(1, 10000) as id, repeat('a', 200), 0;
-- should be at least 2x as large than temp_buffers
SELECT pg_relation_size('test_temp') / current_setting('block_size')::int8 > 200;
DROP TABLE test_temp;
