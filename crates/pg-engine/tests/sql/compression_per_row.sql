CREATE TABLE cpr_t(f1 text COMPRESSION lz4);
INSERT INTO cpr_t VALUES (repeat('123456789', 1004));
ALTER TABLE cpr_t ALTER COLUMN f1 SET COMPRESSION pglz;
INSERT INTO cpr_t VALUES (repeat('123456789', 1004));
SELECT xmin::text::int - (select min(xmin::text::int) from cpr_t), pg_column_compression(f1) FROM cpr_t;
CREATE TABLE cpr_p(f1 text COMPRESSION lz4) partition by hash (f1);
CREATE TABLE cpr_p1 partition of cpr_p for values with (modulus 1, remainder 0);
INSERT INTO cpr_p VALUES (repeat('123456789', 1004));
ALTER TABLE cpr_p1 ALTER COLUMN f1 SET COMPRESSION pglz;
INSERT INTO cpr_p VALUES (repeat('123456789', 1004));
SELECT pg_column_compression(f1) FROM cpr_p1;
-- copies keep the method they were stored with
create table cpr_copy (f1 text compression pglz);
insert into cpr_copy select f1 from cpr_t;
select pg_column_compression(f1) from cpr_copy;
update cpr_t set f1 = f1;
select pg_column_compression(f1) from cpr_t;
drop table cpr_t, cpr_p, cpr_copy;
