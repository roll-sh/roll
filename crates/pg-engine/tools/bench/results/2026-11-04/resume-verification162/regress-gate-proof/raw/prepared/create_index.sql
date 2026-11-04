--
-- CREATE_INDEX
-- Create ancillary data structures (i.e. indices)
--

-- directory paths are passed to us in environment variables
\getenv abs_srcdir PG_ABS_SRCDIR
-- @@@ 0

--
-- BTREE
--
CREATE INDEX onek_unique1 ON onek USING btree(unique1 int4_ops);
-- @@@ 1

CREATE INDEX IF NOT EXISTS onek_unique1 ON onek USING btree(unique1 int4_ops);
-- @@@ 2

CREATE INDEX IF NOT EXISTS ON onek USING btree(unique1 int4_ops);
-- @@@ 3

CREATE INDEX onek_unique2 ON onek USING btree(unique2 int4_ops);
-- @@@ 4

CREATE INDEX onek_hundred ON onek USING btree(hundred int4_ops);
-- @@@ 5

CREATE INDEX onek_stringu1 ON onek USING btree(stringu1 name_ops);
-- @@@ 6

CREATE INDEX tenk1_unique1 ON tenk1 USING btree(unique1 int4_ops);
-- @@@ 7

CREATE INDEX tenk1_unique2 ON tenk1 USING btree(unique2 int4_ops);
-- @@@ 8

CREATE INDEX tenk1_hundred ON tenk1 USING btree(hundred int4_ops);
-- @@@ 9

CREATE INDEX tenk1_thous_tenthous ON tenk1 (thousand, tenthous);
-- @@@ 10

CREATE INDEX tenk2_unique1 ON tenk2 USING btree(unique1 int4_ops);
-- @@@ 11

CREATE INDEX tenk2_unique2 ON tenk2 USING btree(unique2 int4_ops);
-- @@@ 12

CREATE INDEX tenk2_hundred ON tenk2 USING btree(hundred int4_ops);
-- @@@ 13

CREATE INDEX rix ON road USING btree (name text_ops);
-- @@@ 14

CREATE INDEX iix ON ihighway USING btree (name text_ops);
-- @@@ 15

CREATE INDEX six ON shighway USING btree (name text_ops);
-- @@@ 16

-- test comments
COMMENT ON INDEX six_wrong IS 'bad index';
-- @@@ 17
COMMENT ON INDEX six IS 'good index';
-- @@@ 18
COMMENT ON INDEX six IS NULL;
-- @@@ 19
SELECT obj_description('six'::regclass, 'pg_class') IS NULL AS six_comment_is_null;
-- @@@ 20
COMMENT ON INDEX six IS 'add the comment back';
-- @@@ 21
COMMENT ON INDEX six IS ''; -- empty string removes the comment, same as NULL
-- @@@ 22
SELECT obj_description('six'::regclass, 'pg_class') IS NULL AS six_comment_is_null;
-- @@@ 23

--
-- BTREE partial indices
--
CREATE INDEX onek2_u1_prtl ON onek2 USING btree(unique1 int4_ops)
	where unique1 < 20 or unique1 > 980;
-- @@@ 24

CREATE INDEX onek2_u2_prtl ON onek2 USING btree(unique2 int4_ops)
	where stringu1 < 'B';
-- @@@ 25

CREATE INDEX onek2_stu1_prtl ON onek2 USING btree(stringu1 name_ops)
	where onek2.stringu1 >= 'J' and onek2.stringu1 < 'K';
-- @@@ 26

--
-- GiST (rtree-equivalent opclasses only)
--

CREATE TABLE slow_emp4000 (
	home_base	 box
);
-- @@@ 27

CREATE TABLE fast_emp4000 (
	home_base	 box
);
-- @@@ 28

\set filename :abs_srcdir '/data/rect.data'
-- @@@ 29
COPY slow_emp4000 FROM :'filename';
-- @@@ 30

INSERT INTO fast_emp4000 SELECT * FROM slow_emp4000;
-- @@@ 31

ANALYZE slow_emp4000;
-- @@@ 32
ANALYZE fast_emp4000;
-- @@@ 33

CREATE INDEX grect2ind ON fast_emp4000 USING gist (home_base);
-- @@@ 34

-- we want to work with a point_tbl that includes a null
CREATE TEMP TABLE point_tbl AS SELECT * FROM public.point_tbl;
-- @@@ 35
INSERT INTO POINT_TBL(f1) VALUES (NULL);
-- @@@ 36

CREATE INDEX gpointind ON point_tbl USING gist (f1);
-- @@@ 37

CREATE TEMP TABLE gpolygon_tbl AS
    SELECT polygon(home_base) AS f1 FROM slow_emp4000;
-- @@@ 38
INSERT INTO gpolygon_tbl VALUES ( '(1000,0,0,1000)' );
-- @@@ 39
INSERT INTO gpolygon_tbl VALUES ( '(0,1000,1000,1000)' );
-- @@@ 40

CREATE TEMP TABLE gcircle_tbl AS
    SELECT circle(home_base) AS f1 FROM slow_emp4000;
-- @@@ 41

CREATE INDEX ggpolygonind ON gpolygon_tbl USING gist (f1);
-- @@@ 42

CREATE INDEX ggcircleind ON gcircle_tbl USING gist (f1);
-- @@@ 43

--
-- Test GiST indexes
--

-- get non-indexed results for comparison purposes

SET enable_seqscan = ON;
-- @@@ 44
SET enable_indexscan = OFF;
-- @@@ 45
SET enable_bitmapscan = OFF;
-- @@@ 46

SELECT * FROM fast_emp4000
    WHERE home_base <@ '(200,200),(2000,1000)'::box
    ORDER BY (home_base[0])[0];
-- @@@ 47

SELECT count(*) FROM fast_emp4000 WHERE home_base && '(1000,1000,0,0)'::box;
-- @@@ 48

SELECT count(*) FROM fast_emp4000 WHERE home_base IS NULL;
-- @@@ 49

SELECT count(*) FROM gpolygon_tbl WHERE f1 && '(1000,1000,0,0)'::polygon;
-- @@@ 50

SELECT count(*) FROM gcircle_tbl WHERE f1 && '<(500,500),500>'::circle;
-- @@@ 51

SELECT count(*) FROM point_tbl WHERE f1 <@ box '(0,0,100,100)';
-- @@@ 52

SELECT count(*) FROM point_tbl WHERE box '(0,0,100,100)' @> f1;
-- @@@ 53

SELECT count(*) FROM point_tbl WHERE f1 <@ polygon '(0,0),(0,100),(100,100),(50,50),(100,0),(0,0)';
-- @@@ 54

SELECT count(*) FROM point_tbl WHERE f1 <@ circle '<(50,50),50>';
-- @@@ 55

SELECT count(*) FROM point_tbl p WHERE p.f1 << '(0.0, 0.0)';
-- @@@ 56

SELECT count(*) FROM point_tbl p WHERE p.f1 >> '(0.0, 0.0)';
-- @@@ 57

SELECT count(*) FROM point_tbl p WHERE p.f1 <<| '(0.0, 0.0)';
-- @@@ 58

SELECT count(*) FROM point_tbl p WHERE p.f1 |>> '(0.0, 0.0)';
-- @@@ 59

SELECT count(*) FROM point_tbl p WHERE p.f1 ~= '(-5, -12)';
-- @@@ 60

SELECT * FROM point_tbl ORDER BY f1 <-> '0,1';
-- @@@ 61

SELECT * FROM point_tbl WHERE f1 IS NULL;
-- @@@ 62

SELECT * FROM point_tbl WHERE f1 IS NOT NULL ORDER BY f1 <-> '0,1';
-- @@@ 63

SELECT * FROM point_tbl WHERE f1 <@ '(-10,-10),(10,10)':: box ORDER BY f1 <-> '0,1';
-- @@@ 64

SELECT * FROM gpolygon_tbl ORDER BY f1 <-> '(0,0)'::point LIMIT 10;
-- @@@ 65

SELECT circle_center(f1), round(radius(f1)) as radius FROM gcircle_tbl ORDER BY f1 <-> '(200,300)'::point LIMIT 10;
-- @@@ 66

-- Now check the results from plain indexscan
SET enable_seqscan = OFF;
-- @@@ 67
SET enable_indexscan = ON;
-- @@@ 68
SET enable_bitmapscan = OFF;
-- @@@ 69

EXPLAIN (COSTS OFF)
SELECT * FROM fast_emp4000
    WHERE home_base <@ '(200,200),(2000,1000)'::box
    ORDER BY (home_base[0])[0];
-- @@@ 70
SELECT * FROM fast_emp4000
    WHERE home_base <@ '(200,200),(2000,1000)'::box
    ORDER BY (home_base[0])[0];
-- @@@ 71

EXPLAIN (COSTS OFF)
SELECT count(*) FROM fast_emp4000 WHERE home_base && '(1000,1000,0,0)'::box;
-- @@@ 72
SELECT count(*) FROM fast_emp4000 WHERE home_base && '(1000,1000,0,0)'::box;
-- @@@ 73

EXPLAIN (COSTS OFF)
SELECT count(*) FROM fast_emp4000 WHERE home_base IS NULL;
-- @@@ 74
SELECT count(*) FROM fast_emp4000 WHERE home_base IS NULL;
-- @@@ 75

EXPLAIN (COSTS OFF)
SELECT count(*) FROM gpolygon_tbl WHERE f1 && '(1000,1000,0,0)'::polygon;
-- @@@ 76
SELECT count(*) FROM gpolygon_tbl WHERE f1 && '(1000,1000,0,0)'::polygon;
-- @@@ 77

EXPLAIN (COSTS OFF)
SELECT count(*) FROM gcircle_tbl WHERE f1 && '<(500,500),500>'::circle;
-- @@@ 78
SELECT count(*) FROM gcircle_tbl WHERE f1 && '<(500,500),500>'::circle;
-- @@@ 79

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl WHERE f1 <@ box '(0,0,100,100)';
-- @@@ 80
SELECT count(*) FROM point_tbl WHERE f1 <@ box '(0,0,100,100)';
-- @@@ 81

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl WHERE box '(0,0,100,100)' @> f1;
-- @@@ 82
SELECT count(*) FROM point_tbl WHERE box '(0,0,100,100)' @> f1;
-- @@@ 83

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl WHERE f1 <@ polygon '(0,0),(0,100),(100,100),(50,50),(100,0),(0,0)';
-- @@@ 84
SELECT count(*) FROM point_tbl WHERE f1 <@ polygon '(0,0),(0,100),(100,100),(50,50),(100,0),(0,0)';
-- @@@ 85

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl WHERE f1 <@ circle '<(50,50),50>';
-- @@@ 86
SELECT count(*) FROM point_tbl WHERE f1 <@ circle '<(50,50),50>';
-- @@@ 87

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl p WHERE p.f1 << '(0.0, 0.0)';
-- @@@ 88
SELECT count(*) FROM point_tbl p WHERE p.f1 << '(0.0, 0.0)';
-- @@@ 89

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl p WHERE p.f1 >> '(0.0, 0.0)';
-- @@@ 90
SELECT count(*) FROM point_tbl p WHERE p.f1 >> '(0.0, 0.0)';
-- @@@ 91

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl p WHERE p.f1 <<| '(0.0, 0.0)';
-- @@@ 92
SELECT count(*) FROM point_tbl p WHERE p.f1 <<| '(0.0, 0.0)';
-- @@@ 93

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl p WHERE p.f1 |>> '(0.0, 0.0)';
-- @@@ 94
SELECT count(*) FROM point_tbl p WHERE p.f1 |>> '(0.0, 0.0)';
-- @@@ 95

