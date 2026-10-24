<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Statement results

Result metadata (column names and types as the driver reports them), rows, paging, LWT results, schema change results and prepared-statement metadata for each statement family. Results use the Python driver over protocol v4 on both dialects (schema-change and USE results come from the raw client). Statements run in order against keyspace `s`; writes use explicit timestamps where the value matters. When both dialects return the same thing it is shown once. Column types are as the Python driver renders the result metadata, which does not show frozen-ness reliably (a frozen list prints as `list<int>`, a non-frozen UDT as `frozen<addr>`); system-schema.md has the authoritative column types. Rows are Python reprs.

Data: `s.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, tu tuple<int, text>, ad addr, PRIMARY KEY (pk, ck))` with rows (1,1,10,s=100,'one',…), (1,2,20,'two'), (1,3,30,'three') at timestamp 1000, (2,1,5) at 2000 and a static-only partition 3; `s.t2 (a, b, c, d, v, PRIMARY KEY ((a, b), c, d)) WITH CLUSTERING ORDER BY (c DESC, d ASC)`; `s.ty` with one column per native type; `s.c` counters.

## Schema-change, USE and TRUNCATE results

### ddl_create_type

```cql
CREATE TYPE s.addr (street text, zip int)
```
Both:

```
OK SchemaChange `["CREATED", "TYPE", "s", "addr"]`
```
### ddl_create_table

```cql
CREATE TABLE s.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<text>, m map<text, int>, fl frozen<list<int>>, tu tuple<int, text>, ad addr, PRIMARY KEY (pk, ck))
```
Both:

```
OK SchemaChange `["CREATED", "TABLE", "s", "t"]`
```
### ddl_create_table_if_not_exists_existing

```cql
CREATE TABLE IF NOT EXISTS s.t (pk int PRIMARY KEY)
```
ScyllaDB 2026.3.3:

```
OK SchemaChange `["CREATED", "TABLE", "s", "t"]`
```
Cassandra 5.0.9:

```
OK Void
```
### ddl_create_counter

```cql
CREATE TABLE s.c (pk int, ck int, n counter, PRIMARY KEY (pk, ck))
```
Both:

```
OK SchemaChange `["CREATED", "TABLE", "s", "c"]`
```
### ddl_create_types

```cql
CREATE TABLE s.ty (pk int PRIMARY KEY, a ascii, bi bigint, b blob, bo boolean, d date, de decimal, db double, du duration, f float, i inet, n int, sm smallint, tx text, tm time, ts timestamp, tid timeuuid, ti tinyint, u uuid, vc varchar, vi varint, vec vector<float, 3>)
```
Both:

```
OK SchemaChange `["CREATED", "TABLE", "s", "ty"]`
```
### ddl_create_t2

```cql
CREATE TABLE s.t2 (a int, b int, c int, d int, v int, PRIMARY KEY ((a, b), c, d)) WITH CLUSTERING ORDER BY (c DESC, d ASC)
```
Both:

```
OK SchemaChange `["CREATED", "TABLE", "s", "t2"]`
```
### ddl_create_index

```cql
CREATE INDEX t_v_idx ON s.t (v)
```
ScyllaDB 2026.3.3:

```
OK SchemaChange `["UPDATED", "TABLE", "s", "t"]`; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack."
warnings: ["Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack."]
```
Cassandra 5.0.9:

```
OK SchemaChange `["UPDATED", "TABLE", "s", "t"]`
```
### ddl_alter_table_add

```cql
ALTER TABLE s.t ADD extra text
```
Both:

```
OK SchemaChange `["UPDATED", "TABLE", "s", "t"]`
```
### ddl_alter_table_with

```cql
ALTER TABLE s.t WITH comment = 'x'
```
Both:

```
OK SchemaChange `["UPDATED", "TABLE", "s", "t"]`
```
### ddl_alter_type_add

```cql
ALTER TYPE s.addr ADD city text
```
Both:

```
OK SchemaChange `["UPDATED", "TYPE", "s", "addr"]`
```
### ddl_alter_type_rename

```cql
ALTER TYPE s.addr RENAME city TO town
```
Both:

```
OK SchemaChange `["UPDATED", "TYPE", "s", "addr"]`
```
### ddl_alter_keyspace

```cql
ALTER KEYSPACE s WITH durable_writes = true
```
ScyllaDB 2026.3.3:

```
OK SchemaChange `["UPDATED", "KEYSPACE", "s"]`
```
Cassandra 5.0.9:

```
OK Void
```
### ddl_create_drop_table

```cql
CREATE TABLE s.tmp (pk int PRIMARY KEY)
```
Both:

```
OK SchemaChange `["CREATED", "TABLE", "s", "tmp"]`
```
### ddl_drop_table

```cql
DROP TABLE s.tmp
```
Both:

```
OK SchemaChange `["DROPPED", "TABLE", "s", "tmp"]`
```
### ddl_drop_table_if_exists_missing

```cql
DROP TABLE IF EXISTS s.tmp
```
Both:

```
OK Void
```
### ddl_drop_index_if_exists_missing

```cql
DROP INDEX IF EXISTS s.nope
```
Both:

```
OK Void
```
### ddl_create_type2

```cql
CREATE TYPE s.tmp_t (x int)
```
Both:

```
OK SchemaChange `["CREATED", "TYPE", "s", "tmp_t"]`
```
### ddl_drop_type

```cql
DROP TYPE s.tmp_t
```
Both:

```
OK SchemaChange `["DROPPED", "TYPE", "s", "tmp_t"]`
```
### ddl_create_ks2

```cql
CREATE KEYSPACE s2 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}
```
ScyllaDB 2026.3.3:

```
OK SchemaChange `["CREATED", "KEYSPACE", "s2"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended."
warnings: ['Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended.']
```
Cassandra 5.0.9:

```
OK SchemaChange `["CREATED", "KEYSPACE", "s2"]`
```
### ddl_drop_ks2

```cql
DROP KEYSPACE s2
```
Both:

```
OK SchemaChange `["DROPPED", "KEYSPACE", "s2"]`
```
### ddl_drop_ks_if_exists_missing

```cql
DROP KEYSPACE IF EXISTS s2
```
Both:

```
OK Void
```
### use

```cql
USE s
```
Both:

```
OK SetKeyspace
```
### use_quoted

```cql
USE "s"
```
Both:

```
OK SetKeyspace
```
### truncate

```cql
TRUNCATE s.t2
```
Both:

```
OK Void
```
### truncate_table_keyword

```cql
TRUNCATE TABLE s.t2
```
Both:

```
OK Void
```
## SELECT

### select_star

```cql
SELECT * FROM s.t WHERE pk = 1
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  1 | 1 | 100 | ['Main', 1, None] | None | [7, 8] | [1, 2, 3] | OrderedMapSerializedKey([('a', 1), ('b', 2)]) | SortedSet(['a', 'b']) | [1, 'x'] | one | 10
  1 | 2 | 100 | None | None | None | None | None | None | None | two | 20
  1 | 3 | 100 | None | None | None | None | None | None | None | three | 30
```
### select_star_all

```cql
SELECT * FROM s.t
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  1 | 1 | 100 | ['Main', 1, None] | None | [7, 8] | [1, 2, 3] | OrderedMapSerializedKey([('a', 1), ('b', 2)]) | SortedSet(['a', 'b']) | [1, 'x'] | one | 10
  1 | 2 | 100 | None | None | None | None | None | None | None | two | 20
  1 | 3 | 100 | None | None | None | None | None | None | None | three | 30
  2 | 1 | None | None | None | None | None | None | None | None | None | 5
  3 | None | 33 | None | None | None | None | None | None | None | None | None
```
### select_columns_order_desc

```cql
SELECT pk, ck, v FROM s.t WHERE pk = 1 ORDER BY ck DESC
```
Both:

```
columns: pk int, ck int, v int
  1 | 3 | 30
  1 | 2 | 20
  1 | 1 | 10
```
### select_static_only_partition

```cql
SELECT * FROM s.t WHERE pk = 3
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  3 | None | 33 | None | None | None | None | None | None | None | None | None
```
### select_distinct

```cql
SELECT DISTINCT pk, s FROM s.t
```
Both:

```
columns: pk int, s int
  1 | 100
  2 | None
  3 | 33
```
### select_distinct_token

```cql
SELECT DISTINCT token(pk) FROM s.t
```
Both:

```
columns: system.token(pk) bigint
  -4069959284402364209
  -3248873570005575792
  9010454139840013625
```
### select_token

```cql
SELECT token(pk), pk FROM s.t WHERE pk = 1 LIMIT 1
```
Both:

```
columns: system.token(pk) bigint, pk int
  -4069959284402364209 | 1
```
### select_token_range

```cql
SELECT pk, ck FROM s.t WHERE token(pk) > -9223372036854775808 AND token(pk) <= 0
```
Both:

```
columns: pk int, ck int
  1 | 1
  1 | 2
  1 | 3
  2 | 1
```
### select_aggregates

```cql
SELECT count(*), count(v), min(v), max(v), sum(v), avg(v) FROM s.t WHERE pk = 1
```
Both:

```
columns: count bigint, system.count(v) bigint, system.min(v) int, system.max(v) int, system.sum(v) int, system.avg(v) int
  3 | 3 | 10 | 30 | 60 | 20
```
### select_count_1

```cql
SELECT count(1) FROM s.t WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
columns: system.count(1) bigint
  3
```
Cassandra 5.0.9:

```
columns: count bigint
  3
```
### select_count_empty

```cql
SELECT count(*), max(v) FROM s.t WHERE pk = 99
```
Both:

```
columns: count bigint, system.max(v) int
  0 | None
```
### select_aggregate_and_column

```cql
SELECT pk, count(*) FROM s.t WHERE pk = 1
```
Both:

```
columns: pk int, count bigint
  1 | 3
```
### select_aliases

```cql
SELECT v AS "Value", count(*) AS n FROM s.t WHERE pk = 1
```
Both:

```
columns: Value int, n bigint
  10 | 3
```
### select_alias_lower

```cql
SELECT v AS Value FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: value int
  10
```
### select_writetime_ttl

```cql
SELECT writetime(v), ttl(v), writetime(s), writetime(txt) FROM s.t WHERE pk = 1
```
Both:

```
columns: writetime(v) bigint, ttl(v) int, writetime(s) bigint, writetime(txt) bigint
  1000 | None | 1000 | 1000
  1000 | None | 1000 | 1000
  1000 | None | 1000 | 1000
```
### select_writetime_collection

```cql
SELECT writetime(l), writetime(st) FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` WRITETIME expects an atomic column, but l is a non-frozen collection
```
Cassandra 5.0.9:

```
columns: writetime(l) list<bigint>, writetime(st) list<bigint>
  [1000, 1000, 1000] | [1000, 1000]
```
### select_writetime_frozen_udt

```cql
SELECT writetime(fl), writetime(tu), writetime(ad) FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` WRITETIME expects an atomic column, but ad is a non-frozen collection
```
Cassandra 5.0.9:

```
error `None` Failed decoding result column "writetime(ad)" of type list<bigint>: Length must be positive
raw re-run: OK Rows
```
### select_maxwritetime

```cql
SELECT maxwritetime(v) FROM s.t WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` Unknown function maxwritetime called
```
Cassandra 5.0.9:

```
columns: maxwritetime(v) bigint
  1000
  1000
  1000
```
### select_cast

```cql
SELECT CAST(v AS text), CAST(v AS double), CAST(v AS varint), CAST(v AS decimal), CAST(v AS bigint) FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
columns: cast(v as text) varchar, cast(v as double) double, cast(v as varint) varint, cast(v as decimal) decimal, cast(v as bigint) bigint
  10 | 10.0 | 10 | Decimal('10.0') | 10
```
Cassandra 5.0.9:

```
columns: cast(v as text) varchar, cast(v as double) double, cast(v as varint) varint, cast(v as decimal) decimal, cast(v as bigint) bigint
  10 | 10.0 | 10 | Decimal('10') | 10
```
### select_cast_lowercase

```cql
SELECT cast(v as text) FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: cast(v as text) varchar
  10
```
### select_cast_all_text

```cql
SELECT CAST(a AS text), CAST(bi AS text), CAST(b AS text), CAST(bo AS text), CAST(d AS text), CAST(de AS text), CAST(db AS text), CAST(du AS text), CAST(f AS text), CAST(i AS text), CAST(n AS text), CAST(sm AS text), CAST(tm AS text), CAST(ts AS text), CAST(tid AS text), CAST(ti AS text), CAST(u AS text), CAST(vi AS text) FROM s.ty WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` org.apache.cassandra.db.marshal.BytesType cannot be cast to org.apache.cassandra.db.marshal.UTF8Type
```
Cassandra 5.0.9:

```
error `0x2200` Invalid call to function system.cast_as_text, none of its type signatures match (known type signatures: system.cast_as_text : (boolean) -> text, system.cast_as_text : (uuid) -> text, system.cast_as_text : (int) -> text, system.cast_as_text : (counter) -> text, system.cast_as_text : (float) -> text, system.cast_as_text : (date) -> text, system.cast_as_text : (smallint) -> text, system.cast_as_text … (781 chars)
```
### select_cast_all_text_2

```cql
SELECT CAST(de AS text), CAST(db AS text), CAST(f AS text), CAST(i AS text), CAST(ts AS text) FROM s.ty WHERE pk = 2
```
ScyllaDB 2026.3.3:

```
columns: cast(de as text) varchar, cast(db as text) varchar, cast(f as text) varchar, cast(i as text) varchar, cast(ts as text) varchar
  -0.000001 | nan | -inf | 192.168.0.1 | 1970-01-01T00:00:00.000Z
```
Cassandra 5.0.9:

```
columns: cast(de as text) varchar, cast(db as text) varchar, cast(f as text) varchar, cast(i as text) varchar, cast(ts as text) varchar
  -0.000001 | NaN | -Infinity | 192.168.0.1 | 1970-01-01T00:00:00.000Z
```
### select_json_types

```cql
SELECT JSON * FROM s.ty
```
ScyllaDB 2026.3.3:

```
columns: [json] varchar
  {"pk": 1, "a": "ascii", "b": "0xcafe", "bi": 9223372036854775807, "bo": true, "d": "2024-02-29", "db": 1.5e+300, "de": 123.45, "du": "1y2mo25d5h6m7s8ms9us10ns", "f": 3.25, "i": "::1", "n": -2147483648… (472 chars)
  {"pk": 2, "a": null, "b": null, "bi": null, "bo": null, "d": null, "db": null, "de": -0.000001, "du": null, "f": null, "i": "192.168.0.1", "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "… (292 chars)
  {"pk": 4, "a": "", "b": "0x", "bi": null, "bo": null, "d": null, "db": null, "de": null, "du": null, "f": null, "i": null, "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "ts": null, "tx":… (252 chars)
  {"pk": 3, "a": null, "b": null, "bi": null, "bo": null, "d": null, "db": null, "de": null, "du": null, "f": null, "i": null, "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "ts": null, "tx… (256 chars)
```
Cassandra 5.0.9:

```
columns: [json] varchar
  {"pk": 1, "a": "ascii", "b": "0xcafe", "bi": 9223372036854775807, "bo": true, "d": "2024-02-29", "db": 1.5E300, "de": 123.4500, "du": "1y2mo25d5h6m7s8ms9us10ns", "f": 3.25, "i": "0:0:0:0:0:0:0:1", "n"… (489 chars)
  {"pk": 2, "a": null, "b": null, "bi": null, "bo": null, "d": null, "db": null, "de": -0.000001, "du": null, "f": null, "i": "192.168.0.1", "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "… (292 chars)
  {"pk": 4, "a": "", "b": "0x", "bi": null, "bo": null, "d": null, "db": null, "de": null, "du": null, "f": null, "i": null, "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "ts": null, "tx":… (252 chars)
  {"pk": 3, "a": null, "b": null, "bi": null, "bo": null, "d": null, "db": null, "de": null, "du": null, "f": null, "i": null, "n": null, "sm": null, "ti": null, "tid": null, "tm": null, "ts": null, "tx… (256 chars)
```
### select_json_t

```cql
SELECT JSON * FROM s.t WHERE pk = 1
```
Both:

```
columns: [json] varchar
  {"pk": 1, "ck": 1, "s": 100, "ad": {"street": "Main", "zip": 1, "town": null}, "extra": null, "fl": [7, 8], "l": [1, 2, 3], "m": {"a": 1, "b": 2}, "st": ["a", "b"], "tu": [1, "x"], "txt": "one", "v": … (203 chars)
  {"pk": 1, "ck": 2, "s": 100, "ad": null, "extra": null, "fl": null, "l": null, "m": null, "st": null, "tu": null, "txt": "two", "v": 20}
  {"pk": 1, "ck": 3, "s": 100, "ad": null, "extra": null, "fl": null, "l": null, "m": null, "st": null, "tu": null, "txt": "three", "v": 30}
```
### select_json_columns_alias

```cql
SELECT JSON pk, v AS "Val", writetime(v) FROM s.t WHERE pk = 1
```
Both:

```
columns: [json] varchar
  {"pk": 1, "\"Val\"": 10, "writetime(v)": 1000}
  {"pk": 1, "\"Val\"": 20, "writetime(v)": 1000}
  {"pk": 1, "\"Val\"": 30, "writetime(v)": 1000}
```
### select_tojson

```cql
SELECT toJson(l), toJson(m), toJson(st), toJson(tu), toJson(ad), toJson(fl) FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: system.tojson(l) varchar, system.tojson(m) varchar, system.tojson(st) varchar, system.tojson(tu) varchar, system.tojson(ad) varchar, system.tojson(fl) varchar
  [1, 2, 3] | {"a": 1, "b": 2} | ["a", "b"] | [1, "x"] | {"street": "Main", "zip": 1, "town": null} | [7, 8]
```
### select_tojson_types

```cql
SELECT toJson(b), toJson(du), toJson(ts), toJson(tm), toJson(d), toJson(vi), toJson(vec) FROM s.ty WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
columns: system.tojson(b) varchar, system.tojson(du) varchar, system.tojson(ts) varchar, system.tojson(tm) varchar, system.tojson(d) varchar, system.tojson(vi) varchar, system.tojson(vec) varchar
  "0xcafe" | "1y2mo25d5h6m7s8ms9us10ns" | "2024-02-29 12:34:56.789Z" | "13:14:15.123456789" | "2024-02-29" | -123456789012345678901234567890 | [1, -2.5, 3]
```
Cassandra 5.0.9:

```
columns: system.tojson(b) varchar, system.tojson(du) varchar, system.tojson(ts) varchar, system.tojson(tm) varchar, system.tojson(d) varchar, system.tojson(vi) varchar, system.tojson(vec) varchar
  "0xcafe" | "1y2mo25d5h6m7s8ms9us10ns" | "2024-02-29 12:34:56.789Z" | "13:14:15.123456789" | "2024-02-29" | -123456789012345678901234567890 | [1.0, -2.5, 3.0]
```
### select_star_types

```cql
SELECT * FROM s.ty WHERE pk = 1
```
Both:

```
columns: pk int, a ascii, b blob, bi bigint, bo boolean, d date, db double, de decimal, du duration, f float, i inet, n int, sm smallint, ti tinyint, tid timeuuid, tm time, ts timestamp, tx varchar, u uuid, vc varchar, vec org.apache.cassandra.db.marshal.VectorType<float, 3>, vi varint
  1 | ascii | b'\xca\xfe' | 9223372036854775807 | True | Date(19782) | 1.5e+300 | Decimal('123.4500') | Duration(14, 25, 18367008009010) | 3.25 | ::1 | -2147483648 | -32768 | -128 | UUID('50554d6e-29bb-11e5-b345-feff819cdc9f') | Time(47655123456789) | datetime.datetime(2024, 2, 29, 12, 34, 56, 789000) | tëxt | UUID('6ba7b810-9dad-41d1-80b4-00c04fd430c8') | varchar | [1.0, -2.5, 3.0] | -123456789012345678901234567890
```
### select_empty_values

```cql
SELECT * FROM s.ty WHERE pk = 4
```
Both:

```
columns: pk int, a ascii, b blob, bi bigint, bo boolean, d date, db double, de decimal, du duration, f float, i inet, n int, sm smallint, ti tinyint, tid timeuuid, tm time, ts timestamp, tx varchar, u uuid, vc varchar, vec org.apache.cassandra.db.marshal.VectorType<float, 3>, vi varint
  4 |  | b'' | None | None | None | None | None | None | None | None | None | None | None | None | None | None |  | None | None | None | None
```
### select_blob_functions

```cql
SELECT intAsBlob(v), blobAsInt(intAsBlob(v)), textAsBlob(txt), varcharAsBlob(txt) FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: system.intasblob(v) blob, system.blobasint(system.intasblob(v)) int, system.textasblob(txt) blob, system.varcharasblob(txt) blob
  b'\x00\x00\x00\n' | 10 | b'one' | b'one'
```
### select_time_functions

```cql
SELECT toDate(ts), toTimestamp(d), toUnixTimestamp(ts), dateOf(tid), unixTimestampOf(tid), toTimestamp(tid), minTimeuuid('2024-01-01 00:00:00+0000'), maxTimeuuid('2024-01-01 00:00:00+0000') FROM s.ty WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
columns: system.todate(ts) date, system.totimestamp(d) timestamp, system.tounixtimestamp(ts) bigint, system.dateof(tid) timestamp, system.unixtimestampof(tid) bigint, system.totimestamp(tid) timestamp, b4cc8000-a838-11ee-8080-808080808080 timeuuid, b4cca70f-a838-11ee-7f7f-7f7f7f7f7f7f timeuuid
  Date(19782) | datetime.datetime(2024, 2, 29, 0, 0) | 1709210096789 | datetime.datetime(2015, 7, 14, 0, 0, 18, 805000) | 1436832018805 | datetime.datetime(2015, 7, 14, 0, 0, 18, 805000) | UUID('b4cc8000-a838-11ee-8080-808080808080') | UUID('b4cca70f-a838-11ee-7f7f-7f7f7f7f7f7f')
```
Cassandra 5.0.9:

```
error `0x2200` Unknown function 'dateof'
```
### select_time_functions_c5

```cql
SELECT to_date(ts), to_unix_timestamp(ts), min_timeuuid('2024-01-01 00:00:00+0000') FROM s.ty WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` Unknown function to_date called
```
Cassandra 5.0.9:

```
columns: system.to_date(ts) date, system.to_unix_timestamp(ts) bigint, system.min_timeuuid('2024-01-01 00:00:00+0000') timeuuid
  Date(19782) | 1709210096789 | UUID('b4cc8000-a838-11ee-8080-808080808080')
```
### select_now_columns

```cql
SELECT now(), currentTimestamp(), currentDate(), currentTime(), currentTimeUUID(), uuid() FROM system.local
```
ScyllaDB 2026.3.3:

```
columns: system.now() timeuuid, system.currenttimestamp() timestamp, system.currentdate() date, system.currenttime() time, system.currenttimeuuid() timeuuid, system.uuid() uuid
  UUID('4add6c70-ccb8-11f1-9bb8-17f08281b1e3') | datetime.datetime(2026, 10, 20, 18, 58, 52, 727000) | Date(20746) | Time(68332727000000) | UUID('4add6c71-ccb8-11f1-9bb8-17f08281b1e3') | UUID('c6b83cf1-46b8-4efd-b0b2-5b40f9665983')
```
Cassandra 5.0.9:

```
columns: system.now() timeuuid, system.currenttimestamp() timestamp, system.currentdate() date, system.currenttime() time, system.currenttimeuuid() timeuuid, system.uuid() uuid
  UUID('4e63fb20-ccb8-11f1-8c36-33d044eb584e') | datetime.datetime(2026, 10, 20, 18, 58, 58, 642000) | Date(20746) | Time(68338642173029) | UUID('4e63fb2a-ccb8-11f1-8c36-33d044eb584e') | UUID('058a47b3-a9de-49d7-9f64-b3f4305b9c19')
```
### select_arith

```cql
SELECT v * 2, v - 1, v / 3, v % 7, -v FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2000` line 1:24 no viable alternative at input ''
```
Cassandra 5.0.9:

```
columns: v * 2 int, v - 1 int, v / 3 int, v % 7 int, -v int
  20 | 9 | 3 | 3 | -10
```
### select_arith_plus

```cql
SELECT v + 1 FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2000` line 1:7 no viable alternative at input 'v'
```
Cassandra 5.0.9:

```
columns: v + 1 int
  11
```
### select_literal

```cql
SELECT 1, 'a', (int) 5 FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2000` line 1:19 no viable alternative at input ')'
```
Cassandra 5.0.9:

```
error `0x2200` Cannot infer type for term 1 in selection clause (try using a cast to force a type)
```
### select_element

```cql
SELECT l[0], m['a'], st['a'] FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
columns: l[0] int, m['a'] int, st['a'] varchar
  1 | 1 | a
```
Cassandra 5.0.9:

```
error `0x2200` Element selection is only allowed on sets and maps, but l is a list
```
### select_slice

```cql
SELECT m['a'..'b'], st['a'..'z'] FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2000` line 1:12 : Missing ']'
```
Cassandra 5.0.9:

```
columns: m['a'..'b'] map<varchar, int>, st['a'..'z'] set<varchar>
  OrderedMapSerializedKey([('a', 1), ('b', 2)]) | SortedSet(['a', 'b'])
```
### select_udt_field

```cql
SELECT ad.street, ad.zip, ad FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: ad.street varchar, ad.zip int, ad frozen<addr>
  Main | 1 | ['Main', 1, None]
```
### select_c5_functions

```cql
SELECT map_keys(m), map_values(m), collection_count(l), collection_min(l), collection_max(l), collection_sum(l), collection_avg(l), abs(v), exp(v), log(v), log10(v), round(v) FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` Unknown function map_keys called
```
Cassandra 5.0.9:

```
columns: system.map_keys(m) set<varchar>, system.map_values(m) list<int>, system.collection_count(l) int, system.collection_min(l) int, system.collection_max(l) int, system.collection_sum(l) int, system.collection_avg(l) int, system.abs(v) int, system.exp(v) int, system.log(v) int, system.log10(v) int, system.round(v) int
  SortedSet(['a', 'b']) | [1, 2] | 3 | 1 | 3 | 6 | 2 | 10 | 22026 | 2 | 1 | 10
```
### select_mask_functions

```cql
SELECT mask_null(v), mask_default(v), mask_default(txt), mask_replace(txt, '*'), mask_inner(txt, 1, 1), mask_outer(txt, 1, 1), mask_hash(txt) FROM s.t WHERE pk = 1 AND ck = 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` Unknown function mask_null called
```
Cassandra 5.0.9:

```
columns: system.mask_null(v) int, system.mask_default(v) int, system.mask_default(txt) varchar, system.mask_replace(txt, '*') varchar, system.mask_inner(txt, 1, 1) varchar, system.mask_outer(txt, 1, 1) varchar, system.mask_hash(txt) blob
  None | 0 | **** | * | o*e | *n* | b'v\x92\xc3\xad5@\xbb\x80<\x02\x0b:\xeef\xcd\x88\x87\x1224\xea\x0cnqC\xc0\xad\xd7?\xf41\xed'
```
### select_vector_similarity

```cql
SELECT similarity_cosine(vec, [1.0, 1.0, 1.0]), similarity_euclidean(vec, [1.0, 1.0, 1.0]), similarity_dot_product(vec, [1.0, 1.0, 1.0]) FROM s.ty WHERE pk = 1
```
ScyllaDB 2026.3.3:

```
columns: system.similarity_cosine(vec, [1, 1, 1]) float, system.similarity_euclidean(vec, [1, 1, 1]) float, system.similarity_dot_product(vec, [1, 1, 1]) float
  0.6074172258377075 | 0.05797101557254791 | 1.25
```
Cassandra 5.0.9:

```
columns: system.similarity_cosine(vec, [1.0, 1.0, 1.0]) float, system.similarity_euclidean(vec, [1.0, 1.0, 1.0]) float, system.similarity_dot_product(vec, [1.0, 1.0, 1.0]) float
  0.6074172258377075 | 0.05797101557254791 | 1.25
```
### select_group_by_pk

```cql
SELECT pk, count(*), max(v) FROM s.t GROUP BY pk
```
ScyllaDB 2026.3.3:

```
columns: pk int, count bigint, system.max(v) int
  1 | 3 | 30
  2 | 1 | 5
  3 | 1 | None
```
Cassandra 5.0.9:

```
columns: pk int, count bigint, system.max(v) int
  1 | 3 | 30
  2 | 1 | 5
  3 | 1 | None
warnings: ['Aggregation query used without partition key']
```
### select_group_by_ck

```cql
SELECT a, b, c, count(*) FROM s.t2 WHERE a = 1 AND b = 1 GROUP BY a, b, c
```
Both:

```
columns: a int, b int, c int, count bigint
  1 | 1 | 2 | 1
  1 | 1 | 1 | 2
```
### select_group_by_no_agg

```cql
SELECT a, b, c, d, v FROM s.t2 WHERE a = 1 AND b = 1 GROUP BY c
```
Both:

```
columns: a int, b int, c int, d int, v int
  1 | 1 | 2 | 1 | 3
  1 | 1 | 1 | 1 | 1
```
### select_per_partition_limit

```cql
SELECT pk, ck FROM s.t PER PARTITION LIMIT 1
```
Both:

```
columns: pk int, ck int
  1 | 1
  2 | 1
  3 | None
```
### select_in_order_by_unpaged

```cql
SELECT pk, ck FROM s.t WHERE pk IN (2, 1) ORDER BY ck DESC
```
ScyllaDB 2026.3.3:

```
columns: pk int, ck int
  1 | 3
  1 | 2
  2 | 1
  1 | 1
```
Cassandra 5.0.9:

```
columns: pk int, ck int
  1 | 3
  1 | 2
  1 | 1
  2 | 1
```
### select_in_order

```cql
SELECT pk, ck FROM s.t WHERE pk IN (2, 1)
```
Both:

```
columns: pk int, ck int
  1 | 1
  1 | 2
  1 | 3
  2 | 1
```
### select_in_clustering

```cql
SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND c IN (2, 1)
```
Both:

```
columns: a int, b int, c int, d int, v int
  1 | 1 | 2 | 1 | 3
  1 | 1 | 1 | 1 | 1
  1 | 1 | 1 | 2 | 2
```
### select_multi_column

```cql
SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND (c, d) > (1, 1)
```
Both:

```
columns: a int, b int, c int, d int, v int
  1 | 1 | 2 | 1 | 3
  1 | 1 | 1 | 2 | 2
```
### select_multi_column_in

```cql
SELECT * FROM s.t2 WHERE a = 1 AND b = 1 AND (c, d) IN ((1, 2), (2, 1))
```
Both:

```
columns: a int, b int, c int, d int, v int
  1 | 1 | 2 | 1 | 3
  1 | 1 | 1 | 2 | 2
```
### select_clustering_desc_natural

```cql
SELECT c, d FROM s.t2 WHERE a = 1 AND b = 1
```
Both:

```
columns: c int, d int
  2 | 1
  1 | 1
  1 | 2
```
### select_clustering_reversed

```cql
SELECT c, d FROM s.t2 WHERE a = 1 AND b = 1 ORDER BY c ASC, d DESC
```
Both:

```
columns: c int, d int
  1 | 2
  1 | 1
  2 | 1
```
### select_filtering

```cql
SELECT pk, ck FROM s.t WHERE v > 10 ALLOW FILTERING
```
Both:

```
columns: pk int, ck int
  1 | 2
  1 | 3
```
### select_filtering_contains

```cql
SELECT pk, ck FROM s.t WHERE l CONTAINS 2 ALLOW FILTERING
```
Both:

```
columns: pk int, ck int
  1 | 1
```
### select_filtering_contains_key

```cql
SELECT pk, ck FROM s.t WHERE m CONTAINS KEY 'a' ALLOW FILTERING
```
Both:

```
columns: pk int, ck int
  1 | 1
```
### select_filtering_map_entry

```cql
SELECT pk, ck FROM s.t WHERE m['a'] = 1 ALLOW FILTERING
```
Both:

```
columns: pk int, ck int
  1 | 1
```
### select_index

```cql
SELECT pk, ck, v FROM s.t WHERE v = 20
```
Both:

```
columns: pk int, ck int, v int
  1 | 2 | 20
```
### select_index_with_pk

```cql
SELECT pk, ck, v FROM s.t WHERE pk = 1 AND v = 20
```
Both:

```
columns: pk int, ck int, v int
  1 | 2 | 20
```
### select_limit_paged

```cql
SELECT pk, ck FROM s.t LIMIT 3
```
Both:

```
columns: pk int, ck int
  1 | 1
  1 | 2
has_more_pages=True paging_state=True
```
### select_page_size_2

```cql
SELECT pk, ck FROM s.t WHERE pk = 1
```
Both:

```
columns: pk int, ck int
  1 | 1
  1 | 2
has_more_pages=True paging_state=True
```
### select_page_size_exact

```cql
SELECT pk, ck FROM s.t WHERE pk = 1
```
Both:

```
columns: pk int, ck int
  1 | 1
  1 | 2
  1 | 3
has_more_pages=True paging_state=True
```
### select_empty_result

```cql
SELECT * FROM s.t WHERE pk = 99
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
```
### select_system_local_some

```cql
SELECT key, cluster_name, data_center, rack, partitioner, release_version, cql_version, native_protocol_version FROM system.local
```
ScyllaDB 2026.3.3:

```
columns: key varchar, cluster_name varchar, data_center varchar, rack varchar, partitioner varchar, release_version varchar, cql_version varchar, native_protocol_version varchar
  local |  | datacenter1 | rack1 | org.apache.cassandra.dht.Murmur3Partitioner | 3.0.8 | 3.3.1 | 4
```
Cassandra 5.0.9:

```
columns: key varchar, cluster_name varchar, data_center varchar, rack varchar, partitioner varchar, release_version varchar, cql_version varchar, native_protocol_version varchar
  local | Test Cluster | datacenter1 | rack1 | org.apache.cassandra.dht.Murmur3Partitioner | 5.0.9 | 3.4.7 | 5
```
### select_tracing_flag

```cql
SELECT * FROM s.t WHERE pk = 1
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  1 | 1 | 100 | ['Main', 1, None] | None | [7, 8] | [1, 2, 3] | OrderedMapSerializedKey([('a', 1), ('b', 2)]) | SortedSet(['a', 'b']) | [1, 'x'] | one | 10
  1 | 2 | 100 | None | None | None | None | None | None | None | two | 20
  1 | 3 | 100 | None | None | None | None | None | None | None | three | 30
```
## Writes and read-back

### collection_ops

```cql
SELECT l, st, m FROM s.t WHERE pk = 1 AND ck = 2
```
Both:

```
columns: l list<int>, st set<varchar>, m map<varchar, int>
  [0] | SortedSet(['c']) | OrderedMapSerializedKey([('y', 25), ('z', 26)])
```
### list_set_by_index

```cql
SELECT l FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: l list<int>
  [1, 99, 3]
```
### delete_element

```cql
SELECT l, m, st FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: l list<int>, m map<varchar, int>, st set<varchar>
  [99, 3] | OrderedMapSerializedKey([('b', 2)]) | SortedSet(['b'])
```
### set_to_empty_collection

```cql
SELECT l, st, m FROM s.t WHERE pk = 1 AND ck = 3
```
Both:

```
columns: l list<int>, st set<varchar>, m map<varchar, int>
  None | None | None
```
### udt_field_update

```cql
SELECT ad FROM s.t WHERE pk = 1 AND ck = 1
```
Both:

```
columns: ad frozen<addr>
  ['Main', 2, None]
```
### update_creates_no_row_marker

```cql
SELECT * FROM s.t WHERE pk = 10
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
```
### insert_keeps_row_marker

```cql
SELECT * FROM s.t WHERE pk = 11
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  11 | 1 | None | None | None | None | None | None | None | None | None | None
```
### range_delete

```cql
SELECT c, d FROM s.t2 WHERE a = 5 AND b = 5
```
Both:

```
columns: c int, d int
  1 | 1
```
### tie_same_timestamp_values

```cql
SELECT v, txt FROM s.t WHERE pk = 20
```
Both:

```
columns: v int, txt varchar
  5 | z
```
### tie_delete_vs_write

```cql
SELECT * FROM s.t WHERE pk = 21
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  21 | 1 | None | None | None | None | None | None | None | None | None | None
```
### tie_ttl_vs_plain

```cql
SELECT v, ttl(v) FROM s.t WHERE pk = 22
```
Both:

```
columns: v int, ttl(v) int
  5 | 100000
```
### older_write_ignored

```cql
SELECT v, writetime(v) FROM s.t WHERE pk = 23
```
Both:

```
columns: v int, writetime(v) bigint
  5 | 5000
```
### insert_json_default_null

```cql
SELECT v, txt FROM s.t WHERE pk = 30
```
Both:

```
columns: v int, txt varchar
  2 | None
```
### insert_json_default_unset

```cql
SELECT v, txt FROM s.t WHERE pk = 31
```
Both:

```
columns: v int, txt varchar
  2 | a
```
### insert_json_types

```cql
SELECT JSON pk, b, du, ts, d, i, vi, de, vec FROM s.ty WHERE pk = 10
```
ScyllaDB 2026.3.3:

```
columns: [json] varchar
  {"pk": 10, "b": "0xcafe", "du": "1h", "ts": "2024-01-01 00:00:00.000Z", "d": "2024-01-01", "i": "10.0.0.1", "vi": 12345678901234567890, "de": 1.5, "vec": [1, 2, 3]}
```
Cassandra 5.0.9:

```
columns: [json] varchar
  {"pk": 10, "b": "0xcafe", "du": "1h", "ts": "2024-01-01 00:00:00.000Z", "d": "2024-01-01", "i": "10.0.0.1", "vi": 12345678901234567890, "de": 1.50, "vec": [1.0, 2.0, 3.0]}
```
### fromjson

```cql
SELECT l, m FROM s.t WHERE pk = 32
```
Both:

```
columns: l list<int>, m map<varchar, int>
  [1, 2] | OrderedMapSerializedKey([('k', 3)])
```
### counter_ops

```cql
SELECT * FROM s.c
```
Both:

```
columns: pk int, ck int, n counter
  1 | 1 | 3
```
### counter_never_set

```cql
SELECT n FROM s.c WHERE pk = 2 AND ck = 2
```
Both:

```
columns: n counter
```
### static_insert_without_ck

```cql
SELECT * FROM s.t WHERE pk = 40
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  40 | None | 1 | None | None | None | None | None | None | None | None | None
```
### static_then_row

```cql
SELECT * FROM s.t WHERE pk = 40
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  40 | 1 | 1 | None | None | None | None | None | None | None | None | 1
```
### delete_partition

```cql
SELECT * FROM s.t WHERE pk = 40
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
```
### delete_static_only

```cql
SELECT * FROM s.t WHERE pk = 41
```
Both:

```
columns: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  41 | 1 | None | None | None | None | None | None | None | None | None | 1
```
### ttl_value

```cql
SELECT ttl(v) FROM s.t WHERE pk = 42
```
Both:

```
columns: ttl(v) int
  86400
```
### ttl_zero

```cql
SELECT ttl(v) FROM s.t WHERE pk = 43
```
Both:

```
columns: ttl(v) int
  None
```
### batch_results

```cql
BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (44, 1, 1); UPDATE s.t SET v = 2 WHERE pk = 44 AND ck = 2; DELETE FROM s.t WHERE pk = 44 AND ck = 3; APPLY BATCH
```
Both:

```
columns: 
```
## Lightweight transactions

### lwt_insert_applied

```cql
INSERT INTO s.t (pk, ck, v) VALUES (50, 1, 1) IF NOT EXISTS
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  True | None | None | None | None | None | None | None | None | None | None | None | None
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_insert_not_applied

```cql
INSERT INTO s.t (pk, ck, v) VALUES (50, 1, 2) IF NOT EXISTS
```
Both:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  False | 50 | 1 | None | None | None | None | None | None | None | None | None | 1
```
### lwt_update_applied

```cql
UPDATE s.t SET v = 3 WHERE pk = 50 AND ck = 1 IF v = 1
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, v int
  True | 1
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_update_not_applied

```cql
UPDATE s.t SET v = 4 WHERE pk = 50 AND ck = 1 IF v = 1
```
Both:

```
columns: [applied] boolean, v int
  False | 3
```
### lwt_update_not_applied_two_cols

```cql
UPDATE s.t SET v = 4 WHERE pk = 50 AND ck = 1 IF v = 1 AND txt = 'x'
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, txt varchar, v int
  False | None | 3
```
Cassandra 5.0.9:

```
columns: [applied] boolean, v int, txt varchar
  False | 3 | None
```
### lwt_update_if_exists_missing

```cql
UPDATE s.t SET v = 4 WHERE pk = 51 AND ck = 1 IF EXISTS
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  False | None | None | None | None | None | None | None | None | None | None | None | None
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  False
```
### lwt_update_if_exists

```cql
UPDATE s.t SET v = 5 WHERE pk = 50 AND ck = 1 IF EXISTS
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  True | 50 | 1 | None | None | None | None | None | None | None | None | None | 3
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_delete_if_exists

```cql
DELETE FROM s.t WHERE pk = 50 AND ck = 1 IF EXISTS
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  True | 50 | 1 | None | None | None | None | None | None | None | None | None | 5
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_delete_if_exists_missing

```cql
DELETE FROM s.t WHERE pk = 50 AND ck = 1 IF EXISTS
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  False | None | None | None | None | None | None | None | None | None | None | None | None
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  False
```
### lwt_delete_condition_not_applied

```cql
DELETE FROM s.t WHERE pk = 1 AND ck = 2 IF v = 999
```
Both:

```
columns: [applied] boolean, v int
  False | 20
```
### lwt_static_condition

```cql
UPDATE s.t SET s = 7 WHERE pk = 1 IF s = 100
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, s int
  True | 100
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_static_condition_not_applied

```cql
UPDATE s.t SET s = 8 WHERE pk = 1 IF s = 100
```
Both:

```
columns: [applied] boolean, s int
  False | 7
```
### lwt_in_condition

```cql
UPDATE s.t SET v = 21 WHERE pk = 1 AND ck = 2 IF v IN (20, 21)
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, v int
  True | 20
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_null_condition

```cql
UPDATE s.t SET v = 21 WHERE pk = 1 AND ck = 2 IF txt = null
```
Both:

```
columns: [applied] boolean, txt varchar
  False | two
```
### lwt_ne_condition

```cql
UPDATE s.t SET v = 22 WHERE pk = 1 AND ck = 2 IF v != 21
```
Both:

```
columns: [applied] boolean, v int
  False | 21
```
### lwt_collection_element_condition

```cql
UPDATE s.t SET v = 23 WHERE pk = 1 AND ck = 1 IF m['b'] = 2
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, m map<varchar, int>
  True | OrderedMapSerializedKey([('b', 2)])
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_udt_field_condition

```cql
UPDATE s.t SET v = 23 WHERE pk = 1 AND ck = 1 IF ad.zip = 9
```
Both:

```
columns: [applied] boolean, ad frozen<addr>
  False | ['Main', 2, None]
```
### lwt_batch_applied

```cql
BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (52, 1, 1) IF NOT EXISTS; INSERT INTO s.t (pk, ck, v) VALUES (52, 2, 1) IF NOT EXISTS; APPLY BATCH
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  True | None | None | None | None | None | None | None | None | None | None | None | None
  True | None | None | None | None | None | None | None | None | None | None | None | None
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
### lwt_batch_not_applied

```cql
BEGIN BATCH INSERT INTO s.t (pk, ck, v) VALUES (52, 1, 1) IF NOT EXISTS; UPDATE s.t SET v = 9 WHERE pk = 52 AND ck = 3 IF v = 1; APPLY BATCH
```
ScyllaDB 2026.3.3:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  False | 52 | 1 | None | None | None | None | None | None | None | None | None | 1
  False | 52 | None | None | None | None | None | None | None | None | None | None | None
```
Cassandra 5.0.9:

```
columns: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
  False | 52 | 1 | None | None | None | None | None | None | None | None | None | 1
```
### lwt_counter_free_insert_json

```cql
INSERT INTO s.t JSON '{"pk": 53, "ck": 1}' IF NOT EXISTS
```
ScyllaDB 2026.3.3:

```
columns: 
```
Cassandra 5.0.9:

```
columns: [applied] boolean
  True
```
## Prepared statement metadata

### prep_select_pk

```cql
SELECT * FROM s.t WHERE pk = ?
```
Both:

```
bind: pk int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [0]
```
### prep_select_named

```cql
SELECT v FROM s.t WHERE pk = :p AND ck = :c
```
Both:

```
bind: p int, c int
result: v int
routing_key_indexes: [0]
```
### prep_select_in

```cql
SELECT * FROM s.t WHERE pk IN ?
```
ScyllaDB 2026.3.3:

```
bind: IN(pk) list<int>
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
Cassandra 5.0.9:

```
bind: in(pk) list<int>
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_in_markers

```cql
SELECT * FROM s.t WHERE pk IN (?, ?)
```
ScyllaDB 2026.3.3:

```
bind: value(IN(pk)) int, value(IN(pk)) int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
Cassandra 5.0.9:

```
bind: pk int, pk int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [1]
```
### prep_select_limit

```cql
SELECT * FROM s.t WHERE pk = ? LIMIT ?
```
Both:

```
bind: pk int, [limit] int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [0]
```
### prep_select_ppl

```cql
SELECT * FROM s.t PER PARTITION LIMIT ?
```
ScyllaDB 2026.3.3:

```
bind: [limit] int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
Cassandra 5.0.9:

```
bind: [per_partition_limit] int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_token

```cql
SELECT * FROM s.t WHERE token(pk) > ? AND token(pk) <= ?
```
Both:

```
bind: partition key token bigint, partition key token bigint
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_slice

```cql
SELECT * FROM s.t WHERE pk = ? AND ck > ? AND ck <= ?
```
Both:

```
bind: pk int, ck int, ck int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [0]
```
### prep_select_multi

```cql
SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) > (?, ?)
```
ScyllaDB 2026.3.3:

```
bind: a int, b int, (c,d)[0] int, (c,d)[1] int
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
Cassandra 5.0.9:

```
bind: a int, b int, c int, d int
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
### prep_select_multi_tuple

```cql
SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) > ?
```
Both:

```
bind: a int, b int, (c,d) frozen<tuple<int, int>>
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
### prep_select_multi_in

```cql
SELECT * FROM s.t2 WHERE a = ? AND b = ? AND (c, d) IN ?
```
ScyllaDB 2026.3.3:

```
bind: a int, b int, IN((c,d)) list<frozen<tuple<int, int>>>
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
Cassandra 5.0.9:

```
bind: a int, b int, in(c,d) list<frozen<tuple<int, int>>>
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
### prep_select_contains

```cql
SELECT * FROM s.t WHERE l CONTAINS ? ALLOW FILTERING
```
Both:

```
bind: value(l) int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_map_entry

```cql
SELECT * FROM s.t WHERE m[?] = ? ALLOW FILTERING
```
Both:

```
bind: key(m) varchar, value(m) int
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_composite

```cql
SELECT * FROM s.t2 WHERE a = ? AND b = ?
```
Both:

```
bind: a int, b int
result: a int, b int, c int, d int, v int
routing_key_indexes: [0, 1]
```
### prep_select_composite_reversed

```cql
SELECT * FROM s.t2 WHERE b = ? AND a = ?
```
Both:

```
bind: b int, a int
result: a int, b int, c int, d int, v int
routing_key_indexes: [1, 0]
```
### prep_select_function_arg

```cql
SELECT * FROM s.t WHERE pk = blobAsInt(?)
```
Both:

```
bind: arg0(system.blobasint) blob
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: None
```
### prep_select_cast_arg

```cql
SELECT CAST(? AS text) FROM s.t
```
ScyllaDB 2026.3.3:

```
error `0x2200` Could not infer type of ?
```
Cassandra 5.0.9:

```
error `0x2200` Cannot infer type for term ? in selection clause (try using a cast to force a type)
```
### prep_select_json

```cql
SELECT JSON * FROM s.t WHERE pk = ?
```
Both:

```
bind: pk int
result: [json] varchar
routing_key_indexes: [0]
```
### prep_select_ann

```cql
SELECT * FROM s.ty ORDER BY vec ANN OF ? LIMIT 1
```
ScyllaDB 2026.3.3:

```
error `0x2200` ANN ordering by vector requires the column to be indexed using 'vector_index'
```
Cassandra 5.0.9:

```
error `0x2200` ANN ordering by vector requires the column to be indexed
```
### prep_insert

```cql
INSERT INTO s.t (pk, ck, v) VALUES (?, ?, ?)
```
Both:

```
bind: pk int, ck int, v int
result: 
routing_key_indexes: [0]
```
### prep_insert_ttl_ts

```cql
INSERT INTO s.t (pk, ck, v) VALUES (?, ?, ?) USING TTL ? AND TIMESTAMP ?
```
Both:

```
bind: pk int, ck int, v int, [ttl] int, [timestamp] bigint
result: 
routing_key_indexes: [0]
```
### prep_insert_json

```cql
INSERT INTO s.t JSON ?
```
ScyllaDB 2026.3.3:

```
bind:  varchar
result: 
routing_key_indexes: None
```
Cassandra 5.0.9:

```
bind: [json] varchar
result: 
routing_key_indexes: None
```
### prep_insert_lwt

```cql
INSERT INTO s.t (pk, ck) VALUES (?, ?) IF NOT EXISTS
```
ScyllaDB 2026.3.3:

```
bind: pk int, ck int
result: [applied] boolean, pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [0]
```
Cassandra 5.0.9:

```
bind: pk int, ck int
result: 
routing_key_indexes: [0]
```
### prep_update_collection

```cql
UPDATE s.t SET l = l + ?, m[?] = ?, st = st - ? WHERE pk = ? AND ck = ?
```
Both:

```
bind: l list<int>, key(m) varchar, value(m) int, st set<varchar>, pk int, ck int
result: 
routing_key_indexes: [4]
```
### prep_update_list_index

```cql
UPDATE s.t SET l[?] = ? WHERE pk = ? AND ck = ?
```
Both:

```
bind: idx(l) int, value(l) int, pk int, ck int
result: 
routing_key_indexes: [2]
```
### prep_update_udt_field

```cql
UPDATE s.t SET ad.zip = ? WHERE pk = ? AND ck = ?
```
Both:

```
bind: ad.zip int, pk int, ck int
result: 
routing_key_indexes: [1]
```
### prep_update_lwt

```cql
UPDATE s.t SET v = ? WHERE pk = ? AND ck = ? IF v = ?
```
ScyllaDB 2026.3.3:

```
bind: v int, pk int, ck int, v int
result: [applied] boolean, v int
routing_key_indexes: [1]
```
Cassandra 5.0.9:

```
bind: v int, pk int, ck int, v int
result: 
routing_key_indexes: [1]
```
### prep_update_lwt_element

```cql
UPDATE s.t SET v = ? WHERE pk = ? AND ck = ? IF m[?] = ?
```
ScyllaDB 2026.3.3:

```
bind: v int, pk int, ck int, key(m) varchar, value(m) int
result: [applied] boolean, m map<varchar, int>
routing_key_indexes: [1]
```
Cassandra 5.0.9:

```
bind: v int, pk int, ck int, key(m) varchar, value(m) int
result: 
routing_key_indexes: [1]
```
### prep_counter

```cql
UPDATE s.c SET n = n + ? WHERE pk = ? AND ck = ?
```
Both:

```
bind: n counter, pk int, ck int
result: 
routing_key_indexes: [1]
```
### prep_delete

```cql
DELETE FROM s.t WHERE pk = ? AND ck = ?
```
Both:

```
bind: pk int, ck int
result: 
routing_key_indexes: [0]
```
### prep_delete_element

```cql
DELETE m[?] FROM s.t WHERE pk = ? AND ck = ?
```
Both:

```
bind: key(m) varchar, pk int, ck int
result: 
routing_key_indexes: [1]
```
### prep_delete_timestamp

```cql
DELETE FROM s.t USING TIMESTAMP ? WHERE pk = ?
```
Both:

```
bind: [timestamp] bigint, pk int
result: 
routing_key_indexes: [1]
```
### prep_timeout

```cql
SELECT * FROM s.t WHERE pk = ? USING TIMEOUT ?
```
ScyllaDB 2026.3.3:

```
bind: pk int, [timeout] duration
result: pk int, ck int, s int, ad frozen<addr>, extra varchar, fl list<int>, l list<int>, m map<varchar, int>, st set<varchar>, tu frozen<tuple<int, varchar>>, txt varchar, v int
routing_key_indexes: [0]
```
Cassandra 5.0.9:

```
error `0x2000` line 1:31 no viable alternative at input 'USING' (...s.t WHERE pk = [?] USING...)
```
### prep_system_local

```cql
SELECT * FROM system.local
```
ScyllaDB 2026.3.3:

```
bind: 
result: key varchar, bootstrapped varchar, broadcast_address inet, cluster_name varchar, cql_version varchar, data_center varchar, gossip_generation int, host_id uuid, listen_address inet, native_protocol_version varchar, partitioner varchar, rack varchar, release_version varchar, rpc_address inet, schema_version uuid, supported_features varchar, tokens set<varchar>, truncated_at map<uuid, blob>
routing_key_indexes: None
```
Cassandra 5.0.9:

```
bind: 
result: key varchar, bootstrapped varchar, broadcast_address inet, broadcast_port int, cluster_name varchar, cql_version varchar, data_center varchar, gossip_generation int, host_id uuid, listen_address inet, listen_port int, native_protocol_version varchar, partitioner varchar, rack varchar, release_version varchar, rpc_address inet, rpc_port int, schema_version uuid, tokens set<varchar>, truncated_at map<uuid, blob>
routing_key_indexes: None
```
### prep_star_with_writetime

```cql
SELECT writetime(v), ttl(v), token(pk), count(*) FROM s.t WHERE pk = ?
```
Both:

```
bind: pk int
result: writetime(v) bigint, ttl(v) int, system.token(pk) bigint, count bigint
routing_key_indexes: [0]
```
### prep_batch_member

```cql
BEGIN BATCH INSERT INTO s.t (pk, ck) VALUES (?, ?); APPLY BATCH
```
Both:

```
bind: pk int, ck int
result: 
routing_key_indexes: [0]
```
## Tracing

### tracing_session_id

```cql
SELECT * FROM s.t WHERE pk = 1 (tracing flag)
```
Both:

```
OK Rows
```
Volatile values: results of `now()`, `uuid()`, `currentTimestamp()` and friends, `ttl()` of live cells (counts down), and tracing session ids.
