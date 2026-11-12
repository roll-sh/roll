-- Batches as statements.
CREATE KEYSPACE {ks} WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE {ks}.t (k int, c int, v text, PRIMARY KEY (k, c));
BEGIN BATCH INSERT INTO {ks}.t (k, c, v) VALUES (1, 1, 'a'); INSERT INTO {ks}.t (k, c, v) VALUES (1, 2, 'b'); APPLY BATCH;
BEGIN UNLOGGED BATCH INSERT INTO {ks}.t (k, c, v) VALUES (2, 1, 'c'); UPDATE {ks}.t SET v = 'd' WHERE k = 2 AND c = 2; APPLY BATCH;
!unordered SELECT * FROM {ks}.t;
BEGIN BATCH SELECT * FROM {ks}.t; APPLY BATCH;
DROP KEYSPACE {ks};
