-- Native types and collections: column specs and serialised cells.
CREATE KEYSPACE {ks} WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TABLE {ks}.t (k int PRIMARY KEY, a ascii, b bigint, bl blob, bo boolean, d decimal, db double, f float, i inet, s smallint, ti tinyint, tx text, ts timestamp, u uuid, vi varint, dt date, tm time, l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, tu tuple<int, text>);
INSERT INTO {ks}.t (k, a, b, bl, bo, d, db, f, i, s, ti, tx, ts, u, vi, dt, tm, l, st, m, fl, tu) VALUES (1, 'asc', 9223372036854775807, 0xcafe, true, 1.25, 2.5, 3.5, '127.0.0.1', 32767, 127, 'text', '2024-01-02 03:04:05+0000', 123e4567-e89b-12d3-a456-426614174000, 123456789012345678901234567890, '2024-01-02', '01:02:03.456', [1, 2, 3], {'x', 'y'}, {'a': 1, 'b': 2}, [4, 5], (1, 'one'));
SELECT * FROM {ks}.t;
INSERT INTO {ks}.t (k) VALUES (2);
SELECT * FROM {ks}.t WHERE k = 2;
UPDATE {ks}.t SET l = l + [4], st = st - {'x'}, m['c'] = 3 WHERE k = 1;
SELECT l, st, m FROM {ks}.t WHERE k = 1;
INSERT INTO {ks}.t (k, b) VALUES (3, 'notanumber');
INSERT INTO {ks}.t (k, s) VALUES (3, 99999);
DROP KEYSPACE {ks};