EXPLAIN (COSTS OFF)
SELECT count(*) FROM point_tbl p WHERE p.f1 ~= '(-5, -12)';
-- @@@ 96
SELECT count(*) FROM point_tbl p WHERE p.f1 ~= '(-5, -12)';
-- @@@ 97

EXPLAIN (COSTS OFF)
SELECT * FROM point_tbl ORDER BY f1 <-> '0,1';
-- @@@ 98
SELECT * FROM point_tbl ORDER BY f1 <-> '0,1';
-- @@@ 99

EXPLAIN (COSTS OFF)
SELECT * FROM point_tbl WHERE f1 IS NULL;
-- @@@ 100
SELECT * FROM point_tbl WHERE f1 IS NULL;
-- @@@ 101

EXPLAIN (COSTS OFF)
SELECT * FROM point_tbl WHERE f1 IS NOT NULL ORDER BY f1 <-> '0,1';
-- @@@ 102
SELECT * FROM point_tbl WHERE f1 IS NOT NULL ORDER BY f1 <-> '0,1';
-- @@@ 103

EXPLAIN (COSTS OFF)
SELECT * FROM point_tbl WHERE f1 <@ '(-10,-10),(10,10)':: box ORDER BY f1 <-> '0,1';
-- @@@ 104
SELECT * FROM point_tbl WHERE f1 <@ '(-10,-10),(10,10)':: box ORDER BY f1 <-> '0,1';
-- @@@ 105

EXPLAIN (COSTS OFF)
SELECT * FROM gpolygon_tbl ORDER BY f1 <-> '(0,0)'::point LIMIT 10;
-- @@@ 106
SELECT * FROM gpolygon_tbl ORDER BY f1 <-> '(0,0)'::point LIMIT 10;
-- @@@ 107

EXPLAIN (COSTS OFF)
SELECT circle_center(f1), round(radius(f1)) as radius FROM gcircle_tbl ORDER BY f1 <-> '(200,300)'::point LIMIT 10;
-- @@@ 108
SELECT circle_center(f1), round(radius(f1)) as radius FROM gcircle_tbl ORDER BY f1 <-> '(200,300)'::point LIMIT 10;
-- @@@ 109

EXPLAIN (COSTS OFF)
SELECT point(x,x), (SELECT f1 FROM gpolygon_tbl ORDER BY f1 <-> point(x,x) LIMIT 1) as c FROM generate_series(0,10,1) x;
-- @@@ 110
SELECT point(x,x), (SELECT f1 FROM gpolygon_tbl ORDER BY f1 <-> point(x,x) LIMIT 1) as c FROM generate_series(0,10,1) x;
-- @@@ 111

-- Now check the results from bitmap indexscan
SET enable_seqscan = OFF;
-- @@@ 112
SET enable_indexscan = OFF;
-- @@@ 113
SET enable_bitmapscan = ON;
-- @@@ 114

EXPLAIN (COSTS OFF)
SELECT * FROM point_tbl WHERE f1 <@ '(-10,-10),(10,10)':: box ORDER BY f1 <-> '0,1';
-- @@@ 115
SELECT * FROM point_tbl WHERE f1 <@ '(-10,-10),(10,10)':: box ORDER BY f1 <-> '0,1';
-- @@@ 116

RESET enable_seqscan;
-- @@@ 117
RESET enable_indexscan;
-- @@@ 118
RESET enable_bitmapscan;
-- @@@ 119

--
-- GIN over int[] and text[]
--
-- Note: GIN currently supports only bitmap scans, not plain indexscans
--

CREATE TABLE array_index_op_test (
	seqno		int4,
	i			int4[],
	t			text[]
);
-- @@@ 120

\set filename :abs_srcdir '/data/array.data'
-- @@@ 121
COPY array_index_op_test FROM :'filename';
-- @@@ 122
ANALYZE array_index_op_test;
-- @@@ 123

SELECT * FROM array_index_op_test WHERE i = '{NULL}' ORDER BY seqno;
-- @@@ 124
SELECT * FROM array_index_op_test WHERE i @> '{NULL}' ORDER BY seqno;
-- @@@ 125
SELECT * FROM array_index_op_test WHERE i && '{NULL}' ORDER BY seqno;
-- @@@ 126
SELECT * FROM array_index_op_test WHERE i <@ '{NULL}' ORDER BY seqno;
-- @@@ 127

SET enable_seqscan = OFF;
-- @@@ 128
SET enable_indexscan = OFF;
-- @@@ 129
SET enable_bitmapscan = ON;
-- @@@ 130

CREATE INDEX intarrayidx ON array_index_op_test USING gin (i);
-- @@@ 131

explain (costs off)
SELECT * FROM array_index_op_test WHERE i @> '{32}' ORDER BY seqno;
-- @@@ 132

SELECT * FROM array_index_op_test WHERE i @> '{32}' ORDER BY seqno;
-- @@@ 133
SELECT * FROM array_index_op_test WHERE i && '{32}' ORDER BY seqno;
-- @@@ 134
SELECT * FROM array_index_op_test WHERE i @> '{17}' ORDER BY seqno;
-- @@@ 135
SELECT * FROM array_index_op_test WHERE i && '{17}' ORDER BY seqno;
-- @@@ 136
SELECT * FROM array_index_op_test WHERE i @> '{32,17}' ORDER BY seqno;
-- @@@ 137
SELECT * FROM array_index_op_test WHERE i && '{32,17}' ORDER BY seqno;
-- @@@ 138
SELECT * FROM array_index_op_test WHERE i <@ '{38,34,32,89}' ORDER BY seqno;
-- @@@ 139
SELECT * FROM array_index_op_test WHERE i = '{47,77}' ORDER BY seqno;
-- @@@ 140
SELECT * FROM array_index_op_test WHERE i = '{}' ORDER BY seqno;
-- @@@ 141
SELECT * FROM array_index_op_test WHERE i @> '{}' ORDER BY seqno;
-- @@@ 142
SELECT * FROM array_index_op_test WHERE i && '{}' ORDER BY seqno;
-- @@@ 143
SELECT * FROM array_index_op_test WHERE i <@ '{}' ORDER BY seqno;
-- @@@ 144

CREATE INDEX textarrayidx ON array_index_op_test USING gin (t);
-- @@@ 145

explain (costs off)
SELECT * FROM array_index_op_test WHERE t @> '{AAAAAAAA72908}' ORDER BY seqno;
-- @@@ 146

SELECT * FROM array_index_op_test WHERE t @> '{AAAAAAAA72908}' ORDER BY seqno;
-- @@@ 147
SELECT * FROM array_index_op_test WHERE t && '{AAAAAAAA72908}' ORDER BY seqno;
-- @@@ 148
SELECT * FROM array_index_op_test WHERE t @> '{AAAAAAAAAA646}' ORDER BY seqno;
-- @@@ 149
SELECT * FROM array_index_op_test WHERE t && '{AAAAAAAAAA646}' ORDER BY seqno;
-- @@@ 150
SELECT * FROM array_index_op_test WHERE t @> '{AAAAAAAA72908,AAAAAAAAAA646}' ORDER BY seqno;
-- @@@ 151
SELECT * FROM array_index_op_test WHERE t && '{AAAAAAAA72908,AAAAAAAAAA646}' ORDER BY seqno;
-- @@@ 152
SELECT * FROM array_index_op_test WHERE t <@ '{AAAAAAAA72908,AAAAAAAAAAAAAAAAAAA17075,AA88409,AAAAAAAAAAAAAAAAAA36842,AAAAAAA48038,AAAAAAAAAAAAAA10611}' ORDER BY seqno;
-- @@@ 153
SELECT * FROM array_index_op_test WHERE t = '{AAAAAAAAAA646,A87088}' ORDER BY seqno;
-- @@@ 154
SELECT * FROM array_index_op_test WHERE t = '{}' ORDER BY seqno;
-- @@@ 155
SELECT * FROM array_index_op_test WHERE t @> '{}' ORDER BY seqno;
-- @@@ 156
SELECT * FROM array_index_op_test WHERE t && '{}' ORDER BY seqno;
-- @@@ 157
SELECT * FROM array_index_op_test WHERE t <@ '{}' ORDER BY seqno;
-- @@@ 158

-- And try it with a multicolumn GIN index

DROP INDEX intarrayidx, textarrayidx;
-- @@@ 159

CREATE INDEX botharrayidx ON array_index_op_test USING gin (i, t);
-- @@@ 160

SELECT * FROM array_index_op_test WHERE i @> '{32}' ORDER BY seqno;
-- @@@ 161
SELECT * FROM array_index_op_test WHERE i && '{32}' ORDER BY seqno;
-- @@@ 162
SELECT * FROM array_index_op_test WHERE t @> '{AAAAAAA80240}' ORDER BY seqno;
-- @@@ 163
SELECT * FROM array_index_op_test WHERE t && '{AAAAAAA80240}' ORDER BY seqno;
-- @@@ 164
SELECT * FROM array_index_op_test WHERE i @> '{32}' AND t && '{AAAAAAA80240}' ORDER BY seqno;
-- @@@ 165
SELECT * FROM array_index_op_test WHERE i && '{32}' AND t @> '{AAAAAAA80240}' ORDER BY seqno;
-- @@@ 166
SELECT * FROM array_index_op_test WHERE t = '{}' ORDER BY seqno;
-- @@@ 167

RESET enable_seqscan;
-- @@@ 168
RESET enable_indexscan;
-- @@@ 169
RESET enable_bitmapscan;
-- @@@ 170

--
-- Try a GIN index with a lot of items with same key. (GIN creates a posting
-- tree when there are enough duplicates)
--
CREATE TABLE array_gin_test (a int[]);
-- @@@ 171

