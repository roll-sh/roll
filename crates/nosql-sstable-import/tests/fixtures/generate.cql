-- Data for the SSTable import fixtures and the live import tests. `imp_fx` is replaced by the
-- keyspace under test. Runs unchanged on cassandra:5.0.9 and scylladb/scylla:2026.3.3.
CREATE KEYSPACE imp_fx WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
CREATE TYPE imp_fx.addr (street text, zip int, tags frozen<set<text>>);
CREATE TYPE imp_fx.point (x double, y double);

-- Every native type, frozen collections, tuples, vectors, frozen UDTs.
CREATE TABLE imp_fx.all_types (
    k int, c text,
    a_ascii ascii, a_bigint bigint, a_blob blob, a_boolean boolean, a_date date, a_decimal decimal,
    a_double double, a_duration duration, a_float float, a_inet inet, a_int int, a_smallint smallint,
    a_text text, a_time time, a_timestamp timestamp, a_timeuuid timeuuid, a_tinyint tinyint,
    a_uuid uuid, a_varchar varchar, a_varint varint, a_vector vector<float, 3>, a_tvector vector<text, 2>,
    f_list frozen<list<int>>, f_set frozen<set<text>>, f_map frozen<map<int, text>>,
    f_tuple tuple<int, text, frozen<list<int>>>, f_addr frozen<addr>, f_nested frozen<map<text, frozen<list<frozen<point>>>>>,
    PRIMARY KEY (k, c));
INSERT INTO imp_fx.all_types (k, c, a_ascii, a_bigint, a_blob, a_boolean, a_date, a_decimal, a_double, a_duration, a_float, a_inet, a_int, a_smallint, a_text, a_time, a_timestamp, a_timeuuid, a_tinyint, a_uuid, a_varchar, a_varint, a_vector, a_tvector, f_list, f_set, f_map, f_tuple, f_addr, f_nested)
  VALUES (1, 'a', 'ascii', -9223372036854775808, 0xcafebabe, true, '2026-10-20', -12345.678901234567890, 3.141592653589793, 1y2mo3d4h5m6s7ms8us9ns, 2.5, '2001:db8::1', -2147483648, -32768, 'héllo ✓', '13:14:15.123456789', '2026-10-20 08:00:00.123+0000', 50554d6e-29bb-11e5-b345-feff819cdc9f, -128, 01234567-89ab-cdef-0123-456789abcdef, 'varchar', 123456789012345678901234567890, [1.5, -2.25, 3.0], ['x', 'yy'], [3, 1, 2], {'b', 'a'}, {2: 'two', 1: 'one'}, (7, 'seven', [7]), {street: 'Main', zip: 1234, tags: {'t1'}}, {'k': [{x: 1.0, y: 2.0}]});
INSERT INTO imp_fx.all_types (k, c, a_int) VALUES (1, 'b', 42) USING TIMESTAMP 1700000000000001;
INSERT INTO imp_fx.all_types (k, c, a_text, a_blob, a_int) VALUES (2, '', '', 0x, blobAsInt(0x));
INSERT INTO imp_fx.all_types (k, c) VALUES (3, 'marker-only');
UPDATE imp_fx.all_types SET a_text = 'update-only' WHERE k = 4 AND c = 'no-marker';
INSERT INTO imp_fx.all_types (k, c, a_blob) VALUES (5, 'big', 0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff);

-- Non-frozen collections and UDTs: appends, element deletes, overwrites (complex deletions),
-- element TTLs.
CREATE TABLE imp_fx.colls (k int PRIMARY KEY, l list<int>, s set<text>, m map<text, int>, u addr, lu list<frozen<addr>>, mf map<int, frozen<list<int>>>);
INSERT INTO imp_fx.colls (k, l, s, m, u, lu, mf) VALUES (1, [1, 2, 3], {'a', 'b', 'c'}, {'x': 1, 'y': 2}, {street: 'S', zip: 1, tags: {'q'}}, [{street: 'L', zip: 2, tags: {}}], {1: [1, 1], 2: [2]});
UPDATE imp_fx.colls SET l = l + [4], s = s - {'b'}, m['z'] = 3 WHERE k = 1;
DELETE l[0], m['x'] FROM imp_fx.colls WHERE k = 1;
UPDATE imp_fx.colls USING TTL 157680000 SET s = s + {'ttl'}, m['t'] = 9 WHERE k = 1;
UPDATE imp_fx.colls SET u.zip = 99 WHERE k = 1;
INSERT INTO imp_fx.colls (k, l) VALUES (2, [5]);
UPDATE imp_fx.colls SET l = [6, 7] WHERE k = 2;
UPDATE imp_fx.colls SET s = {} WHERE k = 3;
INSERT INTO imp_fx.colls (k, m) VALUES (4, {'only': 1});
DELETE m FROM imp_fx.colls WHERE k = 4;

-- Static columns: static-only partitions, statics with rows, static TTLs and deletes.
CREATE TABLE imp_fx.statics (k int, c int, s text static, s2 int static, v text, PRIMARY KEY (k, c));
INSERT INTO imp_fx.statics (k, c, s, s2, v) VALUES (1, 1, 'st1', 10, 'v1');
INSERT INTO imp_fx.statics (k, c, v) VALUES (1, 2, 'v2');
INSERT INTO imp_fx.statics (k, s) VALUES (2, 'static-only');
INSERT INTO imp_fx.statics (k, s2) VALUES (3, 33) USING TTL 157680000;
INSERT INTO imp_fx.statics (k, c, s, v) VALUES (4, 1, 'gone', 'stays');
DELETE s FROM imp_fx.statics WHERE k = 4;

