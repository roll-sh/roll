# Client libraries against the engine

The engine's wire protocol, catalogs and SQL as real drivers and ORMs see them: each client's
own test suite, run once against the engine and once against PostgreSQL 18.6 (the same image as
`roll-engine-ref`, default settings), and compared test by test. Only differences count: a test
that fails on PostgreSQL too (no SSL, no replication, no max_prepared_transactions, a missing
contrib module in the image) is the environment's.

## Running

```sh
# PostgreSQL 18.6 on 127.0.0.1:55838 (see run.sh's header), the engine on 127.0.0.1:55839:
roll-engine --data /some/dir --listen 127.0.0.1:55839 --password pw --no-sync --allow-server-programs
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
there, so fewer of its tests ran than in round 17). "Round 19" counts are round 19, agent 66's
runs (debug build, a private reference; both sides run again), Hibernate's over all of
hibernate-core (its failures on PostgreSQL there are the environment's).

| suite | version | PostgreSQL | engine, round 17 | engine, round 18 | differences left |
|---|---|---:|---:|---:|---|
| psycopg 3 (tests/, binary impl) | 3.2.9 | 4924 / 9 | 4907 / 26 | 4915 / 18 | 9: hstore binary round trips |
| psycopg2 (tests/) | 2.9.10 | 711 / 2 | 710 / 3 | 711 / 2 | 0 |
| asyncpg (tests/) | 0.30.0 | 262 / 5 | 253 / 14 | 258 / 9 * | 4: hstore codecs (3), introspection timing |
| pgx (root, pgconn, pgtype, pgxpool, stdlib) | 5.7.5 | 1197 / 4 | 1207 / 7 | 1213 / 7 * | 4: hstore binary codec |
| lib/pq | 1.10.9 | 232 / 0 | 230 / 2 | 232 / 0 * | 0 |
| node-postgres (pg integration) | 8.16.0 | 146 / 3 | 146 / 3 | 145 / 4 * | 1: gh-issues/1105 (5000 inserts in 5 s, a debug build) |
| postgres.js | 3.4.7 | 194 / 62 | 194 / 62 | 194 / 62 | 0 |
| pgjdbc (jdbc2/3/4/42, core) | 42.7.7 | 8221 / 0 | 8214 / 7 | 8218 / 3 *; round 19: 8221 / 0 | 0 (catalog DML, LIKE ending in its escape: round 19, agent 66) |
| Npgsql (Npgsql.Tests) | 9.0.3 | 1914 / 2 | 1908 / 16 | 1917 / 2 (round 19, agents 53 and 66) | 0 |
| SQLAlchemy (dialect/postgresql, sql) | 2.0.41 | 15801 / 2 | 15755 / 48 | 15801 / 2 * | 0 |
| Django (26 apps) | 5.2.4 | 3793 / 15 | 3789 / 19 | 3792 / 16; round 19: 3792 / 16 | 0 (a file_fdw `program` table: round 19, agent 81) |
| tokio-postgres (tests, postgres, derive) | 0.7.18 | 221 / 0 | | 212 / 9 *; round 19: 221 / 0 | 0 |
| sqlx (9 postgres targets) | 0.9.0 | 333 / 0 | | 318 / 15 *; round 19: 333 / 0 | 0 |
| Prisma (push, pull, migrate, client) | 7.10.1 | 47 / 0 | | 47 / 0 * | 0 (bit(n) lengths in `db pull`, fixed) |
| ActiveRecord (Rails, PostgreSQL adapter tests) | 8.1.4 | 10159 / 1; adapters: 776 / 1 | | 748 / 28; round 19: 769 / 8 (its 2 referential-integrity failures pass since, rerun) | 5: postgres_fdw's loopback (4), a cancel (see below) |
| Hibernate (hibernate-core subset; round 19: all of hibernate-core) | 7.4.14 | 3088 / 0; round 19: 12774 / 858 | | 3080 / 8; round 19: 12772 / 860 | 2: see below |
| TypeORM (postgres) | 1.1.2 | 3059 / 11 | | 2977 / 56; round 19: 3058 / 12 | 1: xml2 (custom extension installation) |
| Drizzle (pg integration) | 0.45.4 | 852 / 0 | | 849 / 3; round 19: 852 / 0 | 0 |
| ruby-pg (spec/, helper patched to a running server) | 1.7.0 | 1076 / 26 | | 1072 / 30; round 19: 1076 / 26 | 0 (rows stream since round 19, agent 65) |

## Differences left, and why

Contrib types (agent 39; binary send / receive and IS DISTINCT FROM since round 19, agent 53):
postgres_fdw (ActiveRecord; native since round 19, agent 59); xml2 isn't implemented (TypeORM's
custom extension installation test installs tablefunc and xml2).

Locking and timeouts (agent 34): statement_timeout and pg_cancel_backend don't end a row-lock
wait, SET LOCAL lock_timeout doesn't stop an UPDATE waiting on a row (ActiveRecord, Hibernate),
idle_in_transaction_session_timeout isn't enforced, serializable write skew in ActiveRecord's
two cases.

Engine, open:
- Hibernate BulkManipulationTest.testUpdateWithWhereExistsSubquery: "column #32 out of range"
  in `WITH id_cte AS MATERIALIZED (SELECT ... JOIN ... JOIN ... WHERE EXISTS (SELECT ... JOIN
  ...)), update_cte AS (UPDATE ... WHERE id IN (SELECT id FROM id_cte)) SELECT count(*) FROM
  id_cte`: a parameterized nested loop's parameters index a wider row than its outer side
  makes (10 columns, Human's). It fails only when the test runs (pgjdbc, after the class's
  other tests); the same statements replayed through psql or the wire after the same DDL and
  rows plan otherwise and pass. A planner bug (join search), left to the planner work.
- Hibernate PackagedEntityManagerTest.testOverriddenPar: `select nextval('Bug_SEQ')` finds no
  bug_seq (the persistence unit's schema export); not looked at.
- ActiveRecord ForeignTableTest (4): postgres_fdw's server has only `dbname` (a loopback
  through libpq's default socket) and its user mapping no password. The reference image trusts
  local socket connections; the engine, started with `--password`, asks for one ("fe_sendauth:
  no password supplied"), with `--allow-outbound local` too. An engine started with trust
  authentication (or a mapping with a password) would pass; how the engine should treat its
  own loopback connections is an authentication policy question (agent 59's postgres_fdw).
- ActiveRecord PostgresqlTransactionTest#test_raises_QueryCanceled_when_canceling_statement_
  due_to_user_request: a cancel while the statement waits on a row lock (agent 56's item).
- ActiveRecord: ALTER TABLE ... DISABLE / ENABLE TRIGGER ALL is about 1.4 ms a table in a debug
  build (PostgreSQL: 0.1 ms; fixture loading); schema introspection is slower.
- asyncpg's introspection tests: its type introspection query on a large catalog is slower
  than the test's 250 ms bound. node-postgres 1105 is timing (a debug build).

Fixed in round 19, agent 81, client suites round 4: server programs. `roll-engine
--allow-server-programs` (`EngineOptions::server_programs`, off by default; HANDOFF
"Decisions") runs COPY ... FROM / TO PROGRAM and file_fdw's `program` as PostgreSQL does,
through the shell, with its errors for a failing program (`src/server_programs.rs`). The
harness's engine runs with it: Django's inspectdb test_foreign_data_wrapper (`SELECT * ...
LIMIT 1` from a `program 'echo 1,2,3,4'` table) passes (inspectdb 27 / 0 on both).

Fixed in round 19, agent 66, client suites round 3 (golden `a66_client_round3`, wire test
`startup_notice_empty_arrays_and_catalog_dml`; file_fdw and server encodings are below and in
HANDOFF):
- DML on pg_description by a superuser (pgjdbc DatabaseMetaDataTest's setUp moves a function's
  comment onto a table): UPDATE / INSERT / DELETE, its unique index and NOT NULL errors, over
  the extended protocol and PREPARE too. A comment now has a classoid of its own when DML puts
  it under another catalog than its object's (`Catalog::class_comments`, a new header field:
  stored catalogs load as before). `UPDATE pg_class SET relname` to the name it has (pgjdbc's
  updatable result set writes every column back); a real rename stays refused.
- tokio-postgres: GUC_IS_NAME values (application_name, ...) are truncated to 63 bytes with
  their NOTICE, at startup too; pg_database's rows in Postgres's heap order (postgres,
  template1, template0, then the databases made since).
- sqlx: an empty array received in binary has no dimensions (array_recv); a parameter Parse
  declares as a domain array keeps that type as a column (`$2` of `positive_int[]`), in
  custom plans too; a check constraint's error names its routine ExecConstraints (the routine
  of a known message followed by a name is found).
- Drizzle: `INSERT ... SELECT id, $1 FROM u ... ORDER BY / LIMIT` types `$1` as its column.
- Hibernate: unnest() of an array of a one-field composite in FROM gives the field.
- TypeORM: btree_gist's gist_text_ops is varchar's default class (the binary-coercible default
  class, a preferred type's winning a tie); information_schema.columns gives an extension type
  as USER-DEFINED in its schema, a renamed enum by its name now, a row type's schema
  (pg_statistic_ext_data.stxdexpr), and a column's collation; a FROM subquery's own FOR UPDATE
  over its outer join's nullable side errors.
- ActiveRecord's check_all_foreign_keys_valid! (PostgreSQLReferentialIntegrityTest, 2): a DO
  block's EXECUTE of `UPDATE pg_catalog.pg_constraint SET convalidated = false ...; ALTER TABLE
  ... VALIDATE CONSTRAINT ...`: a superuser's UPDATE of pg_constraint.convalidated, in
  PL/pgSQL too.
- Django: a GROUP BY key that is a sub-select over a join, named by its ordinal (two bindings of
  the sub-select differ in their joins' `bound` ids only).
- pgjdbc: `relname LIKE 'a\'` (a pattern ending in its escape character) fails only when
  matching reaches it, and a pg_class scan reads only the names the pattern's fixed prefix
  selects, as the index scan EXPLAIN shows does; a debug build's pg_class row-count check
  skips a catalog another session's DDL replaced meanwhile (TypeCacheDLLStressTest).

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
