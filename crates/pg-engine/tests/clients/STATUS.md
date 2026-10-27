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
| psycopg 3 (tests/, binary impl) | 3.2.9 | 4924 / 9 | 4907 / 26 | 4915 / 18; round 19 (agent 81): 4924 / 9 | 0 |
| psycopg2 (tests/) | 2.9.10 | 711 / 2 | 710 / 3 | 711 / 2 | 0 |
| asyncpg (tests/) | 0.30.0 | 262 / 5 | 253 / 14 | 258 / 9 *; round 19 (agent 81): 259 / 8 | 2: introspection timing (a debug build), executemany timing (see below) |
| pgx (root, pgconn, pgtype, pgxpool, stdlib) | 5.7.5 | 1197 / 4 | 1207 / 7 | 1213 / 7 *; round 19 (agent 81): 1217 / 3 | 0 |
| lib/pq | 1.10.9 | 232 / 0 | 230 / 2 | 232 / 0 * | 0 |
| node-postgres (pg integration) | 8.16.0 | 146 / 3 | 146 / 3 | 145 / 4 *; round 19 (agent 81): 145 / 4 | 1: gh-issues/1105 (5000 inserts in 5 s, a debug build) |
| postgres.js | 3.4.7 | 194 / 62 | 194 / 62 | 194 / 62 | 0 |
| pgjdbc (jdbc2/3/4/42, core) | 42.7.7 | 8221 / 0 | 8214 / 7 | 8218 / 3 *; round 19: 8221 / 0 | 0 (catalog DML, LIKE ending in its escape: round 19, agent 66) |
| Npgsql (Npgsql.Tests) | 9.0.3 | 1914 / 2 | 1908 / 16 | 1917 / 2 (round 19, agents 53 and 66) | 0 |
| SQLAlchemy (dialect/postgresql, sql) | 2.0.41 | 15801 / 2 | 15755 / 48 | 15801 / 2 * | 0 |
| Django (26 apps) | 5.2.4 | 3793 / 15 | 3789 / 19 | 3792 / 16; round 19: 3792 / 16 | 0 (a file_fdw `program` table: round 19, agent 81) |
| tokio-postgres (tests, postgres, derive) | 0.7.18 | 221 / 0 | | 212 / 9 *; round 19: 221 / 0 | 0 |
| sqlx (9 postgres targets) | 0.9.0 | 333 / 0 | | 318 / 15 *; round 19: 333 / 0 | 0 |
| Prisma (push, pull, migrate, client) | 7.10.1 | 47 / 0 | | 47 / 0 * | 0 (bit(n) lengths in `db pull`, fixed) |
| ActiveRecord (Rails, PostgreSQL adapter tests) | 8.1.4 | 10159 / 1; adapters: 776 / 1 | | 748 / 28; round 19: 769 / 8; agent 81: 771 / 6 | 5: postgres_fdw's loopback (4), a cancel (see below) |
| Hibernate (hibernate-core subset; round 19: all of hibernate-core) | 7.4.14 | 3088 / 0; round 19: 12774 / 858 | | 3080 / 8; round 19: 12772 / 860 | 2: see below |
| TypeORM (postgres) | 1.1.2 | 3059 / 11 | | 2977 / 56; round 19: 3058 / 12 | 1: xml2 (custom extension installation) |
| Drizzle (pg integration) | 0.45.4 | 852 / 0 | | 849 / 3; round 19: 852 / 0 | 0 |
| ruby-pg (spec/, helper patched to a running server) | 1.7.0 | 1076 / 26 | | 1072 / 30; round 19: 1076 / 26 (agent 81 too) | 0 (rows stream since round 19, agent 65) |

Round 4 (round 19, agent 81) added these suites. Counts are before agent 81's fixes (the
engine of 29a81105, a debug build); "engine-only" lists the causes, all fixed below unless
marked open.

