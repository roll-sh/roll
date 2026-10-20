-- "char": \ooo escapes on input, high-bit bytes shown as \ooo
SELECT '\101'::"char", '\377'::"char"::text, '\000'::"char"::text, 'a'::text::"char", '\377'::text::"char";
-- bit: position of an empty substring; a bit literal is plain bit
SELECT position(B'' IN B'00001010'), position(B'' IN B''), position(B'1' IN B'');
CREATE TABLE ga_bit_defaults (b1 bit(4) DEFAULT '1001', b2 bit(4) DEFAULT B'0101', b3 bit varying(5) DEFAULT '1001', b4 bit varying(5) DEFAULT B'0101');
SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = 'ga_bit_defaults'::regclass ORDER BY 1;
CREATE VIEW ga_bit_view AS SELECT B'0101' AS a, '11'::"bit" AS c;
SELECT pg_get_viewdef('ga_bit_view'::regclass);
SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'ga_bit_view'::regclass ORDER BY attnum;
DROP VIEW ga_bit_view;
DROP TABLE ga_bit_defaults;
-- oid / xid / cid input: strtoul (octal, hex, negatives)
SELECT '010'::xid, '0x10'::xid, '-1'::xid, ' 12 '::oid, '010'::oid, '-2147483648'::oid;
SELECT '0x'::oid;
SELECT '4294967296'::xid;
SELECT '1 2'::cid;
-- pg_lsn: no whitespace
SELECT ' 0/12345678'::pg_lsn;
SELECT '0/12345678 '::pg_lsn;
SELECT '123456789/1'::pg_lsn;
-- tid input
SELECT '(-1,0)'::tid, '(4294967295,65535)'::tid, '( 1 , 2 )'::tid, 'x(0,1)'::tid;
SELECT '(0,-1)'::tid;
SELECT '(4294967296,1)'::tid;
SELECT '(0,1) x'::tid;
-- reg* names
SELECT regrole('foo.bar');
SELECT regnamespace('foo.bar');
SELECT 'no_such_catalog.schema.name'::regtype;
SELECT * FROM pg_input_error_info('no_such_catalog.schema.name', 'regtype');
-- trailing junk after a parameter
PREPARE ga_p1 AS SELECT $1a;
PREPARE ga_p1 AS SELECT $0_1;
SELECT '$1a', $$ $1a $$, "$1a" FROM (SELECT 1 AS "$1a") s;
-- type I/O functions called directly
SELECT int4in('5'), int4out(5), textout('a'), boolin('t'), tsvectorin(tsvectorout('b a'::tsvector)), numeric_out(1.50);
-- oidvector / int2vector comparisons
SELECT '1 2'::oidvector = '1 2', '1 2'::oidvector <> '', '1 2'::oidvector < '3', '3'::oidvector < '1 2', '1 2'::int2vector = '1 2';
SELECT count(*) > 0 FROM pg_proc WHERE proargtypes <> '';
-- hash support functions
CREATE TYPE ga_mood AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE ga_pair AS (a int, b text);
CREATE TYPE ga_vb AS (a varbit, b text);
SELECT hash_range(int4range(1, 5)), hash_range_extended(int4range(1, 5), 0), hash_range_extended(int4range(1, 5), 1), hash_range('empty'::int4range), hash_range(numrange(1.5, NULL));
SELECT hash_multirange(int4multirange(int4range(1, 5), int4range(7, 9))), hash_multirange_extended(int4multirange(int4range(1, 5)), 0), hash_multirange('{}'::int4multirange);
SELECT hash_record(row(1, 'aaa')::ga_pair), hash_record_extended(row(1, 'aaa')::ga_pair, 0), hash_record_extended(row(NULL, 'b')::ga_pair, 7);
SELECT hash_record(row('10', 'aaa')::ga_vb);
SELECT hash_record_extended(row('10', 'aaa')::ga_vb, 0);
SELECT hash_array('{101}'::varbit[]);
SELECT hash_array_extended('{101}'::varbit[], 0);
SELECT v, hashenum(v)::bit(32) = hashenumextended(v, 0)::bit(32), hashenum(v)::bit(32) <> hashenumextended(v, 1)::bit(32) FROM unnest(enum_range(NULL::ga_mood)) v;
SELECT hash_aclitem(a)::bit(32) = hash_aclitem_extended(a, 0)::bit(32) FROM (SELECT relacl[1] a FROM pg_class WHERE relacl IS NOT NULL LIMIT 3) s;
DROP TYPE ga_vb;
DROP TYPE ga_pair;
-- new enum values can't be used in the transaction that added them
BEGIN;
ALTER TYPE ga_mood ADD VALUE 'new';
SAVEPOINT x;
SELECT 'new'::ga_mood;
ROLLBACK TO x;
SELECT enum_first(NULL::ga_mood);
SELECT enum_last(NULL::ga_mood);
ROLLBACK TO x;
SELECT enum_range(NULL::ga_mood);
ROLLBACK TO x;
SELECT enum_range('ok'::ga_mood, 'happy'::ga_mood);
COMMIT;
SELECT 'new'::ga_mood, enum_last(NULL::ga_mood);
BEGIN;
ALTER TYPE ga_mood RENAME VALUE 'sad' TO 'blue';
SELECT 'blue'::ga_mood;
ROLLBACK;
BEGIN;
CREATE TYPE ga_fresh AS ENUM ('a');
ALTER TYPE ga_fresh ADD VALUE 'b';
SELECT 'b'::ga_fresh, enum_range(NULL::ga_fresh);
ROLLBACK;
DROP TYPE ga_mood;
-- strict functions returning a record (not SETOF) give one NULL row for a NULL
SELECT * FROM pg_split_walfile_name(NULL);
SELECT pg_split_walfile_name(NULL) IS NULL;
SELECT * FROM abs(NULL::int);
-- replication origin names
SELECT pg_replication_origin_create('regress_' || repeat('a', 505));
-- JSON: an invalid token runs over non-ASCII characters
SELECT repeat(U&'\00A7', 30)::json;
SELECT '[1, 2, €€x]'::jsonb;
-- client_encoding
SET client_encoding TO latin1;
SHOW client_encoding;
SELECT 'abc';
SET client_encoding TO 'euc-jp';
SHOW client_encoding;
RESET client_encoding;
SHOW client_encoding;
SET client_encoding TO no_such_encoding;
-- pg_timezone_abbrevs: the session zone's own abbreviations
SET timezone TO 'America/Los_Angeles';
SELECT * FROM pg_timezone_abbrevs_zone() ORDER BY 1;
SELECT * FROM pg_timezone_abbrevs WHERE abbrev IN ('LMT', 'PST', 'PWT') ORDER BY 1;
RESET timezone;
-- system views
SELECT count(*) > 0 AS ok, count(*) FILTER (WHERE error IS NOT NULL) = 0 AS no_err FROM pg_hba_file_rules;
SELECT count(*) > 0 FROM pg_locks WHERE locktype = 'virtualxid' AND pid = pg_backend_pid();
WITH c AS (SELECT * FROM pg_backend_memory_contexts) SELECT count(*) > 1 FROM c c1, c c2 WHERE c2.name = 'CacheMemoryContext' AND c1.path[c2.level] = c2.path[c2.level];
SELECT name, level FROM pg_backend_memory_contexts WHERE level = 1;
-- currtid2
CREATE TABLE ga_tid_tab (a int);
SELECT currtid2('ga_tid_tab'::text, '(0,1)'::tid);
INSERT INTO ga_tid_tab VALUES (1);
SELECT currtid2('ga_tid_tab'::text, '(0,1)'::tid);
SELECT currtid2('ga_tid_tab'::text, '(0,0)'::tid);
CREATE VIEW ga_tid_view AS SELECT ctid, a FROM ga_tid_tab;
SELECT currtid2('ga_tid_view'::text, '(0,1)'::tid);
CREATE VIEW ga_tid_view2 AS SELECT a FROM ga_tid_tab;
SELECT currtid2('ga_tid_view2'::text, '(0,1)'::tid);
CREATE INDEX ga_tid_ind ON ga_tid_tab (a);
SELECT currtid2('ga_tid_ind'::text, '(0,1)'::tid);
SELECT currtid2('ga_no_such'::text, '(0,1)'::tid);
DROP VIEW ga_tid_view;
DROP VIEW ga_tid_view2;
-- TID scans and TID range scans
INSERT INTO ga_tid_tab VALUES (2), (3);
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE ctid = '(0,2)';
SELECT ctid, * FROM ga_tid_tab WHERE ctid = '(0,2)';
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE ctid = '(0,3)' OR '(0,1)' = ctid;
SELECT ctid, * FROM ga_tid_tab WHERE ctid = '(0,3)' OR '(0,1)' = ctid;
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE ctid = ANY (ARRAY['(0,1)', '(0,2)']::tid[]) AND a > 1;
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE (a = 3 AND ctid = '(0,3)') OR (ctid = '(0,1)' AND a = 1);
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE ctid <> '(0,1)';
EXPLAIN (COSTS OFF) SELECT ctid FROM ga_tid_tab WHERE ctid > '(0,1)' AND '(0,3)' >= ctid;
SELECT ctid FROM ga_tid_tab WHERE ctid > '(0,1)' AND '(0,3)' >= ctid;
SET enable_tidscan = off;
EXPLAIN (COSTS OFF) SELECT ctid, * FROM ga_tid_tab WHERE ctid = '(0,2)';
RESET enable_tidscan;
RESET enable_seqscan;
BEGIN;
DECLARE ga_c CURSOR FOR SELECT ctid, * FROM ga_tid_tab;
FETCH NEXT FROM ga_c;
UPDATE ga_tid_tab SET a = -a WHERE CURRENT OF ga_c RETURNING *;
COMMIT;
SELECT a FROM ga_tid_tab ORDER BY a;
DROP TABLE ga_tid_tab;
-- inet <<, <<= on a btree index
CREATE TABLE ga_inet (i inet);
INSERT INTO ga_inet VALUES ('192.168.1.5'), ('192.168.1.0/26'), ('10.0.0.1'), ('192.168.1.255');
CREATE INDEX ga_inet_idx ON ga_inet (i);
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT * FROM ga_inet WHERE i << '192.168.1.0/24'::cidr;
EXPLAIN (COSTS OFF) SELECT * FROM ga_inet WHERE '192.168.1.0/24'::cidr >>= i;
SELECT * FROM ga_inet WHERE i <<= '192.168.1.0/24'::cidr ORDER BY i;
RESET enable_seqscan;
DROP TABLE ga_inet;
-- plans show anytextcat's text cast, internal functions by their own name
EXPLAIN (VERBOSE, COSTS OFF) SELECT (i || '/' || j)::pg_lsn FROM generate_series(1, 2) i, generate_series(1, 2) j;
CREATE FUNCTION ga_int_eq(int, int) RETURNS bool LANGUAGE internal STRICT IMMUTABLE AS $$int4eq$$;
EXPLAIN (COSTS OFF) SELECT * FROM generate_series(1, 10) g WHERE ga_int_eq(g, 4);
SELECT * FROM generate_series(1, 10) g WHERE ga_int_eq(g, 4);
DROP FUNCTION ga_int_eq(int, int);
-- generate_series row estimates for numeric and timestamp bounds
EXPLAIN SELECT * FROM generate_series(1.0, 25.0, 2.0);
EXPLAIN SELECT * FROM generate_series(25.0, 1.0);
EXPLAIN SELECT * FROM generate_series('2024-02-01'::timestamp, '2024-03-01', '7 days');
EXPLAIN SELECT * FROM generate_series('2024-02-01'::timestamptz, '2024-03-01', '1 day', 'UTC');
-- EXPLAIN (FORMAT JSON): Inner Unique after the target list
EXPLAIN (COSTS OFF, VERBOSE, FORMAT JSON) SELECT * FROM generate_series(1, 2) a JOIN generate_series(1, 2) b ON a = b;
-- a child table's row passed for its parent's row type, a typed table's for its type
CREATE TABLE ga_p1 (ff1 int);
CREATE TABLE ga_p2 (f1 text, f2 int);
CREATE FUNCTION ga_p2text(ga_p2) RETURNS text AS 'SELECT $1.f1' LANGUAGE sql;
CREATE TABLE ga_c1 (f3 int) INHERITS (ga_p1, ga_p2);
INSERT INTO ga_c1 VALUES (123456789, 'hi', 42, 123455);
SELECT ga_p2text(ga_c1.*) FROM ga_c1;
SELECT ga_p2text(c) FROM ga_c1 c;
DROP TABLE ga_c1;
DROP FUNCTION ga_p2text(ga_p2);
DROP TABLE ga_p1, ga_p2;
CREATE TYPE ga_person AS (id int, name text);
CREATE TABLE ga_persons OF ga_person;
INSERT INTO ga_persons VALUES (1, 'test');
CREATE FUNCTION ga_namelen(ga_person) RETURNS int LANGUAGE sql AS $$ SELECT length($1.name) $$;
SELECT id, ga_namelen(ga_persons) FROM ga_persons;
DROP TABLE ga_persons;
DROP FUNCTION ga_namelen(ga_person);
DROP TYPE ga_person;
-- a SQL function returning SETOF a named composite keeps that type; (x).f calls f(x)
CREATE TABLE ga_hobbies (name text, person text);
CREATE TABLE ga_equipment (name text, hobby text);
INSERT INTO ga_hobbies VALUES ('posthacking', 'mike'), ('basketball', 'joe');
INSERT INTO ga_equipment VALUES ('advil', 'posthacking'), ('hightops', 'basketball');
CREATE FUNCTION ga_hobbies_of(text) RETURNS SETOF ga_hobbies AS 'SELECT * FROM ga_hobbies WHERE person = $1' LANGUAGE sql;
CREATE FUNCTION ga_equipment(ga_hobbies) RETURNS SETOF ga_equipment AS 'SELECT * FROM ga_equipment WHERE hobby = $1.name' LANGUAGE sql;
SELECT name(ga_equipment(ga_hobbies_of('mike')));
SELECT (ga_hobbies_of('joe')).ga_equipment.name;
SELECT pg_typeof(ga_hobbies_of('joe'));
DROP FUNCTION ga_equipment(ga_hobbies);
DROP FUNCTION ga_hobbies_of(text);
DROP TABLE ga_hobbies, ga_equipment;
-- statistics of a replication slot that has none
SELECT slot_name, total_txns FROM pg_stat_get_replication_slot('ga_nonexistent');
SELECT * FROM pg_stat_get_replication_slot(NULL);
