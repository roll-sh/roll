-- Paging: page boundaries and the has-more-pages flag.
CREATE KEYSPACE {ks} WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE {ks}.t (k int, c int, PRIMARY KEY (k, c));
INSERT INTO {ks}.t (k, c) VALUES (1, 1);
INSERT INTO {ks}.t (k, c) VALUES (1, 2);
INSERT INTO {ks}.t (k, c) VALUES (1, 3);
INSERT INTO {ks}.t (k, c) VALUES (1, 4);
INSERT INTO {ks}.t (k, c) VALUES (1, 5);
@page 2
SELECT c FROM {ks}.t WHERE k = 1;
@page 5
SELECT c FROM {ks}.t WHERE k = 1;
@page 10
SELECT c FROM {ks}.t WHERE k = 1;
@page 0
SELECT c FROM {ks}.t WHERE k = 1;
DROP KEYSPACE {ks};