INSERT INTO array_gin_test SELECT ARRAY[1, g%5, g] FROM generate_series(1, 10000) g;
-- @@@ 172

CREATE INDEX array_gin_test_idx ON array_gin_test USING gin (a);
-- @@@ 173

SELECT COUNT(*) FROM array_gin_test WHERE a @> '{2}';
-- @@@ 174

DROP TABLE array_gin_test;
-- @@@ 175

--
-- Test GIN index's reloptions
--
CREATE INDEX gin_relopts_test ON array_index_op_test USING gin (i)
  WITH (FASTUPDATE=on, GIN_PENDING_LIST_LIMIT=128);
-- @@@ 176
\d+ gin_relopts_test
-- @@@ 177

--
-- HASH
--
CREATE UNLOGGED TABLE unlogged_hash_table (id int4);
-- @@@ 178
CREATE INDEX unlogged_hash_index ON unlogged_hash_table USING hash (id int4_ops);
-- @@@ 179
DROP TABLE unlogged_hash_table;
-- @@@ 180

-- CREATE INDEX hash_ovfl_index ON hash_ovfl_heap USING hash (x int4_ops);

-- Test hash index build tuplesorting.  Force hash tuplesort using low
-- maintenance_work_mem setting and fillfactor:
SET maintenance_work_mem = '1MB';
-- @@@ 181
CREATE INDEX hash_tuplesort_idx ON tenk1 USING hash (stringu1 name_ops) WITH (fillfactor = 10);
-- @@@ 182
EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1 WHERE stringu1 = 'TVAAAA';
-- @@@ 183
SELECT count(*) FROM tenk1 WHERE stringu1 = 'TVAAAA';
-- @@@ 184
-- OR-clauses shouldn't be transformed into SAOP because hash indexes don't
-- support SAOP scans.
SET enable_seqscan = off;
-- @@@ 185
EXPLAIN (COSTS OFF)
SELECT COUNT(*) FROM tenk1 WHERE stringu1 = 'TVAAAA' OR  stringu1 = 'TVAAAB';
-- @@@ 186
RESET enable_seqscan;
-- @@@ 187
DROP INDEX hash_tuplesort_idx;
-- @@@ 188
RESET maintenance_work_mem;
-- @@@ 189


--
-- Test unique null behavior
--
CREATE TABLE unique_tbl (i int, t text);
-- @@@ 190

CREATE UNIQUE INDEX unique_idx1 ON unique_tbl (i) NULLS DISTINCT;
-- @@@ 191
CREATE UNIQUE INDEX unique_idx2 ON unique_tbl (i) NULLS NOT DISTINCT;
-- @@@ 192

INSERT INTO unique_tbl VALUES (1, 'one');
-- @@@ 193
INSERT INTO unique_tbl VALUES (2, 'two');
-- @@@ 194
INSERT INTO unique_tbl VALUES (3, 'three');
-- @@@ 195
INSERT INTO unique_tbl VALUES (4, 'four');
-- @@@ 196
INSERT INTO unique_tbl VALUES (5, 'one');
-- @@@ 197
INSERT INTO unique_tbl (t) VALUES ('six');
-- @@@ 198
INSERT INTO unique_tbl (t) VALUES ('seven');  -- error from unique_idx2
-- @@@ 199

DROP INDEX unique_idx1, unique_idx2;
-- @@@ 200

INSERT INTO unique_tbl (t) VALUES ('seven');
-- @@@ 201

-- build indexes on filled table
CREATE UNIQUE INDEX unique_idx3 ON unique_tbl (i) NULLS DISTINCT;  -- ok
-- @@@ 202
CREATE UNIQUE INDEX unique_idx4 ON unique_tbl (i) NULLS NOT DISTINCT;  -- error
-- @@@ 203

DELETE FROM unique_tbl WHERE t = 'seven';
-- @@@ 204

CREATE UNIQUE INDEX unique_idx4 ON unique_tbl (i) NULLS NOT DISTINCT;  -- ok now
-- @@@ 205

\d unique_tbl
-- @@@ 206
\d unique_idx3
-- @@@ 207
\d unique_idx4
-- @@@ 208
SELECT pg_get_indexdef('unique_idx3'::regclass);
-- @@@ 209
SELECT pg_get_indexdef('unique_idx4'::regclass);
-- @@@ 210

DROP TABLE unique_tbl;
-- @@@ 211


--
-- Test functional index
--
CREATE TABLE func_index_heap (f1 text, f2 text);
-- @@@ 212
CREATE UNIQUE INDEX func_index_index on func_index_heap (textcat(f1,f2));
-- @@@ 213

INSERT INTO func_index_heap VALUES('ABC','DEF');
-- @@@ 214
INSERT INTO func_index_heap VALUES('AB','CDEFG');
-- @@@ 215
INSERT INTO func_index_heap VALUES('QWE','RTY');
-- @@@ 216
-- this should fail because of unique index:
INSERT INTO func_index_heap VALUES('ABCD', 'EF');
-- @@@ 217
-- but this shouldn't:
INSERT INTO func_index_heap VALUES('QWERTY');
-- @@@ 218

-- while we're here, see that the metadata looks sane
\d func_index_heap
-- @@@ 219
\d func_index_index
-- @@@ 220


--
-- Same test, expressional index
--
DROP TABLE func_index_heap;
-- @@@ 221
CREATE TABLE func_index_heap (f1 text, f2 text);
-- @@@ 222
CREATE UNIQUE INDEX func_index_index on func_index_heap ((f1 || f2) text_ops);
-- @@@ 223

INSERT INTO func_index_heap VALUES('ABC','DEF');
-- @@@ 224
INSERT INTO func_index_heap VALUES('AB','CDEFG');
-- @@@ 225
INSERT INTO func_index_heap VALUES('QWE','RTY');
-- @@@ 226
-- this should fail because of unique index:
INSERT INTO func_index_heap VALUES('ABCD', 'EF');
-- @@@ 227
-- but this shouldn't:
INSERT INTO func_index_heap VALUES('QWERTY');
-- @@@ 228

-- while we're here, see that the metadata looks sane
\d func_index_heap
-- @@@ 229
\d func_index_index
-- @@@ 230

-- this should fail because of unsafe column type (anonymous record)
create index on func_index_heap ((f1 || f2), (row(f1, f2)));
-- @@@ 231


--
-- Test unique index with included columns
--
CREATE TABLE covering_index_heap (f1 int, f2 int, f3 text);
-- @@@ 232
CREATE UNIQUE INDEX covering_index_index on covering_index_heap (f1,f2) INCLUDE(f3);
-- @@@ 233

INSERT INTO covering_index_heap VALUES(1,1,'AAA');
-- @@@ 234
INSERT INTO covering_index_heap VALUES(1,2,'AAA');
-- @@@ 235
-- this should fail because of unique index on f1,f2:
INSERT INTO covering_index_heap VALUES(1,2,'BBB');
-- @@@ 236
-- and this shouldn't:
INSERT INTO covering_index_heap VALUES(1,4,'AAA');
-- @@@ 237
-- Try to build index on table that already contains data
CREATE UNIQUE INDEX covering_pkey on covering_index_heap (f1,f2) INCLUDE(f3);
-- @@@ 238
-- Try to use existing covering index as primary key
ALTER TABLE covering_index_heap ADD CONSTRAINT covering_pkey PRIMARY KEY USING INDEX
covering_pkey;
-- @@@ 239
DROP TABLE covering_index_heap;
-- @@@ 240

--
-- Try some concurrent index builds
--
-- Unfortunately this only tests about half the code paths because there are
-- no concurrent updates happening to the table at the same time.

CREATE TABLE concur_heap (f1 text, f2 text);
-- @@@ 241
-- empty table
CREATE INDEX CONCURRENTLY concur_index1 ON concur_heap(f2,f1);
-- @@@ 242
CREATE INDEX CONCURRENTLY IF NOT EXISTS concur_index1 ON concur_heap(f2,f1);
-- @@@ 243
INSERT INTO concur_heap VALUES  ('a','b');
-- @@@ 244
INSERT INTO concur_heap VALUES  ('b','b');
-- @@@ 245
-- unique index
CREATE UNIQUE INDEX CONCURRENTLY concur_index2 ON concur_heap(f1);
-- @@@ 246
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS concur_index2 ON concur_heap(f1);
-- @@@ 247
-- check if constraint is set up properly to be enforced
INSERT INTO concur_heap VALUES ('b','x');
-- @@@ 248
-- check if constraint is enforced properly at build time
CREATE UNIQUE INDEX CONCURRENTLY concur_index3 ON concur_heap(f2);
-- @@@ 249
-- test that expression indexes and partial indexes work concurrently
CREATE INDEX CONCURRENTLY concur_index4 on concur_heap(f2) WHERE f1='a';
-- @@@ 250
CREATE INDEX CONCURRENTLY concur_index5 on concur_heap(f2) WHERE f1='x';
-- @@@ 251
-- here we also check that you can default the index name
CREATE INDEX CONCURRENTLY on concur_heap((f2||f1));
-- @@@ 252
-- You can't do a concurrent index build in a transaction
BEGIN;
-- @@@ 253
CREATE INDEX CONCURRENTLY concur_index7 ON concur_heap(f1);
-- @@@ 254
COMMIT;
-- @@@ 255
-- test where predicate is able to do a transactional update during
-- a concurrent build before switching pg_index state flags.
CREATE FUNCTION predicate_stable() RETURNS bool IMMUTABLE
LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'SELECT txid_current()';
  RETURN true;
END; $$;
-- @@@ 256
CREATE INDEX CONCURRENTLY concur_index8 ON concur_heap (f1)
  WHERE predicate_stable();
-- @@@ 257
DROP INDEX concur_index8;
-- @@@ 258
DROP FUNCTION predicate_stable();
-- @@@ 259

-- But you can do a regular index build in a transaction
BEGIN;
-- @@@ 260
CREATE INDEX std_index on concur_heap(f2);
-- @@@ 261
COMMIT;
-- @@@ 262

-- Failed builds are left invalid by VACUUM FULL, fixed by REINDEX
VACUUM FULL concur_heap;
-- @@@ 263
REINDEX TABLE concur_heap;
-- @@@ 264
DELETE FROM concur_heap WHERE f1 = 'b';
-- @@@ 265
VACUUM FULL concur_heap;
-- @@@ 266
\d concur_heap
-- @@@ 267
REINDEX TABLE concur_heap;
-- @@@ 268
\d concur_heap
-- @@@ 269

