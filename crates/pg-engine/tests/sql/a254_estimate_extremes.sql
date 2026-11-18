-- Estimates over extreme values and over system catalogs, against PostgreSQL's: infinite dates
-- compared with timestamps (date_cmp_timestamp: a date past the timestamps' range below
-- infinity; the histogram's scale, date2timestamp_no_overflow), interval histograms, jsonb
-- operators' matchingsel over most common values and histograms, and catalog scans by initdb's
-- pg_class sizes, statistics and indexes.
-- (The catalogs first, while the database is as initdb left it.)
EXPLAIN SELECT * FROM pg_class WHERE oid = 1259;
EXPLAIN SELECT relname, oid FROM pg_class WHERE relname = ANY ('{p1,p2,c1,gc1}');
EXPLAIN SELECT count(*) FROM pg_attribute WHERE attrelid = 1259;
EXPLAIN SELECT p.proname, n.nspname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname IN ('a', 'b') ORDER BY 1;
EXPLAIN SELECT * FROM pg_trigger;
SELECT 'infinity'::date = 'infinity'::timestamp, '5874897-12-31'::date > '294276-12-31 23:59:59'::timestamp, '5874897-12-31'::date < 'infinity'::timestamp, '5874897-12-31'::date = 'infinity'::timestamp;
CREATE TABLE a254_d (x date, t timestamp, i interval) WITH (autovacuum_enabled = false);
INSERT INTO a254_d SELECT '2000-01-01'::date + (s % 3000), '2000-01-01'::timestamp + s * interval '1 hour', s * interval '1 minute' FROM generate_series(1, 10000) s;
INSERT INTO a254_d SELECT 'infinity', 'infinity', 'infinity' FROM generate_series(1, 50);
INSERT INTO a254_d SELECT '-infinity', '-infinity', '-infinity' FROM generate_series(1, 50);
ANALYZE a254_d;
EXPLAIN SELECT * FROM a254_d WHERE x < '2003-01-01'::timestamp;
EXPLAIN SELECT * FROM a254_d WHERE x > '2005-01-01 00:00:00+00'::timestamptz;
EXPLAIN SELECT * FROM a254_d WHERE x = 'infinity'::timestamp;
EXPLAIN SELECT * FROM a254_d WHERE x < 'infinity'::timestamp;
EXPLAIN SELECT * FROM a254_d WHERE t < '2000-06-01'::date;
EXPLAIN SELECT * FROM a254_d WHERE i < '30 hours';
EXPLAIN SELECT * FROM a254_d WHERE i > '100 hours';
CREATE TABLE a254_j (id int, j jsonb) WITH (autovacuum_enabled = false);
INSERT INTO a254_j SELECT s, jsonb_build_object('a', s % 10, 'b', s % 3, 'k' || (s % 50), true) FROM generate_series(1, 5000) s;
INSERT INTO a254_j SELECT s, '{"z": 1}' FROM generate_series(1, 2000) s;
INSERT INTO a254_j SELECT s, NULL FROM generate_series(1, 300) s;
ANALYZE a254_j;
EXPLAIN SELECT * FROM a254_j WHERE j @> '{"a": 1}';
EXPLAIN SELECT * FROM a254_j WHERE '{"z": 1}' <@ j;
EXPLAIN SELECT * FROM a254_j WHERE j ?| array['k7', 'z'];
EXPLAIN SELECT * FROM a254_j WHERE j @? '$.a';
