# Client libraries against the engine

The engine's wire protocol, catalogs and SQL as real drivers and ORMs see them: each client's
own test suite, run once against the engine and once against PostgreSQL 18.6 (the same image as
`roll-engine-ref`, default settings), and compared test by test. Only differences count: a test
that fails on PostgreSQL too (no SSL, no replication, no max_prepared_transactions, a missing
contrib module in the image) is the environment's.

## Running

```sh
# PostgreSQL 18.6 on 127.0.0.1:55838 (see run.sh's header), the engine on 127.0.0.1:55839:
roll-engine --data /some/dir --listen 127.0.0.1:55839 --password pw --no-sync
sg docker -c 'tests/clients/run.sh psycopg3 pg'
sg docker -c 'tests/clients/run.sh psycopg3 engine'
tests/clients/run.sh psycopg3 compare          # the tests whose outcome differs
python3 -I tests/clients/failures.py ~/.cache/pg-clients/results/psycopg3-engine.d \
  ~/.cache/pg-clients/results/psycopg3-pg.txt  # the engine's failures, grouped by message
python3 -I tests/clients/gofail.py <x>.d/pgx.gojson <pg results>   # the same for go test -json
python3 -I tests/clients/wire.py probe.py      # raw protocol transcripts of both servers, diffed
```

- `suites/<name>.sh` runs in a container on the host network (`# image:`), from a pinned source
  tarball (`# src:`); `suites/<name>.setup.sql` makes the databases and roles it needs, through
  the pinned psql. Results land in `$CLIENTS_CACHE/results` (default `~/.cache/pg-clients`).
- Keep at most two suites running at once; each container gets `--cpus=4` (`CLIENTS_CPUS`).
- A suite's first run downloads its toolchain's packages into the cache (pip, Go modules,
  Gradle, NuGet, npm/yarn); later runs are offline-fast.
- Round 18 added tokiopg, sqlx, prisma, activerecord (a first argument starting with `test/`
  is a file glob: `test/cases/adapters/postgresql/**/*_test.rb` is what ran against the engine),
  hibernate (a package subset; assertions off for org.postgresql), typeorm, drizzle and rubypg
  (its spec helper, which runs initdb / pg_ctl, is replaced by one pointing at the server).
  Their caches are large: about 9 GB in all (cargo's target, Gradle).

## Where it stands

Pass / fail per suite (tests that ran; skips aside), each suite against a fresh engine. "Round 17"
is the engine after agent 38's work; "round 18" the engine of round 18, agent 51 (its first
batch, 0a8ec4ee; the suites marked * after its second, 73e8d070). PostgreSQL's counts are this
round's runs against a private reference (pgx's pgxpool package stops at TestPoolBeforeClose
there, so fewer of its tests ran than in round 17).

| suite | version | PostgreSQL | engine, round 17 | engine, round 18 | differences left |
|---|---|---:|---:|---:|---|
| psycopg 3 (tests/, binary impl) | 3.2.9 | 4924 / 9 | 4907 / 26 | 4915 / 18 | 9: hstore binary round trips |
| psycopg2 (tests/) | 2.9.10 | 711 / 2 | 710 / 3 | 711 / 2 | 0 |
| asyncpg (tests/) | 0.30.0 | 262 / 5 | 253 / 14 | 258 / 9 * | 4: hstore codecs (3), introspection timing |
| pgx (root, pgconn, pgtype, pgxpool, stdlib) | 5.7.5 | 1197 / 4 | 1207 / 7 | 1213 / 7 * | 4: hstore binary codec |
| lib/pq | 1.10.9 | 232 / 0 | 230 / 2 | 232 / 0 * | 0 |
| node-postgres (pg integration) | 8.16.0 | 146 / 3 | 146 / 3 | 145 / 4 * | 1: gh-issues/1105 (5000 inserts in 5 s, a debug build) |
| postgres.js | 3.4.7 | 194 / 62 | 194 / 62 | 194 / 62 | 0 |
| pgjdbc (jdbc2/3/4/42, core) | 42.7.7 | 8221 / 0 | 8214 / 7 | 8218 / 3 * | 3: DML on pg_description / pg_class |
| Npgsql (Npgsql.Tests) | 9.0.3 | 1914 / 2 | 1908 / 16 | 1917 / 2 (round 19, agents 53 and 66) | 0 |
| SQLAlchemy (dialect/postgresql, sql) | 2.0.41 | 15801 / 2 | 15755 / 48 | 15801 / 2 * | 0 |
| Django (26 apps) | 5.2.4 | 3793 / 15 | 3789 / 19 | 3792 / 16 | 2: a file_fdw program table (see below), an ungrouped column (round 19) |
| tokio-postgres (tests, postgres, derive) | 0.7.18 | 221 / 0 | | 212 / 9 * | 9: ltree / lquery / ltxtquery / hstore binary (7), startup NOTICE, pg_database order |
| sqlx (9 postgres targets) | 0.9.0 | 333 / 0 | | 318 / 15 * | 15: contrib types (7), domain arrays (3), empty arrays (2), 3 not looked at (see below) |
| Prisma (push, pull, migrate, client) | 7.10.1 | 47 / 0 | | 47 / 0 * | 0 (bit(n) lengths in `db pull`, fixed) |
| ActiveRecord (Rails, PostgreSQL adapter tests) | 8.1.4 | 10159 / 1 | | 748 / 28 | 27: see below (the engine ran adapters/postgresql only) |
| Hibernate (hibernate-core subset) | 7.4.14 | 3088 / 0 | | 3080 / 8 | 8: see below |
| TypeORM (postgres) | 1.1.2 | 3059 / 11 | | 2977 / 56 | 17, and 28 setup hooks: see below |
| Drizzle (pg integration) | 0.45.4 | 852 / 0 | | 849 / 3 | 3: INSERT ... SELECT parameter typing |
| ruby-pg (spec/, helper patched to a running server) | 1.7.0 | 1076 / 26 | | 1072 / 30; round 19: 1076 / 26 | 0 (rows stream since round 19, agent 65) |