-- Counters, with increments, decrements and a deleted row.
CREATE TABLE imp_fx.counters (k int, c int, n counter, m counter, PRIMARY KEY (k, c));
UPDATE imp_fx.counters SET n = n + 5, m = m - 3 WHERE k = 1 AND c = 1;
UPDATE imp_fx.counters SET n = n + 7 WHERE k = 1 AND c = 1;
UPDATE imp_fx.counters SET n = n - 100 WHERE k = 1 AND c = 2;
UPDATE imp_fx.counters SET m = m + 1 WHERE k = 2 AND c = 1;
UPDATE imp_fx.counters SET n = n + 1 WHERE k = 3 AND c = 1;
DELETE FROM imp_fx.counters WHERE k = 3 AND c = 1;

-- Write times and TTLs: per-cell timestamps, row TTLs, mixed cell TTLs, expired data.
CREATE TABLE imp_fx.ttls (k int, c int, a text, b text, PRIMARY KEY (k, c));
INSERT INTO imp_fx.ttls (k, c, a, b) VALUES (1, 1, 'a', 'b') USING TTL 157680000 AND TIMESTAMP 1600000000000000;
UPDATE imp_fx.ttls USING TTL 86400000 SET b = 'b2' WHERE k = 1 AND c = 1;
INSERT INTO imp_fx.ttls (k, c, a) VALUES (1, 2, 'short') USING TTL 1;
INSERT INTO imp_fx.ttls (k, c, a, b) VALUES (1, 3, 'keep', 'expire') ;
UPDATE imp_fx.ttls USING TTL 1 SET b = 'expire' WHERE k = 1 AND c = 3;
INSERT INTO imp_fx.ttls (k, c, a) VALUES (2, 1, 'future') USING TIMESTAMP FUTURE_TS;

-- Deletions of every kind.
CREATE TABLE imp_fx.deletes (k int, c1 int, c2 text, v int, PRIMARY KEY (k, c1, c2));
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 1, 'a', 1);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 1, 'b', 2);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 2, 'a', 3);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 3, 'a', 4);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 4, 'a', 5);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 5, 'a', 6);
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (1, 6, 'a', 7);
DELETE FROM imp_fx.deletes WHERE k = 1 AND c1 = 1 AND c2 = 'a';
DELETE FROM imp_fx.deletes WHERE k = 1 AND c1 = 2;
DELETE FROM imp_fx.deletes USING TIMESTAMP 1000 WHERE k = 1 AND c1 > 2 AND c1 <= 4;
DELETE FROM imp_fx.deletes WHERE k = 1 AND c1 > 4 AND c1 < 6;
DELETE FROM imp_fx.deletes WHERE k = 1 AND c1 >= 6;
DELETE v FROM imp_fx.deletes WHERE k = 1 AND c1 = 1 AND c2 = 'b';
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (2, 1, 'a', 1);
DELETE FROM imp_fx.deletes WHERE k = 2;
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (2, 9, 'after', 9);
DELETE FROM imp_fx.deletes WHERE k = 3;
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (4, 1, 'x', 1) USING TIMESTAMP 2000;
DELETE FROM imp_fx.deletes USING TIMESTAMP 3000 WHERE k = 4 AND c1 = 1;
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (4, 1, 'y', 2) USING TIMESTAMP 2500;
INSERT INTO imp_fx.deletes (k, c1, c2, v) VALUES (4, 1, 'z', 3) USING TIMESTAMP 3500;

-- Composite partition keys and DESC clustering.
CREATE TABLE imp_fx.composite (k1 int, k2 text, c1 int, c2 timestamp, v text, PRIMARY KEY ((k1, k2), c1, c2)) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC);
INSERT INTO imp_fx.composite (k1, k2, c1, c2, v) VALUES (1, 'a', 1, '2026-01-01', 'x');
INSERT INTO imp_fx.composite (k1, k2, c1, c2, v) VALUES (1, 'a', 2, '2026-01-01', 'y');
INSERT INTO imp_fx.composite (k1, k2, c1, c2, v) VALUES (1, 'b', 1, '2026-01-02', 'z');
DELETE FROM imp_fx.composite WHERE k1 = 1 AND k2 = 'a' AND c1 > 1;
INSERT INTO imp_fx.composite (k1, k2, c1, c2, v) VALUES (1, 'a', 3, '2026-01-03', 'after-range');

-- A dropped column (Cassandra records it in schema.cql).
CREATE TABLE imp_fx.dropped (k int PRIMARY KEY, keep text, gone text);
INSERT INTO imp_fx.dropped (k, keep, gone) VALUES (1, 'kept', 'dropped');
ALTER TABLE imp_fx.dropped DROP gone;
INSERT INTO imp_fx.dropped (k, keep) VALUES (2, 'after-drop');

-- Compression variants.
CREATE TABLE imp_fx.c_lz4 (k int, c int, v text, PRIMARY KEY (k, c)) WITH compression = {'class': 'LZ4Compressor', 'chunk_length_in_kb': 4};
CREATE TABLE imp_fx.c_snappy (k int, c int, v text, PRIMARY KEY (k, c)) WITH compression = {'class': 'SnappyCompressor'};
CREATE TABLE imp_fx.c_deflate (k int, c int, v text, PRIMARY KEY (k, c)) WITH compression = {'class': 'DeflateCompressor'};
CREATE TABLE imp_fx.c_zstd (k int, c int, v text, PRIMARY KEY (k, c)) WITH compression = {'class': 'ZstdCompressor'};
CREATE TABLE imp_fx.c_none (k int, c int, v text, PRIMARY KEY (k, c)) WITH compression = {'enabled': 'false'};
