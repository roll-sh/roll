-- The same statements on every protocol version the dialect has.
CREATE KEYSPACE {ks} WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE {ks}.t (k int PRIMARY KEY, d date, tm time, s smallint, ti tinyint);
INSERT INTO {ks}.t (k, d, tm, s, ti) VALUES (1, '2024-01-02', '01:02:03', 1, 2);
@version 3
SELECT * FROM {ks}.t;
SELEC;
@version 4
SELECT * FROM {ks}.t;
@version 5
SELECT * FROM {ks}.t;
SELECT * FROM {ks}.nope;
@version 4
DROP KEYSPACE {ks};