## Differences left, and why

Contrib types (agent 39): hstore, ltree, lquery, ltxtquery and cube are sent as text when a
client asks for binary (the RowDescription says format 1; psycopg 3, asyncpg, pgx, Npgsql,
tokio-postgres, sqlx), and IS [NOT] DISTINCT FROM / NULLIF don't use their `=` (citext compares
case-sensitively there: sqlx); file_fdw (Django), postgres_fdw (ActiveRecord); btree_gist has no
varchar opclass (TypeORM's setup hooks); information_schema.columns.udt_schema of an extension
type is pg_catalog (TypeORM).

Locking and timeouts (agent 34): statement_timeout and pg_cancel_backend don't end a row-lock
wait, SET LOCAL lock_timeout doesn't stop an UPDATE waiting on a row (ActiveRecord, Hibernate),
idle_in_transaction_session_timeout isn't enforced, serializable write skew in ActiveRecord's
two cases, FOR UPDATE on a LEFT JOIN's nullable side doesn't error (TypeORM).

Engine, open:
- pgjdbc TypeCacheDLLStressTest.createDropTableAndGetTypeInfo (round 19): a debug build's
  assertion on pg_class's row count (`catalog/system.rs`) fails while another session creates
  and drops tables; the same before round 19's streaming. Release builds don't check it.
- DML on system catalogs: `UPDATE pg_description SET objoid = ...` (pgjdbc
  DatabaseMetaDataTest's setUp moves a function's comment onto a table, classoid kept: the
  engine's comments have no classoid of their own), `UPDATE pg_class SET relname` (pgjdbc
  testOidUpdatable). pg_class's statistics columns, reloptions and pg_depend are writable.
- sqlx: a parameter declared as a domain-array oid is described by its base array type
  (domain_type_array_1..3), empty binary arrays aren't normalized (1-D with no elements:
  i32_array_empty, i32_vec; probably Hibernate's ArrayConstructorTest too).
- ActiveRecord: PL/pgSQL EXECUTE of a string of several statements; ALTER TABLE ... DISABLE /
  ENABLE TRIGGER ALL is 3-5 ms a table (fixture loading); schema introspection 7-20x slower.
- Hibernate: unnest() of an array of a one-field composite returns the row, not the field
  (ArrayUnnestStructTest); "column #32 out of range" in a WITH ... AS MATERIALIZED with an
  UPDATE CTE (BulkManipulationTest.testUpdateWithWhereExistsSubquery).
- Drizzle: `INSERT INTO t (a, b) SELECT id, $1 FROM u WHERE ... ORDER BY ...` types $1 text.
- TypeORM: enum change / rename (loadTables' regtype cast over a renamed enum), table names
  with quotes, collation change SQL, extension installation.
- Django (round 19, agent 66): file_fdw is native now (contrib STATUS.md), but inspectdb's
  test_foreign_data_wrapper reads `SELECT * ... LIMIT 1` from a `program 'echo 1,2,3,4'`
  table, which runs a server program: refused, as COPY ... PROGRAM is (HANDOFF
  "Decisions"); it passes only if programs were allowed. aggregation's
  test_aggregation_subquery_annotation_multivalued fails with "subquery uses ungrouped
  column" (new since round 18's runs, not from file_fdw; not looked at).
- tokio-postgres: no NOTICE at startup for a truncated application_name; pg_database's row
  order.
- asyncpg's introspection tests: its type introspection query on a large catalog is slower
  than the test's 250 ms bound. node-postgres 1105 is timing (a debug build).
- sqlx it_can_inspect_constraint_errors, it_can_prepare_then_execute,
  test_describe_outer_join_nullable: not looked at yet.

Fixed in round 19, agent 65 (wire tests `rows_stream_as_the_executor_makes_them`,
`cancel_while_rows_stream`; session test `query_each_streams_rows_to_a_callback`): a SELECT's
rows and COPY TO's data go to the client as the executor makes them (`session/sink.rs`,
`server.rs` `WireSink`). The first row arrives while the statement runs, notices come in order
with the rows, the rows made before an error go ahead of it, and memory stays bounded for any
result. ruby-pg now matches PostgreSQL (its single-row-mode and COPY TO tests pass).
Six suites ran again against the round 19 engine (debug build) and a private reference:
- ruby-pg: 0 differences.
- psycopg 3: the 9 hstore ones. Its two `test_copy_out_error_with_copy_not_finished` tests
  found that a cancel didn't stop data streaming from a node that doesn't check for
  interrupts; each streamed row now checks.
- asyncpg: the 4 above, plus `test_executemany_server_failure_during_writes`, which is
  timing-dependent. On this loaded machine it fails two runs in three against both the old
  and the new engine: whether the error reaches the client before it has written all 128
  rows.
- pgx: the 4 hstore ones.
- node-postgres: 1105, timing.
- pgjdbc: the 3 above, plus TypeCacheDLLStressTest (see "Engine, open").

Fixed in round 19, agent 66: databases in other encodings than UTF8 (Npgsql
Non_UTF8_Encoding: a SQL_ASCII database written in WIN1252 and read back, then a UTF8 client's
"invalid byte sequence"): CREATE DATABASE ... ENCODING, server_encoding, client_encoding
conversions as PostgreSQL's (HANDOFF "Server encodings"); `tests/locale/encodings.sql`.
With agent 53's extension types on the wire, Npgsql's suite now fails only where PostgreSQL's
does (Serialization and one more). Cancel_while_reading_from_long_running_query failed in one
of three engine runs before agent 65's streamed rows (it reads rows while a later row's
pg_sleep(100) runs).