-- Temporary tables with concurrent builds and on-commit actions
-- CONCURRENTLY used with CREATE INDEX and DROP INDEX is ignored.
-- PRESERVE ROWS, the default.
CREATE TEMP TABLE concur_temp (f1 int, f2 text)
  ON COMMIT PRESERVE ROWS;
-- @@@ 270
INSERT INTO concur_temp VALUES (1, 'foo'), (2, 'bar');
-- @@@ 271
CREATE INDEX CONCURRENTLY concur_temp_ind ON concur_temp(f1);
-- @@@ 272
DROP INDEX CONCURRENTLY concur_temp_ind;
-- @@@ 273
DROP TABLE concur_temp;
-- @@@ 274
-- ON COMMIT DROP
BEGIN;
-- @@@ 275
CREATE TEMP TABLE concur_temp (f1 int, f2 text)
  ON COMMIT DROP;
-- @@@ 276
INSERT INTO concur_temp VALUES (1, 'foo'), (2, 'bar');
-- @@@ 277
-- Fails when running in a transaction.
CREATE INDEX CONCURRENTLY concur_temp_ind ON concur_temp(f1);
-- @@@ 278
COMMIT;
-- @@@ 279
-- ON COMMIT DELETE ROWS
CREATE TEMP TABLE concur_temp (f1 int, f2 text)
  ON COMMIT DELETE ROWS;
-- @@@ 280
INSERT INTO concur_temp VALUES (1, 'foo'), (2, 'bar');
-- @@@ 281
CREATE INDEX CONCURRENTLY concur_temp_ind ON concur_temp(f1);
-- @@@ 282
DROP INDEX CONCURRENTLY concur_temp_ind;
-- @@@ 283
DROP TABLE concur_temp;
-- @@@ 284

--
-- Try some concurrent index drops
--
DROP INDEX CONCURRENTLY "concur_index2";				-- works
-- @@@ 285
DROP INDEX CONCURRENTLY IF EXISTS "concur_index2";		-- notice
-- @@@ 286

-- failures
DROP INDEX CONCURRENTLY "concur_index2", "concur_index3";
-- @@@ 287
BEGIN;
-- @@@ 288
DROP INDEX CONCURRENTLY "concur_index5";
-- @@@ 289
ROLLBACK;
-- @@@ 290

-- successes
DROP INDEX CONCURRENTLY IF EXISTS "concur_index3";
-- @@@ 291
DROP INDEX CONCURRENTLY "concur_index4";
-- @@@ 292
DROP INDEX CONCURRENTLY "concur_index5";
-- @@@ 293
DROP INDEX CONCURRENTLY "concur_index1";
-- @@@ 294
DROP INDEX CONCURRENTLY "concur_heap_expr_idx";
-- @@@ 295

\d concur_heap
-- @@@ 296

DROP TABLE concur_heap;
-- @@@ 297

--
-- Test ADD CONSTRAINT USING INDEX
--

CREATE TABLE cwi_test( a int , b varchar(10), c char);
-- @@@ 298

-- add some data so that all tests have something to work with.

INSERT INTO cwi_test VALUES(1, 2), (3, 4), (5, 6);
-- @@@ 299

CREATE UNIQUE INDEX cwi_uniq_idx ON cwi_test(a , b);
-- @@@ 300
ALTER TABLE cwi_test ADD primary key USING INDEX cwi_uniq_idx;
-- @@@ 301

\d cwi_test
-- @@@ 302
\d cwi_uniq_idx
-- @@@ 303

CREATE UNIQUE INDEX cwi_uniq2_idx ON cwi_test(b , a);
-- @@@ 304
ALTER TABLE cwi_test DROP CONSTRAINT cwi_uniq_idx,
	ADD CONSTRAINT cwi_replaced_pkey PRIMARY KEY
		USING INDEX cwi_uniq2_idx;
-- @@@ 305

\d cwi_test
-- @@@ 306
\d cwi_replaced_pkey
-- @@@ 307

DROP INDEX cwi_replaced_pkey;	-- Should fail; a constraint depends on it
-- @@@ 308

-- Check that non-default index options are rejected
CREATE UNIQUE INDEX cwi_uniq3_idx ON cwi_test(a desc);
-- @@@ 309
ALTER TABLE cwi_test ADD UNIQUE USING INDEX cwi_uniq3_idx;  -- fail
-- @@@ 310
CREATE UNIQUE INDEX cwi_uniq4_idx ON cwi_test(b collate "POSIX");
-- @@@ 311
ALTER TABLE cwi_test ADD UNIQUE USING INDEX cwi_uniq4_idx;  -- fail
-- @@@ 312

DROP TABLE cwi_test;
-- @@@ 313

-- ADD CONSTRAINT USING INDEX is forbidden on partitioned tables
CREATE TABLE cwi_test(a int) PARTITION BY hash (a);
-- @@@ 314
create unique index on cwi_test (a);
-- @@@ 315
alter table cwi_test add primary key using index cwi_test_a_idx ;
-- @@@ 316
DROP TABLE cwi_test;
-- @@@ 317

-- PRIMARY KEY constraint cannot be backed by a NULLS NOT DISTINCT index
CREATE TABLE cwi_test(a int, b int);
-- @@@ 318
CREATE UNIQUE INDEX cwi_a_nnd ON cwi_test (a) NULLS NOT DISTINCT;
-- @@@ 319
ALTER TABLE cwi_test ADD PRIMARY KEY USING INDEX cwi_a_nnd;
-- @@@ 320
DROP TABLE cwi_test;
-- @@@ 321

--
-- Check handling of indexes on system columns
--
CREATE TABLE syscol_table (a INT);
-- @@@ 322

-- System columns cannot be indexed
CREATE INDEX ON syscol_table (ctid);
-- @@@ 323

-- nor used in expressions
CREATE INDEX ON syscol_table ((ctid >= '(1000,0)'));
-- @@@ 324

-- nor used in predicates
CREATE INDEX ON syscol_table (a) WHERE ctid >= '(1000,0)';
-- @@@ 325

DROP TABLE syscol_table;
-- @@@ 326

--
-- Tests for IS NULL/IS NOT NULL with b-tree indexes
--

CREATE TABLE onek_with_null AS SELECT unique1, unique2 FROM onek;
-- @@@ 327
INSERT INTO onek_with_null(unique1, unique2)
VALUES (NULL, -1), (NULL, 2_147_483_647), (NULL, NULL),
       (100, NULL), (500, NULL);
-- @@@ 328
CREATE UNIQUE INDEX onek_nulltest ON onek_with_null (unique2,unique1);
-- @@@ 329

SET enable_seqscan = OFF;
-- @@@ 330
SET enable_indexscan = ON;
-- @@@ 331
SET enable_bitmapscan = ON;
-- @@@ 332

SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL;
-- @@@ 333
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NULL;
-- @@@ 334
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL;
-- @@@ 335
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NOT NULL;
-- @@@ 336
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL AND unique1 > 500;
-- @@@ 337
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique1 > 500;
-- @@@ 338
SELECT unique1, unique2 FROM onek_with_null WHERE unique1 = 500 ORDER BY unique2 DESC, unique1 DESC LIMIT 1;
-- @@@ 339

DROP INDEX onek_nulltest;
-- @@@ 340

CREATE UNIQUE INDEX onek_nulltest ON onek_with_null (unique2 desc,unique1);
-- @@@ 341

SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL;
-- @@@ 342
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NULL;
-- @@@ 343
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL;
-- @@@ 344
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NOT NULL;
-- @@@ 345
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL AND unique1 > 500;
-- @@@ 346
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique1 > 500;
-- @@@ 347
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IN (-1, 0, 1);
-- @@@ 348
SELECT unique1, unique2 FROM onek_with_null WHERE unique1 = 500 ORDER BY unique2 DESC, unique1 DESC LIMIT 1;
-- @@@ 349

DROP INDEX onek_nulltest;
-- @@@ 350

CREATE UNIQUE INDEX onek_nulltest ON onek_with_null (unique2 desc nulls last,unique1);
-- @@@ 351

SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL;
-- @@@ 352
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NULL;
-- @@@ 353
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL;
-- @@@ 354
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NOT NULL;
-- @@@ 355
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL AND unique1 > 500;
-- @@@ 356
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique1 > 500;
-- @@@ 357
SELECT unique1, unique2 FROM onek_with_null WHERE unique1 = 500 ORDER BY unique2 DESC, unique1 DESC LIMIT 1;
-- @@@ 358

DROP INDEX onek_nulltest;
-- @@@ 359

CREATE UNIQUE INDEX onek_nulltest ON onek_with_null (unique2  nulls first,unique1);
-- @@@ 360

SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL;
-- @@@ 361
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NULL;
-- @@@ 362
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL;
-- @@@ 363
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique2 IS NOT NULL;
-- @@@ 364
SELECT count(*) FROM onek_with_null WHERE unique1 IS NOT NULL AND unique1 > 500;
-- @@@ 365
SELECT count(*) FROM onek_with_null WHERE unique1 IS NULL AND unique1 > 500;
-- @@@ 366
SELECT unique1, unique2 FROM onek_with_null WHERE unique1 = 500 ORDER BY unique2 DESC, unique1 DESC LIMIT 1;
-- @@@ 367

DROP INDEX onek_nulltest;
-- @@@ 368

-- Check initial-positioning logic too

CREATE UNIQUE INDEX onek_nulltest ON onek_with_null (unique2);
-- @@@ 369

SET enable_seqscan = OFF;
-- @@@ 370
SET enable_indexscan = ON;
-- @@@ 371
SET enable_bitmapscan = OFF;
-- @@@ 372

SELECT unique1, unique2 FROM onek_with_null
  ORDER BY unique2 LIMIT 2;
-- @@@ 373
SELECT unique1, unique2 FROM onek_with_null WHERE unique2 >= -1
  ORDER BY unique2 LIMIT 2;
-- @@@ 374
SELECT unique1, unique2 FROM onek_with_null WHERE unique2 >= 0
  ORDER BY unique2 LIMIT 2;
-- @@@ 375

SELECT unique1, unique2 FROM onek_with_null
  ORDER BY unique2 DESC LIMIT 5;
-- @@@ 376
SELECT unique1, unique2 FROM onek_with_null WHERE unique2 >= -1
  ORDER BY unique2 DESC LIMIT 3;
-- @@@ 377
SELECT unique1, unique2 FROM onek_with_null WHERE unique2 < 999
  ORDER BY unique2 DESC LIMIT 2;
-- @@@ 378

RESET enable_seqscan;
-- @@@ 379
RESET enable_indexscan;
-- @@@ 380
RESET enable_bitmapscan;
-- @@@ 381

DROP TABLE onek_with_null;
-- @@@ 382

