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

## Where it stands

Pass / fail per suite (tests that ran; skips aside). "Before" is the engine at the start of this
work (round 17, agent 38), "after" this batch.

| suite | version | PostgreSQL | engine before | engine after | differences left |
|---|---|---:|---:|---:|---|
| psycopg 3 (tests/, binary impl) | 3.2.9 | 4924 / 9 | 4515 / 418 | 4907 / 26 | 17: hstore |
| psycopg2 (tests/) | 2.9.10 | 711 / 2 | 704 / 9 | 710 / 3 | 1: LOCK TABLE deadlock |
| asyncpg (tests/) | 0.30.0 | 262 / 5 | 242 / 25 | 253 / 14 | 9: hstore, citext, isn, introspection timing |
| pgx (root, pgconn, pgtype, pgxpool, stdlib) | 5.7.5 | 1216 / 4 | 1135 / 79 | 1207 / 7 | 4: ltree, COPY FROM abort, SSI |
| lib/pq | 1.10.9 | 232 / 0 | 227 / 5 | 230 / 2 | 2: hstore, rule-rewritten Describe (fixed after the run) |
| node-postgres (pg integration) | 8.16.0 | 146 / 3 | 145 / 4 | 146 / 3 | 0 |
| postgres.js | 3.4.7 | 194 / 62 | 189 / 67 | 194 / 62 | 0 |
| pgjdbc (jdbc2/3/4/42, core) | 42.7.7 | 8221 / 0 | 6623 / 1598 | 8214 / 7 | 7: see below |
| Npgsql (Npgsql.Tests) | 9.0.3 | 1915 / 2 | 1905 / 19 | 1908 / 16 | 14: hstore, ltree, citext schema, SQL_ASCII database, see below |
| SQLAlchemy (dialect/postgresql, sql) | 2.0.41 | 15801 / 2 | 1729 / 240 | 15755 / 48 | 46: hstore (32), reflection, see below |
| Django (26 apps) | 5.2.4 | 3793 / 15 | 3776 / 32 | 3789 / 19 | 4: file_fdw, 3 "outer reference without an outer row" |

## Differences left, and why

Contrib extensions (agent 39): hstore (psycopg 3, asyncpg, lib/pq, Npgsql, SQLAlchemy), ltree
(pgx, Npgsql), isn (asyncpg), file_fdw (Django). citext and pgcrypto came with the rebase onto
agent 39's work (Django's SHA functions pass now).

Locking and isolation (agent 34): LOCK TABLE ... ACCESS EXCLUSIVE in two sessions doesn't
block or deadlock (psycopg2 test_deadlock, pgjdbc closeInProgressStatement, which waits on a
lock); serializable write skew isn't detected at commit (pgx TestTxCommitSerializationFailure).

Engine, open:
- Notices go out at the end of the statement, not as they're raised (Postgres flushes each
  one): pgjdbc closeInProgressStatement waits for the first. Needs the session to reach the
  connection's output buffer mid-statement (server.rs; agent 36 works on that path).
- COPY FROM STDIN is processed at CopyDone: a bad first row isn't reported until the client
  has sent everything (pgx TestConnCopyFromFailServerSideMidwayAbortsWithoutWaiting).
- A portal runs to completion at its first Execute; one whose later rows fail errors at once
  instead of after its first max_rows rows.
- DML on system catalogs (`UPDATE pg_description`, `pg_class`) is refused (pgjdbc
  DatabaseMetaDataTest's setUp, testOidUpdatable).
- pgjdbc BlobTransactionTest.concurrentReplace: a statement it sends fails with
  `syntax error at or near "$"` (not looked at yet).
- An array type's name when `_name` is taken (pgjdbc getCorrectSQLTypeForShadowedTypes).
- CREATE DATABASE ... ENCODING 'SQL_ASCII' (Npgsql Non_UTF8_Encoding).
- Django: three queries with correlated EXISTS fail with the internal error "outer reference
  without an outer row" since the rebase onto 6dd2469 (they passed before it; not reproduced
  in isolation yet: planner work, round 17).
- SQLAlchemy reflection: views list information_schema's views, materialized view and domain
  reflection details, `test_domain_type_reflection` (collation 'C' of a domain).
- asyncpg's introspection tests: its type introspection query on a large catalog is slower
  than the test's 250 ms bound.

Fixed in this round (round 17, agent 38): see the commit message and HANDOFF; each has a
test in `tests/wire.rs` or `tests/sql/client_compat.sql`.

## Not run

- ruby-pg 1.5.9: its specs start their own server with `initdb` / `pg_ctl` (no option to use a
  running one).
- tokio-postgres / sqlx: tokio-postgres is the client of `tests/wire.rs`; its own suite needs
  per-user pg_hba methods and SSL on port 5433 (not set up yet). sqlx's needs its CLI and
  migrations.
- Prisma, ActiveRecord, Hibernate, TypeORM / Drizzle: not set up yet (see HANDOFF).
