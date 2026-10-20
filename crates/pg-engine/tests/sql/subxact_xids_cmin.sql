create table sx_s (a int);
begin;
select txid_current() is not null as has;
insert into sx_s values (1);
savepoint s1;
insert into sx_s values (2);
savepoint s2;
insert into sx_s values (3);
rollback to s2;
insert into sx_s values (4);
release s1;
insert into sx_s values (5);
select a, xmin::text::int8 - (select xmin::text::int8 from sx_s where a = 1) as rel from sx_s order by a;
select pg_current_xact_id() = (select xmin::text::xid8 from sx_s where a = 1);
commit;
select a, xmin::text::int8 - (select xmin::text::int8 from sx_s where a = 1) as rel from sx_s order by a;
select a, txid_status(xmin::text::int8) from sx_s order by a;
begin;
savepoint a;
select pg_current_xact_id_if_assigned() is null;
insert into sx_s values (6);
select pg_current_xact_id_if_assigned() is not null;
select (select xmin::text::int8 from sx_s where a = 6) - pg_current_xact_id()::text::int8;
rollback to a;
select count(*) from sx_s;
commit;
begin;
savepoint q;
insert into sx_s values (7);
select txid_status(xmin::text::int8) from sx_s where a = 7;
rollback to q;
select txid_status(txid_current() + 1);
commit;
-- cmin / cmax and combo command ids
create temp table sx_c (v int);
begin;
insert into sx_c select 1 limit 0;
insert into sx_c values (1), (2);
select ctid, cmin, cmax, v from sx_c order by v;
select 1;
update sx_c set v = v * 10 where v = 1;
select ctid, cmin, cmax, v from sx_c order by v;
savepoint x;
delete from sx_c;
rollback to x;
select ctid, cmin, cmax, v from sx_c order by v;
commit;
select ctid, cmin, cmax, v from sx_c order by v;
begin;
delete from sx_c where v = 2;
select ctid, cmin, v from sx_c order by v;
rollback;
select ctid, cmin, cmax, v from sx_c order by v;
--
-- Tests for some likely failure cases with combo cmin/cmax mechanism
--
CREATE TEMP TABLE sx_cc (foobar int);

BEGIN;

-- a few dummy ops to push up the CommandId counter
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;

INSERT INTO sx_cc VALUES (1);
INSERT INTO sx_cc VALUES (2);

SELECT ctid,cmin,* FROM sx_cc;

SAVEPOINT s1;

UPDATE sx_cc SET foobar = foobar + 10;

-- here we should see only updated tuples
SELECT ctid,cmin,* FROM sx_cc;

ROLLBACK TO s1;

-- now we should see old tuples, but with combo CIDs starting at 0
SELECT ctid,cmin,* FROM sx_cc;

COMMIT;

-- combo data is not there anymore, but should still see tuples
SELECT ctid,cmin,* FROM sx_cc;

-- Test combo CIDs with portals
BEGIN;

INSERT INTO sx_cc VALUES (333);

DECLARE c CURSOR FOR SELECT ctid,cmin,* FROM sx_cc;

DELETE FROM sx_cc;

FETCH ALL FROM c;

ROLLBACK;

SELECT ctid,cmin,* FROM sx_cc;

-- check behavior with locked tuples
BEGIN;

-- a few dummy ops to push up the CommandId counter
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;
INSERT INTO sx_cc SELECT 1 LIMIT 0;

INSERT INTO sx_cc VALUES (444);

SELECT ctid,cmin,* FROM sx_cc;

SAVEPOINT s1;

-- this doesn't affect cmin
SELECT ctid,cmin,* FROM sx_cc FOR UPDATE;
SELECT ctid,cmin,* FROM sx_cc;

-- but this does
UPDATE sx_cc SET foobar = foobar + 10;

SELECT ctid,cmin,* FROM sx_cc;

ROLLBACK TO s1;

SELECT ctid,cmin,* FROM sx_cc;

COMMIT;

SELECT ctid,cmin,* FROM sx_cc;

-- test for bug reported in
-- CABRT9RC81YUf1=jsmWopcKJEro=VoeG2ou6sPwyOUTx_qteRsg@mail.gmail.com
CREATE TABLE IF NOT EXISTS sx_tc(
	id int PRIMARY KEY,
	balance numeric
);
INSERT INTO sx_tc VALUES (1, 0);
BEGIN;
SELECT * FROM sx_tc WHERE sx_tc.id = 1 FOR UPDATE;
UPDATE sx_tc SET balance = balance + 400 WHERE id=1;
SAVEPOINT subxact;
UPDATE sx_tc SET balance = balance - 100 WHERE id=1;
ROLLBACK TO SAVEPOINT subxact;
-- should return one tuple
SELECT * FROM sx_tc WHERE id = 1 FOR UPDATE;
ROLLBACK;
DROP TABLE sx_tc;
