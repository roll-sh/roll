-- Difftest round 7 (agent 138): cookDefault's mismatch names the "default expression" (a
-- column's DEFAULT, a generation expression, a domain's DEFAULT); buffered binary COPY
-- refuses a row NOT NULL refuses before a later tuple's format error, as ExecConstraints
-- checks each row as it is read; the receive functions' leftover-bytes error in COPY doesn't
-- name a bind parameter. sum(float8) is float8pl alone: only avg / variance accumulate the
-- sum of squares that 1e300 overflows.
CREATE TABLE a138f (id int, f float8);
INSERT INTO a138f VALUES (1, 1e300), (2, 0.5);
SELECT sum(f) FROM a138f;
SELECT sum(f), count(f), max(f) FROM a138f;
SELECT avg(f) FROM a138f;
SELECT stddev(f) FROM a138f;
SELECT sum(f) FROM (VALUES (1e308::float8), (1e308)) v(f);
SELECT sum(f) FROM (VALUES ('Infinity'::float8), (1e308), (1e308)) v(f);
SELECT sum(f) FROM (VALUES ('-0'::float8), ('-0')) v(f);
-- float8in / float4in take strtod's hexadecimal constants.
SELECT '0x10'::float8, ' 0x1p3 '::float8, '-0X1.8p1'::float8, '0x.8'::float8, '0xAp-2'::float4;
SELECT '0x1.fffffffffffffp1023'::float8, '0x1p-1074'::float8, '0x0'::float8, '-0x0p0'::float8;
SELECT '0x123456789abcdef0123456789abcdef'::float8;
SELECT '0x1p1024'::float8;
SELECT '0x1p-1080'::float8;
SELECT '0x'::float8;
SELECT '0x1q'::float8;
SELECT '0x1p'::float8;
CREATE TABLE a138d1 (b bool DEFAULT 0);
CREATE TABLE a138d2 (b bool GENERATED ALWAYS AS (1) STORED);
CREATE TABLE a138d3 (b int, v text GENERATED ALWAYS AS (b + 1) VIRTUAL);
CREATE DOMAIN a138dd AS bool DEFAULT 0;
CREATE DOMAIN a138de AS bool;
ALTER DOMAIN a138de SET DEFAULT 1;
CREATE TABLE a138d4 (b int);
ALTER TABLE a138d4 ALTER COLUMN b SET DEFAULT now();
ALTER TABLE a138d4 ADD COLUMN c bool DEFAULT 0;
CREATE VIEW a138v AS SELECT b FROM a138d4;
ALTER VIEW a138v ALTER COLUMN b SET DEFAULT 'x'::text;
CREATE TABLE a138c (id int PRIMARY KEY, email text UNIQUE, name text, n int NOT NULL DEFAULT 0);
-- A NULL key, then a tuple whose int4 field has 8 bytes.
COPY a138c (id, email, name) FROM STDIN (FORMAT binary);
\binary 5047434f 50590aff 0d0a00 00000000 00000000 0003 ffffffff 00000001 61 00000001 62 0003 00000008 00000000 0000000f 00000001 63 00000001 64 ffff
\.
SELECT count(*) FROM a138c;
-- The tuple with 8 bytes first.
COPY a138c (id, email, name) FROM STDIN (FORMAT binary);
\binary 5047434f 50590aff 0d0a00 00000000 00000000 0003 00000008 00000000 0000000f 00000001 61 00000001 62 0003 ffffffff 00000001 63 00000001 64 ffff
\.
SELECT count(*) FROM a138c;
-- Good rows, then the NULL key: none of them go in.
COPY a138c (id, email, name) FROM STDIN (FORMAT binary);
\binary 5047434f 50590aff 0d0a00 00000000 00000000 0003 00000004 00000001 00000001 61 00000001 62 0003 ffffffff 00000001 63 00000001 64 0003 00000004 00000003 00000001 65 00000001 66 ffff
\.
SELECT count(*) FROM a138c;
-- Good rows, then a tuple whose int4 field has 2 bytes.
COPY a138c (id, email, name) FROM STDIN (FORMAT binary);
\binary 5047434f 50590aff 0d0a00 00000000 00000000 0003 00000004 00000001 00000001 61 00000001 62 0003 00000002 0001 00000001 63 00000001 64 ffff
\.
SELECT count(*) FROM a138c;