| suite | version | PostgreSQL | engine | engine-only |
|---|---|---:|---:|---|
| GORM (tests/, simple and extended protocol) | 1.31.2, driver/postgres 1.6.3 | 31988 / 2 | 31988 / 2 | 0 |
| sqlc (Postgres examples, managed-db replay) | 1.31.1 | 1628 / 130 | 1619 / 139 | 9: keys before columns, sequence attnotnull, ON CONFLICT at Parse |
| Diesel (CI's Postgres job) | 2.3.14 | 1009 / 19 | 1005 / 24 | 5: startup name truncation, DROP DATABASE wait, read-only row locks |
| SeaORM (tests, migration) | 2.0.4 | 238 / 0 | 238 / 0 | 0 |
| Postgrex | 0.22.4 | 398 / 6 | 392 / 12 | 6: DML portal re-run, `bit '101'`, binary timestamptz range, 128 MB bytea, 2 timing |
| Ecto SQL (pg integration) | 3.14.0 | 480 / 15 | 478 / 17 | 2: PREPARE ... AS parameters, DISTINCT ON a sub-select |
| Sequelize (unit, integration) | 6.37.8 | 3202 / 26 | 3202 / 26 | 0 |
| Knex (integration, pg) | 3.3.0 | 523 / 0 | 523 / 0 | 0 |
| Kysely (node tests, postgres) | 0.29.7 | 745 / 0 | 734 / 3 | 3 (11 not run): concurrent CREATE TABLE OIDs |
| MikroORM (postgresql files) | 7.2.5 | 1414 / 67 | 1400 / 70 | 3: keys before columns, FK check cost, GROUP BY + LIMIT order (open) |
| Laravel (Integration/Database, pgsql) | 13.38.0 | 886 / 5 | 886 / 5 | 0 |
| Doctrine DBAL (pdo_pgsql) | 4.5.0 | 3558 / 4 | 3557 / 5 | 1: superuser DML renumbering pg_class.oid (open) |
| Spring JDBC / Flyway / Liquibase (driver program) | Boot 4.1.2, Flyway 13.11.1, Liquibase 5.0.4 | 271 / 0 | 226 / 2 | 2 (+43 ids): pg_sequence_last_value, oidvectortypes |
| jOOQ (codegen + DSL program) | 3.21.9 | 141 / 0 | 140 / 0 (11 ids) | domain is_nullable, _pg_truetypid, int2vector::int[] |
| r2dbc-postgresql | 1.1.3 | 1251 / 21 | 1251 / 21 | 0 |
| EF Core (EFCore.PG.FunctionalTests) | efcore.pg 10.0.3 | 27410 / 0 | 23851 / 3559 | N'...' literals (2490), oidvectortypes (1066), a LATERAL "outer reference" (4, open) |
| Dapper | 2.1.89 | 153 / 170 | 153 / 170 | 0 |
| Alembic | 1.20.1 | 2152 / 1 | 2144 / 9 | 8: jsonb subscripts deparsed as #> |
| SQLModel + Alembic autogenerate (driver program) | 0.0.48, asyncpg 0.32.0 | 74 / 0 | 74 / 0 | 0 |
| psycopg_pool (tests/pool, slow ones too) | 3.2.9 | 311 / 0 | 311 / 0 | 0 |
| pgAdmin 4 / DBeaver metadata queries (replayed) | 9.18 / 26.2.3 | 856 ids | 814 same | catalog values (proargdefaults, relhasrules, FK operator arrays, ...) |

## Differences left, and why

Contrib types (agent 39; binary send / receive and IS DISTINCT FROM since round 19, agent 53):
postgres_fdw (ActiveRecord; native since round 19, agent 59); xml2 isn't implemented (TypeORM's
custom extension installation test installs tablefunc and xml2).

Locking and timeouts (agent 34): statement_timeout and pg_cancel_backend don't end a row-lock
wait, SET LOCAL lock_timeout doesn't stop an UPDATE waiting on a row (ActiveRecord, Hibernate),
idle_in_transaction_session_timeout isn't enforced, serializable write skew in ActiveRecord's
two cases. (Round 20, agent 73: the lock waits end over the simple and the extended
protocol since round 17's lock manager, now checked by `tests/wire_waits.rs` for every kind
of wait against PostgreSQL's transcript; idle_in_transaction_session_timeout,
transaction_timeout and idle_session_timeout are enforced; a safe-snapshot wait's
statement_timeout and extended-protocol cancel, and a cancel arriving while idle, are fixed.
psycopg 3's, asyncpg's and pgx's cancel, timeout, interrupt, deadlock and notification tests
(`-k` / `-run` on those words: 42, 31 and 110 tests) pass on both servers, no difference.)