--
-- Check bitmap index path planning
--

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3 OR tenthous = 42 OR tenthous = 0);
-- @@@ 383
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3 OR tenthous = 42 OR tenthous = 0);
-- @@@ 384

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = (SELECT 1 + 2) OR tenthous = 42);
-- @@@ 385
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = (SELECT 1 + 2) OR tenthous = 42);
-- @@@ 386

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3 OR tenthous = 42 OR tenthous IS NULL);
-- @@@ 387

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1::int2 OR tenthous::int2 = 3::int8 OR tenthous = 42::int8);
-- @@@ 388

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1::int2 OR tenthous::int2 = 3::int8 OR tenthous::int2 = 42::int8);
-- @@@ 389


EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1::int2 OR tenthous = 3::int8 OR tenthous = 42::int8);
-- @@@ 390

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99);
-- @@@ 391
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99);
-- @@@ 392

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3 OR tenthous = 42);
-- @@@ 393
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3 OR tenthous = 42);
-- @@@ 394

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1::numeric OR tenthous = 3::int4 OR tenthous = 42::numeric);
-- @@@ 395

EXPLAIN (COSTS OFF)
SELECT * FROM tenk1
  WHERE tenthous = 1::numeric OR tenthous = 3::int4 OR tenthous = 42::numeric;
-- @@@ 396

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1 t1
  WHERE t1.thousand = 42 OR t1.thousand = (SELECT t2.tenthous FROM tenk1 t2 WHERE t2.thousand = t1.tenthous + 1 LIMIT 1);
-- @@@ 397
SELECT count(*) FROM tenk1 t1
  WHERE t1.thousand = 42 OR t1.thousand = (SELECT t2.tenthous FROM tenk1 t2 WHERE t2.thousand = t1.tenthous + 1 LIMIT 1);
-- @@@ 398

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99);
-- @@@ 399
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99);
-- @@@ 400

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand < 42 OR thousand < 99 OR 43 > thousand OR 42 > thousand);
-- @@@ 401
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand < 42 OR thousand < 99 OR 43 > thousand OR 42 > thousand);
-- @@@ 402

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3) OR thousand = 41;
-- @@@ 403
SELECT count(*) FROM tenk1
  WHERE thousand = 42 AND (tenthous = 1 OR tenthous = 3) OR thousand = 41;
-- @@@ 404

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99 OR tenthous < 2) OR thousand = 41;
-- @@@ 405
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 99 OR tenthous < 2) OR thousand = 41;
-- @@@ 406

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 41 OR thousand = 99 AND tenthous = 2);
-- @@@ 407
SELECT count(*) FROM tenk1
  WHERE hundred = 42 AND (thousand = 42 OR thousand = 41 OR thousand = 99 AND tenthous = 2);
-- @@@ 408

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1, tenk2
  WHERE tenk1.hundred = 42 AND (tenk2.thousand = 42 OR tenk1.thousand = 41 OR tenk2.tenthous = 2) AND
  tenk2.hundred = tenk1.hundred;
-- @@@ 409

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1, tenk2
  WHERE tenk1.hundred = 42 AND (tenk2.thousand = 42 OR tenk2.thousand = 41 OR tenk2.tenthous = 2) AND
  tenk2.hundred = tenk1.hundred;
-- @@@ 410

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1 JOIN tenk2 ON
  tenk1.hundred = 42 AND (tenk2.thousand = 42 OR tenk2.thousand = 41 OR tenk2.tenthous = 2) AND
  tenk2.hundred = tenk1.hundred;
-- @@@ 411

EXPLAIN (COSTS OFF)
SELECT count(*) FROM tenk1 LEFT JOIN tenk2 ON
  tenk1.hundred = 42 AND (tenk2.thousand = 42 OR tenk2.thousand = 41 OR tenk2.tenthous = 2) AND
  tenk2.hundred = tenk1.hundred;
-- @@@ 412
--
-- Check behavior with duplicate index column contents
--

CREATE TABLE dupindexcols AS
  SELECT unique1 as id, stringu2::text as f1 FROM tenk1;
-- @@@ 413
CREATE INDEX dupindexcols_i ON dupindexcols (f1, id, f1 text_pattern_ops);
-- @@@ 414
ANALYZE dupindexcols;
-- @@@ 415

EXPLAIN (COSTS OFF)
  SELECT count(*) FROM dupindexcols
    WHERE f1 BETWEEN 'WA' AND 'ZZZ' and id < 1000 and f1 ~<~ 'YX';
-- @@@ 416
SELECT count(*) FROM dupindexcols
  WHERE f1 BETWEEN 'WA' AND 'ZZZ' and id < 1000 and f1 ~<~ 'YX';
-- @@@ 417

--
-- Check that index scans with SAOP array and/or skip array indexquals
-- return rows in index order
--

explain (costs off)
SELECT unique1 FROM tenk1
WHERE unique1 IN (1,42,7)
ORDER BY unique1;
-- @@@ 418

SELECT unique1 FROM tenk1
WHERE unique1 IN (1,42,7)
ORDER BY unique1;
-- @@@ 419

-- Skip array on "thousand", SAOP array on "tenthous":
explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand < 2 AND tenthous IN (1001,3000)
ORDER BY thousand;
-- @@@ 420

SELECT thousand, tenthous FROM tenk1
WHERE thousand < 2 AND tenthous IN (1001,3000)
ORDER BY thousand;
-- @@@ 421

-- Skip array on "thousand", SAOP array on "tenthous", backward scan:
explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand < 2 AND tenthous IN (1001,3000)
ORDER BY thousand DESC, tenthous DESC;
-- @@@ 422

SELECT thousand, tenthous FROM tenk1
WHERE thousand < 2 AND tenthous IN (1001,3000)
ORDER BY thousand DESC, tenthous DESC;
-- @@@ 423

explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand > 995 and tenthous in (998, 999)
ORDER BY thousand desc;
-- @@@ 424

SELECT thousand, tenthous FROM tenk1
WHERE thousand > 995 and tenthous in (998, 999)
ORDER BY thousand desc;
-- @@@ 425

--
-- Check elimination of redundant and contradictory index quals
--
explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = ANY('{7, 8, 9}');
-- @@@ 426

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = ANY('{7, 8, 9}');
-- @@@ 427

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 = ANY('{7, 14, 22}') and unique1 = ANY('{33, 44}'::bigint[]);
-- @@@ 428

SELECT unique1 FROM tenk1 WHERE unique1 = ANY('{7, 14, 22}') and unique1 = ANY('{33, 44}'::bigint[]);
-- @@@ 429

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 = ANY(NULL);
-- @@@ 430

SELECT unique1 FROM tenk1 WHERE unique1 = ANY(NULL);
-- @@@ 431

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 = ANY('{NULL,NULL,NULL}');
-- @@@ 432

SELECT unique1 FROM tenk1 WHERE unique1 = ANY('{NULL,NULL,NULL}');
-- @@@ 433

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IS NULL AND unique1 IS NULL;
-- @@@ 434

SELECT unique1 FROM tenk1 WHERE unique1 IS NULL AND unique1 IS NULL;
-- @@@ 435

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = 1;
-- @@@ 436

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = 1;
-- @@@ 437

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = 12345;
-- @@@ 438

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 = 12345;
-- @@@ 439

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 >= 42;
-- @@@ 440

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 >= 42;
-- @@@ 441

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 > 42;
-- @@@ 442

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 > 42;
-- @@@ 443

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 > 9996 and unique1 >= 9999;
-- @@@ 444

SELECT unique1 FROM tenk1 WHERE unique1 > 9996 and unique1 >= 9999;
-- @@@ 445

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 < 3 and unique1 <= 3;
-- @@@ 446

SELECT unique1 FROM tenk1 WHERE unique1 < 3 and unique1 <= 3;
-- @@@ 447

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 < 3 and unique1 < (-1)::bigint;
-- @@@ 448

SELECT unique1 FROM tenk1 WHERE unique1 < 3 and unique1 < (-1)::bigint;
-- @@@ 449

explain (costs off)
SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 < (-1)::bigint;
-- @@@ 450

SELECT unique1 FROM tenk1 WHERE unique1 IN (1, 42, 7) and unique1 < (-1)::bigint;
-- @@@ 451

explain (costs off)
SELECT unique1 FROM tenk1 WHERE (thousand, tenthous) > (NULL, 5);
-- @@@ 452

SELECT unique1 FROM tenk1 WHERE (thousand, tenthous) > (NULL, 5);
-- @@@ 453

-- Skip array redundancy (pair of redundant low_compare inequalities)
explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand > -1 and thousand >= 0 AND tenthous = 3000
ORDER BY thousand;
-- @@@ 454

SELECT thousand, tenthous FROM tenk1
WHERE thousand > -1 and thousand >= 0 AND tenthous = 3000
ORDER BY thousand;
-- @@@ 455

-- Skip array redundancy (pair of redundant high_compare inequalities)
explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand < 3 and thousand <= 2 AND tenthous = 1001
ORDER BY thousand;
-- @@@ 456

SELECT thousand, tenthous FROM tenk1
WHERE thousand < 3 and thousand <= 2 AND tenthous = 1001
ORDER BY thousand;
-- @@@ 457

-- Skip array preprocessing increments "thousand > -1" to  "thousand >= 0"
explain (costs off)
SELECT thousand, tenthous FROM tenk1
WHERE thousand > -1 AND tenthous IN (1001,3000)
ORDER BY thousand limit 2;
-- @@@ 458

SELECT thousand, tenthous FROM tenk1
WHERE thousand > -1 AND tenthous IN (1001,3000)
ORDER BY thousand limit 2;
-- @@@ 459

--
-- Check elimination of constant-NULL subexpressions
--

explain (costs off)
  select * from tenk1 where (thousand, tenthous) in ((1,1001), (null,null));
-- @@@ 460

--
-- Check matching of boolean index columns to WHERE conditions and sort keys
--

create temp table boolindex (b bool, i int, unique(b, i), junk float);
-- @@@ 461

explain (costs off)
  select * from boolindex order by b, i limit 10;
-- @@@ 462
explain (costs off)
  select * from boolindex where b order by i limit 10;
-- @@@ 463
explain (costs off)
  select * from boolindex where b = true order by i desc limit 10;
-- @@@ 464
explain (costs off)
  select * from boolindex where not b order by i limit 10;
-- @@@ 465
explain (costs off)
  select * from boolindex where b is true order by i desc limit 10;
-- @@@ 466
explain (costs off)
  select * from boolindex where b is false order by i desc limit 10;
-- @@@ 467