Fixed in round 18, agent 51 (each with a golden, `a51_exists_param_join` / `a51_client_catalog`,
or a wire test, `notices_copy_lines_and_portal_rows_as_they_come` /
`described_types_startup_options_and_copy_not_null`):
- Django's three "outer reference without an outer row" queries (and a wrong result, queries
  test_combining_does_not_mutate): EXISTS over a join its own planning had made a nested loop
  over a parameterized index path was pulled up with the path's parameter left in the inner
  scan. Since 530923f (it took any nested loop with parameters for a LATERAL join; round 17
  agent 38 saw it from the rebase onto 6dd2469, which came after it). Such a join is now taken
  back to the one written (`sublink::unparameterized`); plans are Postgres's.
- Notices go to the client as they are raised (PL/pgSQL RAISE in a long statement too).
- COPY FROM STDIN reads lines / tuples as they come: a bad line or a NULL in a NOT NULL column
  fails the COPY while the client is still sending (pgx).
- A portal's SELECT runs only as far as each Execute's row count (and MOVE / FETCH on it).
- ParameterDescription: an INSERT a DO INSTEAD NOTHING rule drops (lib/pq TestIssue186), a
  parameter assigned to a domain column (tokio-postgres domain); RowDescription: EXPLAIN
  (FORMAT JSON / XML), NULL::anyarray; EXPLAIN EXECUTE of a prepared utility statement (sqlx).
- Startup: explicit parameters win over `options`' -c (lib/pq TestRuntimeParameters).
- Catalog: makeArrayTypeName's `_name_1` (pgjdbc), pg_table_is_visible of information_schema
  and pg_type_is_visible of row types (SQLAlchemy), pg_get_keywords (pgjdbc / Hibernate),
  pg_get_serial_sequence of a (materialized) view's column (SQLAlchemy), information_schema's
  bit lengths (Prisma), a default naming the same CREATE TABLE's serial sequence and
  ARRAY[...]::oidvector / int2vector (ActiveRecord, tokio-postgres).
- Limits: "target lists can have at most 1664 entries", ROW expressions (Npgsql); the largest
  numerics in binary (pgx TestNumericCodec panicked).

## Not run

- ActiveRecord's full suite against the engine (about 6 hours: fixture loading, see above);
  only its PostgreSQL adapter tests ran there.