Engine, open:
- (Fixed in round 21, agent 78: Hibernate BulkManipulationTest.testUpdateWithWhereExistsSubquery,
  "column #32 out of range". The plan, not the protocol: with the class's rows (three
  Humans, no ANALYZE) psql gets it too. The EXISTS is a semi join of `Human` alone with its
  sub-select, the other two tables joined above it, and their index scans took the class
  member `Human_friends.human1` from the semi join's right side, which the engine's semi join
  doesn't return (`joinpath.rs` `semi_hidden`). The class's 54 tests pass; wire test
  `semi_join_right_side_not_a_parameter`, golden `a78_semi_param`.)
- Hibernate PackagedEntityManagerTest.testOverriddenPar: `select nextval('Bug_SEQ')` finds no
  bug_seq (the persistence unit's schema export); not looked at.
- ActiveRecord ForeignTableTest (4): postgres_fdw's server has only `dbname` (a loopback
  through libpq's default socket) and its user mapping no password. The reference image trusts
  local socket connections; the engine, started with `--password`, asks for one ("fe_sendauth:
  no password supplied"), with `--allow-outbound local` too. Since round 20 (agent 80) the
  engine takes pg_hba.conf rules (`--hba-file`, `--hba`), and its connections to itself
  through libpq's default socket are `local` ones: started with `--hba "local all all
  trust" --hba "host all all all scram-sha-256"` (the reference image's policy) it trusts
  them as the reference does; postgres_fdw's password_required rule for non-superusers
  stays.
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

Fixed in round 19, agent 81, second batch (golden `a81_client_round4`):
- Literal types: `N'VINET'`, `char 'x'`, `bit '101'`, `'x'::pg_catalog.bpchar` have no length
  (ConstCharacter / ConstBit); they were char(1) / bit(1), cutting values to one character
  (EF Core's Northwind, 2,490 tests; Postgrex). The grammar gives `char` / `bit` in a column
  or cast their length 1 itself.
- CREATE TABLE's table constraints are resolved once every column is known (MikroORM, sqlc:
  `PRIMARY KEY (a, b)` before `a`), in their place among the columns' own.
- A range over an extension type with a default btree class (citext; asyncpg), and ADD
  PRIMARY KEY USING INDEX on such a column: the catalog's default class, not only built-in
  ones; pg_range.rngsubopc shows it.
- information_schema.columns.is_nullable is NO for a NOT NULL domain's column (jOOQ);
  information_schema._pg_truetypid / _pg_truetypmod (pg_attribute, pg_type);
  int2vector::int[] (element-wise, jOOQ's indoption); pg_sequence_last_value (Liquibase);
  oidvectortypes (Flyway's clean, EF Core's database cleaner: 1,066 tests); sequence
  functions on another relation give PG 18's "cannot open relation" with
  errdetail_relkind_not_supported (pg_sequence_parameters "cache lookup failed",
  pg_get_sequence_data NULLs).
- SELECT DISTINCT ON a sub-select matches the same sub-select in ORDER BY (Ecto); SELECT FOR
  UPDATE / SHARE in a read-only transaction fails as ExecCheckXactReadOnly does (Diesel); ON
  CONFLICT's arbiter inference waits for planning, so PREPARE / Parse of an INSERT with no
  matching constraint succeeds and EXECUTE fails (sqlc), its result columns still described.
- asyncpg's introspection: the introspection query takes 98 ms in a release build (the
  test's bound is 250 ms); in a debug build 1.2 s, mostly `Catalog::rel_rowtype`'s O(n)
  debug_assert while pg_type's rows are made for 1,000 tables. Its recursive term now plans
  as PostgreSQL's: `ti.oid = any(tt.attrtypoids)` is an index (or BitmapOr arm) condition
  parameterized by the outer array, estimated for 10 elements (scalararraysel), and a strict
  operator's `x = ANY (a)` implies `a IS NOT NULL` (the arms' filters go).

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