--
-- REINDEX (VERBOSE)
--
CREATE TABLE reindex_verbose(id integer primary key);
-- @@@ 468
\set VERBOSITY terse \\ -- suppress machine-dependent details
-- @@@ 469
REINDEX (VERBOSE) TABLE reindex_verbose;
-- @@@ 470
\set VERBOSITY default
-- @@@ 471
DROP TABLE reindex_verbose;
-- @@@ 472

--
-- REINDEX CONCURRENTLY
--
CREATE TABLE concur_reindex_tab (c1 int);
-- @@@ 473
-- REINDEX
REINDEX TABLE concur_reindex_tab; -- notice
-- @@@ 474
REINDEX (CONCURRENTLY) TABLE concur_reindex_tab; -- notice
-- @@@ 475
ALTER TABLE concur_reindex_tab ADD COLUMN c2 text; -- add toast index
-- @@@ 476
-- Normal index with integer column
CREATE UNIQUE INDEX concur_reindex_ind1 ON concur_reindex_tab(c1);
-- @@@ 477
-- Normal index with text column
CREATE INDEX concur_reindex_ind2 ON concur_reindex_tab(c2);
-- @@@ 478
-- UNIQUE index with expression
CREATE UNIQUE INDEX concur_reindex_ind3 ON concur_reindex_tab(abs(c1));
-- @@@ 479
-- Duplicate column names
CREATE INDEX concur_reindex_ind4 ON concur_reindex_tab(c1, c1, c2);
-- @@@ 480
-- Create table for check on foreign key dependence switch with indexes swapped
ALTER TABLE concur_reindex_tab ADD PRIMARY KEY USING INDEX concur_reindex_ind1;
-- @@@ 481
CREATE TABLE concur_reindex_tab2 (c1 int REFERENCES concur_reindex_tab);
-- @@@ 482
INSERT INTO concur_reindex_tab VALUES  (1, 'a');
-- @@@ 483
INSERT INTO concur_reindex_tab VALUES  (2, 'a');
-- @@@ 484
-- Reindex concurrently of exclusion constraint currently not supported
CREATE TABLE concur_reindex_tab3 (c1 int, c2 int4range, EXCLUDE USING gist (c2 WITH &&));
-- @@@ 485
INSERT INTO concur_reindex_tab3 VALUES  (3, '[1,2]');
-- @@@ 486
REINDEX INDEX CONCURRENTLY  concur_reindex_tab3_c2_excl;  -- error
-- @@@ 487
REINDEX TABLE CONCURRENTLY concur_reindex_tab3;  -- succeeds with warning
-- @@@ 488
INSERT INTO concur_reindex_tab3 VALUES  (4, '[2,4]');
-- @@@ 489
-- Check materialized views
CREATE MATERIALIZED VIEW concur_reindex_matview AS SELECT * FROM concur_reindex_tab;
-- @@@ 490
-- Dependency lookup before and after the follow-up REINDEX commands.
-- These should remain consistent.
SELECT pg_describe_object(classid, objid, objsubid) as obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) as objref,
       deptype
FROM pg_depend
WHERE classid = 'pg_class'::regclass AND
  objid in ('concur_reindex_tab'::regclass,
            'concur_reindex_ind1'::regclass,
	    'concur_reindex_ind2'::regclass,
	    'concur_reindex_ind3'::regclass,
	    'concur_reindex_ind4'::regclass,
	    'concur_reindex_matview'::regclass)
  ORDER BY 1, 2;
-- @@@ 491
REINDEX INDEX CONCURRENTLY concur_reindex_ind1;
-- @@@ 492
REINDEX TABLE CONCURRENTLY concur_reindex_tab;
-- @@@ 493
REINDEX TABLE CONCURRENTLY concur_reindex_matview;
-- @@@ 494
SELECT pg_describe_object(classid, objid, objsubid) as obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) as objref,
       deptype
FROM pg_depend
WHERE classid = 'pg_class'::regclass AND
  objid in ('concur_reindex_tab'::regclass,
            'concur_reindex_ind1'::regclass,
	    'concur_reindex_ind2'::regclass,
	    'concur_reindex_ind3'::regclass,
	    'concur_reindex_ind4'::regclass,
	    'concur_reindex_matview'::regclass)
  ORDER BY 1, 2;
-- @@@ 495
-- Check that comments are preserved
CREATE TABLE testcomment (i int);
-- @@@ 496
CREATE INDEX testcomment_idx1 ON testcomment (i);
-- @@@ 497
COMMENT ON INDEX testcomment_idx1 IS 'test comment';
-- @@@ 498
SELECT obj_description('testcomment_idx1'::regclass, 'pg_class');
-- @@@ 499
REINDEX TABLE testcomment;
-- @@@ 500
SELECT obj_description('testcomment_idx1'::regclass, 'pg_class');
-- @@@ 501
REINDEX TABLE CONCURRENTLY testcomment ;
-- @@@ 502
SELECT obj_description('testcomment_idx1'::regclass, 'pg_class');
-- @@@ 503
DROP TABLE testcomment;
-- @@@ 504
-- Check that indisclustered updates are preserved
CREATE TABLE concur_clustered(i int);
-- @@@ 505
CREATE INDEX concur_clustered_i_idx ON concur_clustered(i);
-- @@@ 506
ALTER TABLE concur_clustered CLUSTER ON concur_clustered_i_idx;
-- @@@ 507
REINDEX TABLE CONCURRENTLY concur_clustered;
-- @@@ 508
SELECT indexrelid::regclass, indisclustered FROM pg_index
  WHERE indrelid = 'concur_clustered'::regclass;
-- @@@ 509
DROP TABLE concur_clustered;
-- @@@ 510
-- Check that indisreplident updates are preserved.
CREATE TABLE concur_replident(i int NOT NULL);
-- @@@ 511
CREATE UNIQUE INDEX concur_replident_i_idx ON concur_replident(i);
-- @@@ 512
ALTER TABLE concur_replident REPLICA IDENTITY
  USING INDEX concur_replident_i_idx;
-- @@@ 513
SELECT indexrelid::regclass, indisreplident FROM pg_index
  WHERE indrelid = 'concur_replident'::regclass;
-- @@@ 514
REINDEX TABLE CONCURRENTLY concur_replident;
-- @@@ 515
SELECT indexrelid::regclass, indisreplident FROM pg_index
  WHERE indrelid = 'concur_replident'::regclass;
-- @@@ 516
DROP TABLE concur_replident;
-- @@@ 517
-- Check that opclass parameters are preserved
CREATE TABLE concur_appclass_tab(i tsvector, j tsvector, k tsvector);
-- @@@ 518
CREATE INDEX concur_appclass_ind on concur_appclass_tab
  USING gist (i tsvector_ops (siglen='1000'), j tsvector_ops (siglen='500'));
-- @@@ 519
CREATE INDEX concur_appclass_ind_2 on concur_appclass_tab
  USING gist (k tsvector_ops (siglen='300'), j tsvector_ops);
-- @@@ 520
REINDEX TABLE CONCURRENTLY concur_appclass_tab;
-- @@@ 521
\d concur_appclass_tab
-- @@@ 522
DROP TABLE concur_appclass_tab;
-- @@@ 523

-- Partitions
-- Create some partitioned tables
CREATE TABLE concur_reindex_part (c1 int, c2 int) PARTITION BY RANGE (c1);
-- @@@ 524
CREATE TABLE concur_reindex_part_0 PARTITION OF concur_reindex_part
  FOR VALUES FROM (0) TO (10) PARTITION BY list (c2);
-- @@@ 525
CREATE TABLE concur_reindex_part_0_1 PARTITION OF concur_reindex_part_0
  FOR VALUES IN (1);
-- @@@ 526
CREATE TABLE concur_reindex_part_0_2 PARTITION OF concur_reindex_part_0
  FOR VALUES IN (2);
-- @@@ 527
-- This partitioned table will have no partitions.
CREATE TABLE concur_reindex_part_10 PARTITION OF concur_reindex_part
  FOR VALUES FROM (10) TO (20) PARTITION BY list (c2);
-- @@@ 528
-- Create some partitioned indexes
CREATE INDEX concur_reindex_part_index ON ONLY concur_reindex_part (c1);
-- @@@ 529
CREATE INDEX concur_reindex_part_index_0 ON ONLY concur_reindex_part_0 (c1);
-- @@@ 530
ALTER INDEX concur_reindex_part_index ATTACH PARTITION concur_reindex_part_index_0;
-- @@@ 531
-- This partitioned index will have no partitions.
CREATE INDEX concur_reindex_part_index_10 ON ONLY concur_reindex_part_10 (c1);
-- @@@ 532
ALTER INDEX concur_reindex_part_index ATTACH PARTITION concur_reindex_part_index_10;
-- @@@ 533
CREATE INDEX concur_reindex_part_index_0_1 ON ONLY concur_reindex_part_0_1 (c1);
-- @@@ 534
ALTER INDEX concur_reindex_part_index_0 ATTACH PARTITION concur_reindex_part_index_0_1;
-- @@@ 535
CREATE INDEX concur_reindex_part_index_0_2 ON ONLY concur_reindex_part_0_2 (c1);
-- @@@ 536
ALTER INDEX concur_reindex_part_index_0 ATTACH PARTITION concur_reindex_part_index_0_2;
-- @@@ 537
SELECT relid, parentrelid, level FROM pg_partition_tree('concur_reindex_part_index')
  ORDER BY relid, level;
-- @@@ 538
SELECT relid, parentrelid, level FROM pg_partition_tree('concur_reindex_part_index')
  ORDER BY relid, level;
-- @@@ 539
-- REINDEX should preserve dependencies of partition tree.
SELECT pg_describe_object(classid, objid, objsubid) as obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) as objref,
       deptype
FROM pg_depend
WHERE classid = 'pg_class'::regclass AND
  objid in ('concur_reindex_part'::regclass,
            'concur_reindex_part_0'::regclass,
            'concur_reindex_part_0_1'::regclass,
            'concur_reindex_part_0_2'::regclass,
            'concur_reindex_part_index'::regclass,
            'concur_reindex_part_index_0'::regclass,
            'concur_reindex_part_index_0_1'::regclass,
            'concur_reindex_part_index_0_2'::regclass)
  ORDER BY 1, 2;
-- @@@ 540
REINDEX INDEX CONCURRENTLY concur_reindex_part_index_0_1;
-- @@@ 541
REINDEX INDEX CONCURRENTLY concur_reindex_part_index_0_2;
-- @@@ 542
SELECT relid, parentrelid, level FROM pg_partition_tree('concur_reindex_part_index')
  ORDER BY relid, level;
-- @@@ 543
REINDEX TABLE CONCURRENTLY concur_reindex_part_0_1;
-- @@@ 544
REINDEX TABLE CONCURRENTLY concur_reindex_part_0_2;
-- @@@ 545
SELECT pg_describe_object(classid, objid, objsubid) as obj,
       pg_describe_object(refclassid,refobjid,refobjsubid) as objref,
       deptype
