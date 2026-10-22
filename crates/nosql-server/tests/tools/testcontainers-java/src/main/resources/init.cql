CREATE KEYSPACE tc WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};
CREATE TABLE tc.users (id int PRIMARY KEY, name text);
INSERT INTO tc.users (id, name) VALUES (1, 'init script');
