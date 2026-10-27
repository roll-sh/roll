-- Syntax and validation errors outside any keyspace.
SELEC * FROM x;
SELECT * FRM x;
SELECT * FROM;
SELECT * FROM nope.t;
SELECT * FROM t;
USE nope;
INSERT INTO nope.t (k) VALUES (1);
CREATE KEYSPACE {ks} WITH replication = {'class': 'NoSuchStrategy'};
CREATE KEYSPACE {ks} WITH replication = {'class': 'SimpleStrategy'};
DROP KEYSPACE nope;
DROP TABLE nope.t;