FROM pg_depend
WHERE classid = 'pg_class'::regclass AND
  objid in ('concur_reindex_part'::regclass,
            'concur_reindex_part_0'::regclass,
            'concur_reindex_part_0_1'::regclass,
            'concur_reindex_part_0_2'::regclass,
            'concur_reindex_part_index'::regclass,
            'concur_reindex_part_index_0'::regclass,
            'concur_reindex_part_index_0_1'::regclass,
            'concur_reindex_part_index_0_2'::regclass)
  ORDER BY 1, 2;
-- @@@ 546
SELECT relid, parentrelid, level FROM pg_partition_tree('concur_reindex_part_index')
  ORDER BY relid, level;
-- @@@ 547

-- REINDEX for partitioned indexes
-- REINDEX TABLE fails for partitioned indexes
-- Top-most parent index
REINDEX TABLE concur_reindex_part_index; -- error
-- @@@ 548
REINDEX TABLE CONCURRENTLY concur_reindex_part_index; -- error
-- @@@ 549
-- Partitioned index with no leaves
REINDEX TABLE concur_reindex_part_index_10; -- error
-- @@@ 550
REINDEX TABLE CONCURRENTLY concur_reindex_part_index_10; -- error
-- @@@ 551
-- Cannot run in a transaction block
BEGIN;
-- @@@ 552
REINDEX INDEX concur_reindex_part_index;
-- @@@ 553
ROLLBACK;
-- @@@ 554
-- Helper functions to track changes of relfilenodes in a partition tree.
-- Create a table tracking the relfilenode state.
CREATE OR REPLACE FUNCTION create_relfilenode_part(relname text, indname text)
  RETURNS VOID AS
  $func$
  BEGIN
  EXECUTE format('
    CREATE TABLE %I AS
      SELECT oid, relname, relfilenode, relkind, reltoastrelid
      FROM pg_class
      WHERE oid IN
         (SELECT relid FROM pg_partition_tree(''%I''));',
	 relname, indname);
  END
  $func$ LANGUAGE plpgsql;
-- @@@ 555
CREATE OR REPLACE FUNCTION compare_relfilenode_part(tabname text)
  RETURNS TABLE (relname name, relkind "char", state text) AS
  $func$
  BEGIN
    RETURN QUERY EXECUTE
      format(
        'SELECT  b.relname,
                 b.relkind,
                 CASE WHEN a.relfilenode = b.relfilenode THEN ''relfilenode is unchanged''
                 ELSE ''relfilenode has changed'' END
           -- Do not join with OID here as CONCURRENTLY changes it.
           FROM %I b JOIN pg_class a ON b.relname = a.relname
           ORDER BY 1;', tabname);
  END
  $func$ LANGUAGE plpgsql;
-- @@@ 556
--  Check that expected relfilenodes are changed, non-concurrent case.
SELECT create_relfilenode_part('reindex_index_status', 'concur_reindex_part_index');
-- @@@ 557
REINDEX INDEX concur_reindex_part_index;
-- @@@ 558
SELECT * FROM compare_relfilenode_part('reindex_index_status');
-- @@@ 559
DROP TABLE reindex_index_status;
-- @@@ 560
-- concurrent case.
SELECT create_relfilenode_part('reindex_index_status', 'concur_reindex_part_index');
-- @@@ 561
REINDEX INDEX CONCURRENTLY concur_reindex_part_index;
-- @@@ 562
SELECT * FROM compare_relfilenode_part('reindex_index_status');
-- @@@ 563
DROP TABLE reindex_index_status;
-- @@@ 564

-- REINDEX for partitioned tables
-- REINDEX INDEX fails for partitioned tables
-- Top-most parent
REINDEX INDEX concur_reindex_part; -- error
-- @@@ 565
REINDEX INDEX CONCURRENTLY concur_reindex_part; -- error
-- @@@ 566
-- Partitioned with no leaves
REINDEX INDEX concur_reindex_part_10; -- error
-- @@@ 567
REINDEX INDEX CONCURRENTLY concur_reindex_part_10; -- error
-- @@@ 568
-- Cannot run in a transaction block
BEGIN;
-- @@@ 569
REINDEX TABLE concur_reindex_part;
-- @@@ 570
ROLLBACK;
-- @@@ 571
-- Check that expected relfilenodes are changed, non-concurrent case.
-- Note that the partition tree changes of the *indexes* need to be checked.
SELECT create_relfilenode_part('reindex_index_status', 'concur_reindex_part_index');
-- @@@ 572
REINDEX TABLE concur_reindex_part;
-- @@@ 573
SELECT * FROM compare_relfilenode_part('reindex_index_status');
-- @@@ 574
DROP TABLE reindex_index_status;
-- @@@ 575
-- concurrent case.
SELECT create_relfilenode_part('reindex_index_status', 'concur_reindex_part_index');
-- @@@ 576
REINDEX TABLE CONCURRENTLY concur_reindex_part;
-- @@@ 577
SELECT * FROM compare_relfilenode_part('reindex_index_status');
-- @@@ 578
DROP TABLE reindex_index_status;
-- @@@ 579

DROP FUNCTION create_relfilenode_part;
-- @@@ 580
DROP FUNCTION compare_relfilenode_part;
-- @@@ 581

-- Cleanup of partition tree used for REINDEX test.
DROP TABLE concur_reindex_part;
-- @@@ 582

-- Check errors
-- Cannot run inside a transaction block
BEGIN;
-- @@@ 583
REINDEX TABLE CONCURRENTLY concur_reindex_tab;
-- @@@ 584
COMMIT;
-- @@@ 585
REINDEX TABLE CONCURRENTLY pg_class; -- no catalog relation
-- @@@ 586
REINDEX INDEX CONCURRENTLY pg_class_oid_index; -- no catalog index
-- @@@ 587
-- These are the toast table and index of pg_database.
REINDEX TABLE CONCURRENTLY pg_toast.pg_toast_1262; -- no catalog toast table
-- @@@ 588
REINDEX INDEX CONCURRENTLY pg_toast.pg_toast_1262_index; -- no catalog toast index
-- @@@ 589
REINDEX SYSTEM CONCURRENTLY postgres; -- not allowed for SYSTEM
-- @@@ 590
REINDEX (CONCURRENTLY) SYSTEM postgres; -- ditto
-- @@@ 591
REINDEX (CONCURRENTLY) SYSTEM;  -- ditto
-- @@@ 592
-- Warns about catalog relations
REINDEX SCHEMA CONCURRENTLY pg_catalog;
-- @@@ 593
-- Not the current database
REINDEX DATABASE not_current_database;
-- @@@ 594

-- Check the relation status, there should not be invalid indexes
\d concur_reindex_tab
-- @@@ 595
DROP MATERIALIZED VIEW concur_reindex_matview;
-- @@@ 596
DROP TABLE concur_reindex_tab, concur_reindex_tab2, concur_reindex_tab3;
-- @@@ 597

-- Check handling of invalid indexes
CREATE TABLE concur_reindex_tab4 (c1 int);
-- @@@ 598
INSERT INTO concur_reindex_tab4 VALUES (1), (1), (2);
-- @@@ 599
-- This trick creates an invalid index.
CREATE UNIQUE INDEX CONCURRENTLY concur_reindex_ind5 ON concur_reindex_tab4 (c1);
-- @@@ 600
-- Reindexing concurrently this index fails with the same failure.
-- The extra index created is itself invalid, and can be dropped.
REINDEX INDEX CONCURRENTLY concur_reindex_ind5;
-- @@@ 601
\d concur_reindex_tab4
-- @@@ 602
DROP INDEX concur_reindex_ind5_ccnew;
-- @@@ 603
-- This makes the previous failure go away, so the index can become valid.
DELETE FROM concur_reindex_tab4 WHERE c1 = 1;
-- @@@ 604
-- The invalid index is not processed when running REINDEX TABLE.
REINDEX TABLE CONCURRENTLY concur_reindex_tab4;
-- @@@ 605
\d concur_reindex_tab4
-- @@@ 606
-- But it is fixed with REINDEX INDEX.
REINDEX INDEX CONCURRENTLY concur_reindex_ind5;
-- @@@ 607
\d concur_reindex_tab4
-- @@@ 608
DROP TABLE concur_reindex_tab4;
-- @@@ 609

-- Check handling of indexes with expressions and predicates.  The
-- definitions of the rebuilt indexes should match the original
-- definitions.
CREATE TABLE concur_exprs_tab (c1 int , c2 boolean);
-- @@@ 610
INSERT INTO concur_exprs_tab (c1, c2) VALUES (1369652450, FALSE),
  (414515746, TRUE),
  (897778963, FALSE);
-- @@@ 611
CREATE UNIQUE INDEX concur_exprs_index_expr
  ON concur_exprs_tab ((c1::text COLLATE "C"));
-- @@@ 612
CREATE UNIQUE INDEX concur_exprs_index_pred ON concur_exprs_tab (c1)
  WHERE (c1::text > 500000000::text COLLATE "C");
-- @@@ 613
CREATE UNIQUE INDEX concur_exprs_index_pred_2
  ON concur_exprs_tab ((1 / c1))
  WHERE ('-H') >= (c2::TEXT) COLLATE "C";
-- @@@ 614
ALTER INDEX concur_exprs_index_expr ALTER COLUMN 1 SET STATISTICS 100;
-- @@@ 615
ANALYZE concur_exprs_tab;
-- @@@ 616
SELECT starelid::regclass, count(*) FROM pg_statistic WHERE starelid IN (
  'concur_exprs_index_expr'::regclass,
  'concur_exprs_index_pred'::regclass,
  'concur_exprs_index_pred_2'::regclass)
  GROUP BY starelid ORDER BY starelid::regclass::text;
-- @@@ 617
SELECT pg_get_indexdef('concur_exprs_index_expr'::regclass);
-- @@@ 618
SELECT pg_get_indexdef('concur_exprs_index_pred'::regclass);
-- @@@ 619
SELECT pg_get_indexdef('concur_exprs_index_pred_2'::regclass);
-- @@@ 620
REINDEX TABLE CONCURRENTLY concur_exprs_tab;
-- @@@ 621
SELECT pg_get_indexdef('concur_exprs_index_expr'::regclass);
-- @@@ 622
SELECT pg_get_indexdef('concur_exprs_index_pred'::regclass);
-- @@@ 623
SELECT pg_get_indexdef('concur_exprs_index_pred_2'::regclass);
-- @@@ 624
-- ALTER TABLE recreates the indexes, which should keep their collations.
ALTER TABLE concur_exprs_tab ALTER c2 TYPE TEXT;
-- @@@ 625
SELECT pg_get_indexdef('concur_exprs_index_expr'::regclass);
-- @@@ 626
SELECT pg_get_indexdef('concur_exprs_index_pred'::regclass);
-- @@@ 627
SELECT pg_get_indexdef('concur_exprs_index_pred_2'::regclass);
-- @@@ 628
-- Statistics should remain intact.
SELECT starelid::regclass, count(*) FROM pg_statistic WHERE starelid IN (
  'concur_exprs_index_expr'::regclass,
  'concur_exprs_index_pred'::regclass,
  'concur_exprs_index_pred_2'::regclass)
  GROUP BY starelid ORDER BY starelid::regclass::text;
-- @@@ 629
-- attstattarget should remain intact
SELECT attrelid::regclass, attnum, attstattarget
  FROM pg_attribute WHERE attrelid IN (
    'concur_exprs_index_expr'::regclass,
    'concur_exprs_index_pred'::regclass,
    'concur_exprs_index_pred_2'::regclass)
  ORDER BY attrelid::regclass::text, attnum;
-- @@@ 630
DROP TABLE concur_exprs_tab;
-- @@@ 631

-- Temporary tables and on-commit actions, where CONCURRENTLY is ignored.
-- ON COMMIT PRESERVE ROWS, the default.
CREATE TEMP TABLE concur_temp_tab_1 (c1 int, c2 text)
  ON COMMIT PRESERVE ROWS;
-- @@@ 632
INSERT INTO concur_temp_tab_1 VALUES (1, 'foo'), (2, 'bar');
-- @@@ 633
CREATE INDEX concur_temp_ind_1 ON concur_temp_tab_1(c2);
-- @@@ 634
REINDEX TABLE CONCURRENTLY concur_temp_tab_1;
-- @@@ 635
REINDEX INDEX CONCURRENTLY concur_temp_ind_1;
-- @@@ 636
-- Still fails in transaction blocks
BEGIN;
-- @@@ 637
REINDEX INDEX CONCURRENTLY concur_temp_ind_1;
-- @@@ 638
COMMIT;
-- @@@ 639
-- ON COMMIT DELETE ROWS
CREATE TEMP TABLE concur_temp_tab_2 (c1 int, c2 text)
  ON COMMIT DELETE ROWS;
-- @@@ 640
CREATE INDEX concur_temp_ind_2 ON concur_temp_tab_2(c2);
-- @@@ 641
REINDEX TABLE CONCURRENTLY concur_temp_tab_2;
-- @@@ 642
REINDEX INDEX CONCURRENTLY concur_temp_ind_2;
-- @@@ 643
-- ON COMMIT DROP
BEGIN;
-- @@@ 644
CREATE TEMP TABLE concur_temp_tab_3 (c1 int, c2 text)
  ON COMMIT PRESERVE ROWS;
-- @@@ 645
INSERT INTO concur_temp_tab_3 VALUES (1, 'foo'), (2, 'bar');
-- @@@ 646
CREATE INDEX concur_temp_ind_3 ON concur_temp_tab_3(c2);
-- @@@ 647
-- Fails when running in a transaction
REINDEX INDEX CONCURRENTLY concur_temp_ind_3;
-- @@@ 648
COMMIT;
-- @@@ 649
-- REINDEX SCHEMA processes all temporary relations
CREATE TABLE reindex_temp_before AS
SELECT oid, relname, relfilenode, relkind, reltoastrelid
  FROM pg_class
  WHERE relname IN ('concur_temp_ind_1', 'concur_temp_ind_2');
-- @@@ 650
SELECT pg_my_temp_schema()::regnamespace as temp_schema_name \gset
-- @@@ 651
REINDEX SCHEMA CONCURRENTLY :temp_schema_name;
-- @@@ 652
SELECT  b.relname,
        b.relkind,
        CASE WHEN a.relfilenode = b.relfilenode THEN 'relfilenode is unchanged'
        ELSE 'relfilenode has changed' END
  FROM reindex_temp_before b JOIN pg_class a ON b.oid = a.oid
  ORDER BY 1;
-- @@@ 653
DROP TABLE concur_temp_tab_1, concur_temp_tab_2, reindex_temp_before;
-- @@@ 654

-- No OR-clause groupings should happen, and there should be no clause
-- permutations in the filtering conditions we could see in the EXPLAIN.
EXPLAIN (COSTS OFF)
SELECT * FROM tenk1 WHERE unique1 < 1 OR hundred < 2;
-- @@@ 655

-- OR clauses in the 'unique1' column are grouped, so clause permutation
-- occurs. W e can see it in the 'Recheck Cond': the second clause involving
-- the 'unique1' column goes just after the first one.
EXPLAIN (COSTS OFF)
SELECT * FROM tenk1 WHERE unique1 < 1 OR unique1 < 3 OR hundred < 2;
-- @@@ 656

-- Check bitmap scan can consider similar OR arguments separately without
-- grouping them into SAOP.
CREATE TABLE bitmap_split_or (a int NOT NULL, b int NOT NULL, c int NOT NULL);
-- @@@ 657
INSERT INTO bitmap_split_or (SELECT 1, 1, i FROM generate_series(1, 1000) i);
-- @@@ 658
INSERT INTO bitmap_split_or (select i, 2, 2 FROM generate_series(1, 1000) i);
-- @@@ 659
VACUUM ANALYZE bitmap_split_or;
-- @@@ 660
CREATE INDEX t_b_partial_1_idx ON bitmap_split_or (b) WHERE a = 1;
-- @@@ 661
CREATE INDEX t_b_partial_2_idx ON bitmap_split_or (b) WHERE a = 2;
-- @@@ 662
EXPLAIN (COSTS OFF)
SELECT * FROM bitmap_split_or WHERE (a = 1 OR a = 2) AND b = 2;
-- @@@ 663
DROP INDEX t_b_partial_1_idx;
-- @@@ 664
DROP INDEX t_b_partial_2_idx;
-- @@@ 665
CREATE INDEX t_a_b_idx ON bitmap_split_or (a, b);
-- @@@ 666
CREATE INDEX t_b_c_idx ON bitmap_split_or (b, c);
-- @@@ 667
CREATE STATISTICS t_a_b_stat (mcv) ON a, b FROM bitmap_split_or;
-- @@@ 668
CREATE STATISTICS t_b_c_stat (mcv) ON b, c FROM bitmap_split_or;
-- @@@ 669
ANALYZE bitmap_split_or;
-- @@@ 670
EXPLAIN (COSTS OFF)
SELECT * FROM bitmap_split_or t1, bitmap_split_or t2
WHERE t1.a = t2.b OR t1.a = 2;
-- @@@ 671
EXPLAIN (COSTS OFF)
SELECT * FROM bitmap_split_or WHERE a = 1 AND (b = 1 OR b = 2) AND c = 2;
-- @@@ 672
DROP TABLE bitmap_split_or;
-- @@@ 673

--
-- REINDEX SCHEMA
--
REINDEX SCHEMA schema_to_reindex; -- failure, schema does not exist
-- @@@ 674
CREATE SCHEMA schema_to_reindex;
-- @@@ 675
SET search_path = 'schema_to_reindex';
-- @@@ 676
CREATE TABLE table1(col1 SERIAL PRIMARY KEY);
-- @@@ 677
INSERT INTO table1 SELECT generate_series(1,400);
-- @@@ 678
CREATE TABLE table2(col1 SERIAL PRIMARY KEY, col2 TEXT NOT NULL);
-- @@@ 679
INSERT INTO table2 SELECT generate_series(1,400), 'abc';
-- @@@ 680
CREATE INDEX ON table2(col2);
-- @@@ 681
CREATE MATERIALIZED VIEW matview AS SELECT col1 FROM table2;
-- @@@ 682
CREATE INDEX ON matview(col1);
-- @@@ 683
CREATE VIEW view AS SELECT col2 FROM table2;
-- @@@ 684
CREATE TABLE reindex_before AS
SELECT oid, relname, relfilenode, relkind, reltoastrelid
	FROM pg_class
	where relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'schema_to_reindex');
-- @@@ 685
INSERT INTO reindex_before
SELECT oid, 'pg_toast_TABLE', relfilenode, relkind, reltoastrelid
FROM pg_class WHERE oid IN
	(SELECT reltoastrelid FROM reindex_before WHERE reltoastrelid > 0);
-- @@@ 686
INSERT INTO reindex_before
SELECT oid, 'pg_toast_TABLE_index', relfilenode, relkind, reltoastrelid
FROM pg_class where oid in
	(select indexrelid from pg_index where indrelid in
		(select reltoastrelid from reindex_before where reltoastrelid > 0));
-- @@@ 687
REINDEX SCHEMA schema_to_reindex;
-- @@@ 688
CREATE TABLE reindex_after AS SELECT oid, relname, relfilenode, relkind
	FROM pg_class
	where relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'schema_to_reindex');
-- @@@ 689
SELECT  b.relname,
        b.relkind,
        CASE WHEN a.relfilenode = b.relfilenode THEN 'relfilenode is unchanged'
        ELSE 'relfilenode has changed' END
  FROM reindex_before b JOIN pg_class a ON b.oid = a.oid
  ORDER BY 1;
-- @@@ 690
REINDEX SCHEMA schema_to_reindex;
-- @@@ 691
BEGIN;
-- @@@ 692
REINDEX SCHEMA schema_to_reindex; -- failure, cannot run in a transaction
-- @@@ 693
END;
-- @@@ 694

-- concurrently
REINDEX SCHEMA CONCURRENTLY schema_to_reindex;
-- @@@ 695

-- Failure for unauthorized user
CREATE ROLE regress_reindexuser NOLOGIN;
-- @@@ 696
SET SESSION ROLE regress_reindexuser;
-- @@@ 697
REINDEX SCHEMA schema_to_reindex;
-- @@@ 698
-- Permission failures with toast tables and indexes (pg_authid here)
RESET ROLE;
-- @@@ 699
GRANT USAGE ON SCHEMA pg_toast TO regress_reindexuser;
-- @@@ 700
SET SESSION ROLE regress_reindexuser;
-- @@@ 701
REINDEX TABLE pg_toast.pg_toast_1262;
-- @@@ 702
REINDEX INDEX pg_toast.pg_toast_1262_index;
-- @@@ 703

-- Clean up
RESET ROLE;
-- @@@ 704
REVOKE USAGE ON SCHEMA pg_toast FROM regress_reindexuser;
-- @@@ 705
DROP ROLE regress_reindexuser;
-- @@@ 706
DROP SCHEMA schema_to_reindex CASCADE;
-- @@@ 707
