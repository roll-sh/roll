# pg-engine handoff

Everything a new agent needs to continue the PostgreSQL 18.6 parity work on another machine.
Read this file, `../../AGENTS.md`, and `tests/regress/STATUS.md` before changing anything.

## Goal

`pg-engine` is roll's in-process PostgreSQL engine. The goal is **1:1 behaviour with
PostgreSQL 18.6**: the same rows, row order, errors (SQLSTATE, message, DETAIL, HINT,
CONTEXT, position), notices, catalog contents and `EXPLAIN` text, byte for byte.

The measure is PostgreSQL's own regression suite (`src/test/regress` at `REL_18_6`, 230 files,
51,150 statements), run through the engine and through a reference PostgreSQL 18.6 and
compared statement by statement.

**Compatible outside, free inside.** A client must be able to use the engine like any other
PostgreSQL 18.6: drivers, ORMs, psql, pg_dump, the wire protocol, concurrency and locking,
extensions. What a client can observe has to match. That covers the items above, plus
transaction and lock behaviour, wire messages, and catalog contents. How the engine gets
there does not. Storage, executor, caches and data structures may differ from PostgreSQL's
wherever that makes the engine faster or leaner. EXPLAIN still prints PostgreSQL's plan,
because clients and the suite read it, but an executor node may compute its rows any way it
likes as long as the rows, their order, errors and side effects are PostgreSQL's.
Performance is a goal of its own: measure it (`tools/bench`, BENCH.md) against the reference
and make the engine faster than PostgreSQL wherever possible.

The regression suite is one measure, not the whole one. Round 16 found locking and
wrong-result bugs that no statement in it catches. Further measures, each with its own
harness and score in `tests/`:
- the isolation suite (`src/test/isolation`);
- differential random queries against the reference;
- pg_dump / restore round-trips;
- driver and ORM test suites;
- contrib extensions.

## Where it stands

As of commit `e890fb0` (2026-10-21), plus server-side files (below) and rounds 13–14 (STATUS.md):

- **Regression suite: 51,135 of 51,150 statements match (99.97%).** 15 differ. The full
  classified list, per file and per unit, is in `tests/regress/STATUS.md`.
  - All 15 are `EXPLAIN` plan text. Query results already match (sqljson_jsontable's join
    order since round 16, agent 17: join relations parameterized by LATERAL references).
  - None differs by design any more (aggregates `balk` matches since round 16, agent 14: a
    Finalize Aggregate runs the user-defined combine functions).
- No engine crashes or panics in a full run. A full run of 286306f measured 50,792: STATUS's
  per-file counts hold except that run's artifacts (see STATUS.md). The "stalls" in full runs
  (`ALTER TABLE tt7 DROP q` in alter_table, fast_default 177–179) were no hang: every ALTER TABLE
  copied the whole catalog once per table (`column_using`), seconds per statement over a full
  run's catalog, so those files ran past the per-file timeout. Fixed in round 15;
  `tests/ddl_scaling.rs` counts the catalog copies DDL makes.
  The domain / oidjoins timeouts of round 16's full runs were the same kind: a catalog scan
  per outer row of a nested loop regenerated the whole catalog; a statement now generates a
  catalog once and looks rows up by the equality (`exec/nodes.rs` `sys_rescan`, round 16
  agent 15), and `ddl_scaling.rs` also counts the catalogs a query generates. A full run of
  that commit measured the table's 51,053 with no timeout.
- **contrib extensions** (round 17, agent 39): CREATE / ALTER / DROP EXTENSION run the
  extensions' own PostgreSQL 18.6 scripts (`src/ext/`, `session/ddl/extension.rs`), C
  functions are native code by library and symbol; the contrib modules' regression tests run
  with `pg_contrib` (`tests/pg_contrib.rs`). Per-module scores and how it works:
  `tests/contrib/STATUS.md`. uuid-ossp, citext and pgcrypto match their contrib suites fully;
  extension types the engine implements natively are `Value::Ext` (`src/ext/types.rs`).
- **Performance** (round 17, agent 36): measured for the first time, `tools/bench/` against a
  pinned PostgreSQL 18.6 on the same 4 cores; numbers, method and the optimisations made are
  in `BENCH.md`. Over the wire the engine beats PostgreSQL on single-client OLTP (point
  selects, inserts, updates, upserts, short transactions: 1.5–2x), pgbench select-only at every
  client count and TPC-B at scale 1, and is about even on TPC-B at scale 10; it is slower on
  multi-client OLTP that reads many rows (range scans, secondary lookups), on analytic queries
  and TPC-H (5–20x: a row-at-a-time executor over decoded `Value` rows, no parallel query),
  bulk loads and DDL. In-process (how roll embeds it) OLTP is 2–27x PostgreSQL's over the wire.
  TPC-H SF1 takes 2.4x less disk. BENCH.md's "Where it is still slower" is the to-do list.
- The full crate test suite (`cargo test -p pg-engine`) passes: unit tests, 350+ golden
  files in engine and wire mode, the wire tests (including a concurrency stress test), the
  storage-format and layout tests.
- **Differential random queries** (round 17, agent 35): `tests/difftest.rs` generates random
  schemas, data, queries and DML from a seed, compares the engine with the reference
  statement by statement, runs TLP / NoREC oracles and planner-setting variants, and reduces
  failures to minimal reproducers. `smoke` (fixed seeds, a few minutes) is a regression check
  against `tests/difftest/smoke_known.txt`. Score, what it found and fixed, and what is open:
  `tests/difftest/STATUS.md`. Round 17 ran about 50,000 statements; its wrong-results fixes:
  grouping sets nulling constants they shouldn't, max / min ties, bpchar keys (merge joins,
  grouping, set operations), self-join elimination over outer join clauses. Open wrong
  results: a pseudoconstant qual of an inner join under an outer join gating the outer join
  (`c LEFT JOIN (a JOIN b ON (SELECT false)) ON ...` loses c's rows), and the empty grouping
  set over no input rows (constants equal to keys).

## Repository layout

- `crates/pg-engine`: the engine.
  - `src/sql/parse.rs`: libpg_query-based parser (plus `scs.rs` for standard_conforming_strings).
  - `src/sql/binder/`: name resolution, typing, coercion, SQL-function inlining, RLS, rules.
  - `src/sql/paths*.rs`, `src/sql/paths/`, `src/sql/joins.rs`, `src/sql/costs.rs`,
    `src/sql/extstat.rs`, `src/sql/binder/optimize.rs`, `src/sql/binder/sublink.rs`,
    `src/sql/binder/partprune.rs`: the cost-based planner, following PostgreSQL's
    `costsize.c` / `allpaths.c` / `joinpath.c` closely.
  - `src/sql/explain.rs` (+ `explain/`): EXPLAIN output.
  - `src/sql/ruleutils*.rs`, `src/sql/nodetree.rs`: deparse (`pg_get_*def`) and
    `pg_node_tree` output.
  - `src/exec/`: executor. `src/plpgsql/`: PL/pgSQL. `src/session/`: sessions, DDL
    (`session/ddl/`), cursors, settings, wire protocol (`src/server.rs`).
  - `src/catalog/`: catalogs, plus PostgreSQL-faithful size models (`heapsim.rs`, `btsim.rs`,
    `hashidx.rs`, `gistsim.rs`, `pglz.rs`, `lz4.rs`) that give PG's `ctid`s, physical row
    order and relation sizes.
  - `src/types/`: datatypes (incl. `regex/`, a port of PG's regex engine; `collation/`;
    `xml/`; `mbconv/` encodings).
  - `src/storage/`, `src/txn/`: key-value storage and MVCC.
  - `tests/golden.rs` + `tests/sql/*.sql|.out`: golden tests recorded from PostgreSQL 18.
  - `tests/pg_regress.rs` (the harness: `tests/harness/regress.rs`, shared with
    `tests/pg_contrib.rs`, the contrib modules' tests): the regression suite. `tests/regress/STATUS.md`: the
    classified list of every remaining difference. `tests/regress/REPORT.md`: last run report.
  - `tools/heavy.sh`: the one-heavy-job-at-a-time lock (see "Machine rules").
  - `tools/bench/`: the benchmark harness against PostgreSQL (`run.sh`, `suite.sh`,
    `perf.sh` for profiles, `report.py`); method and results in `BENCH.md`.
- `crates/pg-icu`: ICU4C 78.1 built from source (exact collation behaviour; needs curl or
  wget, tar, make and a C++17 compiler, about 60 s, cached in `target/icu4c`).
- `crates/pg-snowball`: Snowball stemmers for full-text search.

## Setting up a new machine

1. Rust stable (edition 2024), Docker, `pnpm` for the web side (not needed for the engine).
2. Start the reference PostgreSQL. It must be exactly this image (PostgreSQL 18.6, Alpine,
   musl, ICU 78.1) and initialised with `--locale=C`:

   ```sh
   docker run -d --name roll-engine-ref -p 55499:5432 \
     -e POSTGRES_PASSWORD=pw \
     -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
     postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
   ```

   The goldens connect to `127.0.0.1:55499` as `postgres` / `pw`. The regress harness drives
   the container's own `psql` with `docker exec` and reaches the engine through
   `host.docker.internal` (on Linux, add `--add-host=host.docker.internal:host-gateway`).

   On a Linux host whose firewall drops traffic from the docker bridge (ufw's default deny
   incoming does), the container can't reach `host.docker.internal`; don't open the firewall.
   Bind the engine to `127.0.0.1` and relay over a Unix socket instead: a sidecar sharing the
   reference container's network namespace (`docker run -d --network container:roll-engine-ref
   -v <dir>:/relay nginx:alpine`, with a `stream { server { listen 127.0.0.1:<port>; proxy_pass
   unix:/relay/e<port>.sock; } }` config) and a host process accepting on `<dir>/e<port>.sock`
   and connecting to `127.0.0.1:<port>`. Then run the harness with
   `PGREGRESS_ENGINE_HOST=127.0.0.1`.
3. Build and run the tests (through the lock):

   ```sh
   crates/pg-engine/tools/heavy.sh env CARGO_BUILD_JOBS=2 cargo test -p pg-engine
   ```

## How to test

**Goldens** (`tests/sql/<name>.sql` → `.out`, recorded from PostgreSQL):

```sh
# compare (all, or a subset by substring)
GOLDEN_FILTER=<name> cargo test -q -p pg-engine --test golden
# record a new golden from the reference: create the .sql, an empty .out, then
: > crates/pg-engine/tests/sql/<name>.out
GOLDEN_RECORD=127.0.0.1:55499 GOLDEN_PASSWORD=pw GOLDEN_FILTER=<name> cargo test -q -p pg-engine --test golden
```

- Use a `GOLDEN_FILTER` that matches **only** your new file: a filter like `_d` once re-recorded
  15 unrelated goldens.
- Never hand-edit a `.out`; always re-record from PostgreSQL.
- Each golden runs in its own database, but roles and tablespaces are cluster-global: drop
  any you create (start and end of the file).
- Avoid raw OIDs and `pg_temp_N` names in outputs.
- psql hides `CONTEXT` on NOTICE/WARNING by default, but goldens record it.
- `golden` runs every file on one thread, in name order, each in a fresh store; `golden_wire`
  runs each on a connection thread of its own. `golden_shuffled` runs a sample (60 files) in a
  shuffled order on one thread, so a file's statements run after other files' than in
  `golden`: a difference there means some state outlived a statement (see "Session and thread
  state"). `GOLDEN_SHUFFLE=all` runs every file (as long as `golden`), `GOLDEN_SHUFFLE=<n>` n
  files, `GOLDEN_SHUFFLE=off` none; `GOLDEN_SHUFFLE_SEED=<n>` another order (the failure message
  names the seed and the files before the differing one, and says whether it matches alone).

**Regression suite** (`tests/pg_regress.rs`, harness in `tests/harness/regress.rs`, `#[ignore]`d):

```sh
# a subset (test_setup always runs first; add prerequisites such as create_misc,create_index)
PGREGRESS_ONLY=join,subselect PGREGRESS_DIR=/tmp/pgregress PGREGRESS_DB=regression_x \
PGREGRESS_PORT=55843 PGREGRESS_REPORT_DIR=/tmp/pgregress/report \
  cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

The contrib modules' tests are a test binary of their own (`--test pg_contrib`, same harness,
`PGCONTRIB_ONLY=<modules>`), so `--test pg_regress -- --ignored` runs only the main suite; its
engine's data directory and binary copy are per process, never the main suite's.

- A full run takes about an hour; subsets take minutes.
- Without `PGREGRESS_REPORT_DIR` a run overwrites `tests/regress/REPORT.md`.
- `PGREGRESS_DB` must start with `regression` or `regress_`: the harness cleanup only drops
  those names, so parallel runs don't destroy each other.
- If the container can't reach the engine through `host.docker.internal` (a host firewall
  dropping the docker bridge's traffic, as on Linux servers that only allow a tailnet), run the
  engine side's psql in a helper container on the host network instead: `docker run -d --name
  <x> --network host --user $(id -u):$(id -g) -v $PGREGRESS_DIR:$PGREGRESS_DIR --entrypoint sleep
  <the pinned image> infinity`, then `PGREGRESS_ENGINE_CONTAINER=<x>`.
- After any build, check it **succeeded** before trusting a regress run: a failed build
  silently reuses the previous engine binary.
- Some files are batch-dependent (catalog sizes grow with earlier files); compare like for
  like.
- Timeouts: `PGREGRESS_TIMEOUT` (default 300 s) is how long **one statement** may run with psql
  printing nothing (psql `-a` echoes each statement as it starts it); past it the file counts
  as stalled, the engine restarts and the file resumes after that unit. `PGREGRESS_FILE_TIMEOUT`
  (default 3600 s) caps a whole run of a file. (It used to be one cap per file: `join` takes
  about 400 s on a loaded machine and lost its last units to it.)
- `PGREGRESS_HUNKS` (default 12): how many differing units per file `hunks/` shows.
  `PGREGRESS_ENGINE_BIN`: the engine binary to run (default: the one cargo built), e.g. one
  copied aside with the test binary (`cargo test --no-run`), so the source can change while a
  run waits for a heavy slot.
- The reference cluster is shared, and tablespaces are cluster-global: when another run already
  made test_setup's `regress_tblspace`, the reference's `CREATE TABLESPACE` fails; the harness
  drops just that `ERROR` line from the reference's output (the engine is compared with what a
  fresh cluster prints), and notes it on stderr. And while another run's database
  (`regression*`) is on the reference, a run ending leaves the tablespaces and roles that
  appeared during it (they may be the other run's: a run once dropped `regress_tblspace` under
  another, whose tablespace and stats files then lost 165 units).
- A full run outlives a tool's 2-hour background limit: start it detached (`setsid nohup ... &`)
  and poll its log. A run killed midway leaves its `PGREGRESS_DB` on the reference (drop it)
  and skips the cleanup of the roles / tablespaces it made.
- Stacks of a running engine (no gdb on the host): a container sharing the host's pid
  namespace, e.g. `docker run -d --name x --pid=host --cap-add=SYS_PTRACE --security-opt
  seccomp=unconfined -v <dir of the engine binary>:<same>:ro -v /lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu:ro
  -v /lib64:/lib64:ro nginx:alpine sleep infinity`, `apk add gdb` in it, then
  `docker exec x gdb <binary> -p <pid> -batch -ex 'thread apply all bt 60'` a few times.

**Probing PostgreSQL directly:**
`docker exec -i roll-engine-ref psql -U postgres -d <scratch_db> -At < file.sql 2>&1`.
Prefix scratch objects, and drop scratch databases and roles afterwards.

## Machine rules (learned the hard way)

- **Limit concurrent builds, not agents.** Six builds at once (each building, running a debug
  engine and driving Docker) ran a machine out of RAM. Any number of agents can read,
  edit and probe PostgreSQL; the heavy part is capped by `tools/heavy.sh`.
- **Every heavy command goes through `tools/heavy.sh`**: cargo build/test/clippy, regress and
  golden runs. It runs `HEAVY_SLOTS` jobs at once across agents (default 1; 2 is fine on a
  62 GB / 16-core machine). Hold a slot only for the command itself.
- **`CARGO_BUILD_JOBS=2`**, and wrap long cargo commands with a timeout.
- **Disk:** on macOS the default `split-debuginfo = unpacked` leaves every build's `*.o` files in
  `target/debug/deps`. 1.5 million of them (~500 GB) once filled the disk. `heavy.sh` prunes
  ones older than 4 hours. It also builds with `CARGO_PROFILE_DEV_DEBUG=line-tables-only`
  (full debuginfo made each test binary ~400 MB and a worktree's target ~15 GB), drops
  `target/debug/incremental` once it passes `HEAVY_MAX_INCREMENTAL_GB` (3) or the disk runs low,
  and waits instead of starting a job while less than `HEAVY_MIN_FREE_GB` (30) is free. The disk
  is shared with other projects: delete a finished worktree's `target/` and your `/tmp` and
  `~/.cache` data when you're done.
- Stop any engine you start after each run; never kill processes, drop databases or delete
  files you didn't create.

## Conventions

- Follow `AGENTS.md`: Rust edition 2024, dependencies in the root `[workspace.dependencies]`,
  match the surrounding style and comment density, don't run `cargo fmt` on others' files.
- Stored data must stay loadable. Format changes need a migration: see
  `Catalog::index_key_format` and `Catalog::storage_format` with `Engine::upgrade_storage` in
  `src/engine.rs` (batched, resumable, tested by `tests/storage_upgrade_batches.rs`).
- Every fix gets a golden recorded against PostgreSQL. Correctness (results, errors, data)
  before EXPLAIN text.
- Commit only verified states: the whole `cargo test -p pg-engine` passes first.

## Session and thread state

A statement's rows, errors and plan may depend only on the catalog it sees, its session's
settings and the data, never on which statements ran before it on the thread or in the
process. The server runs each connection on a thread of its own, but the engine is a library:
embedders (and the golden harness) run many sessions, databases and even engines on one thread,
and `cargo test` runs several engines in one process. Oids, transaction ids, catalog versions'
starting points and row keys repeat across engines and stores, and freed `Arc`s' addresses are
reused at once.

What outlives a statement today, and what keys or invalidates it (round 16 audit, agent 13):

- **Keyed by the scope it depends on**:
  - `Session` prepared statements' bindings (`Prepared::bound`): catalog version, search path,
    role, row_security. The generic plan's cost: by a `Weak` to the binding.
  - PL/pgSQL (`plpgsql/mod.rs`): `PLANS` (body, statement, `Database::instance`, catalog
    version, search path, role, row_security, variable types, a DO block's
    standard_conforming_strings), `LEXED` (database, function oid, body: each function is
    scanned at its own first run), `PARSED` (body; a DO block's with
    standard_conforming_strings), `COMPILED` / `VALIDATED` (database, oid / schema + name, body),
    `pl_conflicts` / `pl_strict_params` (per session, by function and body).
  - Catalog versions are unique in the process (`Catalog::bump`, also for a rebased commit), so
    a version identifies one catalog of one database's history; rolled-back DDL's versions are
    never reused.
  - `EQ_OPS` (`paths/joinpath.rs`, the `=` per type to rebuild hash keys): by a `Weak` to the
    catalog and the search path, started over by every `remember_equalities`.
  - `SYS_ROWS` (`explain::sys_tuples`, a system catalog's row count): by the catalog's address
    with a `Weak` holding it.
  - `SessionState::sys_rows` (`catalog::system::rows`, round 17 agent 36: the generated rows
    of `pg_class`, `pg_attribute`, `pg_attrdef`, `pg_type`, `pg_namespace`, `pg_index`,
    `pg_constraint`, `pg_proc`, the catalogs whose rows read nothing but the catalog, roles,
    path and settings): by a `Weak` to the catalog, current and session user, search path and
    `Settings::generation` (unique per set of setting values). Only catalogs generated from
    those alone may join the list (`rows_cacheable`): not statistics, locks, sequences, large
    objects or other databases.
  - `sql::parse` `PARSED` (parse trees, per thread): by the text and the parser options (a
    pure function); `ast::parse_exprs` / `parse_stmt` (stored texts) under tags of their own.
  - `costs::SLOT_VALUES` (statistics slots' parsed values, per thread): by
    `FormatCtx::settings` (the settings' generation; 0, a context not made from a session's
    settings, isn't cached), the type and the texts; plain scalar types only.
  - `sizing::TRACKED` (tables that ever stored a compressed or out-of-line datum, whose
    values `note_read` remembers; process-wide): by `Database::instance` and table, only
    ever added to.
  - `exec::sizing::METHODS` (a stored datum's compression method, process-wide): by
    `Database::instance`, table, column and value.
  - `func::range::user_constructors` (process-wide): by the whole range type and its
    constructors' oids. `func::pubsub::RESETS` (subscription statistics' reset times): by the
    cluster (its `OidGen`, held by a `Weak`) and the subscription. `types::collation::get`: by the whole definition. `regex::compile`: by
    pattern, flags and strategy. Interners (`func::intern`), the built-in registries and the
    `include_str!` tables are pure.
  - Subquery / CTE ids (`Binder::fresh_id`) are unique in the process: `ExecCtx`'s
    `subq_cache`, `exists_hash`, `scalar_hash`, `ctes`, `work` are keyed by them, and a SQL
    function's statements (bound by binders of their own) run in their caller's context.
- **Set from the catalog by `binder::note_catalog`** (every `Binder::new`, every statement a
  session runs, every DDL command): `cast::USER_CASTS`, `optimize::ALIAS_FAMILIES`. Readers
  outside a binder (CREATE AGGREGATE's lookup, PL/pgSQL assignment casts) see this statement's
  catalog's. `partition::PRUNING` (enable_partition_pruning) is the last `Binder::new`'s: it
  only decides which partitions run-time pruning skips.
- **Scoped to a call**, put back by a guard's `Drop` (on an error and on a panic too; the
  server turns a panic into an ERROR and keeps the session): `current::CURRENT`,
  `partition::COLLS`, `range::COLL`, `depend::USER_TYPES`, `dml::DML_TARGETS`, PL/pgSQL
  `DEPTH` / `TRIGGER_DEPTH` / definer / SET clauses, and through `crate::scoped::set` /
  `set_ref`: `ddl::TOP_LEVEL`, the binder's `SECURITY_QUALS` / `QUERY_RLS` / `PHV_OUTPUTS` /
  `EXISTS_SUBLINKS` / `CASE_TEST_DOMAIN`, ruleutils' `RTABLES` / `NO_INDENT` (`FN_ARGS` by
  `body::Reset`), explain's `FILTER_OVER_WINDOW` / `MERGE_MEMBERS` / `UNDER_LOCK_ROWS` /
  `RULE_RETURNING`, `scs::OPTS` / `QUIET`, `regex::INTERRUPTS`, trigger `MOVING_ROW` /
  `UPDATED_COLS`; `event_trigger::STATES` (`Begun`'s `Drop`), `CTE_ESTIMATES` / `CTE_ORDERS`
  (`CteEstimatesPushed`), the session's error frames (`SessionState::framed`).
- **Per-statement scratch set in one place and taken in another** (explain's `WINDOW_TLIST` /
  `WINDOW_ORDER` / `INH_SCANS`, `PHV_PENDING`, and as a backstop everything above that has a
  rest value): cleared by `scoped::reset_statement_scratch` at the start of every statement a
  `Session` runs (`with_ctx_path`).
- **A session's running stacks** (`SessionState`'s definer roles, `sro`, frames, tables in use,
  COPY progress): pushed and popped by hand in a few places; `Session::recover_from_panic`
  (what the server runs after catching a panic, and an embedder catching one must call)
  clears them, with the scratch. `tests/session_state.rs::panic_leaves_no_state` panics just
  after each guarded point is set (`scoped::panic_at_for_test`) and checks the session's next
  statements run as in a session where the statement never ran.
- **Per session, synced before each statement**: `scs::OPTS` (lexer settings), `SOURCE`,
  `catalog::SESSION_TEMP_SCHEMA` (set by `execute_with`; a Parse / Describe message binding
  before any execute on a thread another session used sees that session's `pg_temp`).
- **Shared on purpose** (PostgreSQL's shared state): the database's locks, sequences, xids,
  relation files model, cumulative statistics, LISTEN / NOTIFY.

Rules for a new cache:

1. Prefer state on the `Planner`, `Binder` or `ExecCtx` (one statement) over a `thread_local!`
   or a `static`.
2. Key it by everything its value depends on: the catalog (by version, or a `Weak<Catalog>`),
   the database (`Database::instance`, never an oid alone or an address), the search path,
   the role, the settings it reads. Never key by `Arc::as_ptr` / an address without holding a
   `Weak` (or the `Arc`) to keep the allocation.
3. A thread-local set for a call goes back in a guard's `Drop` (`crate::scoped::set` /
   `set_ref`, which also give `panic_at_for_test` a point), so errors (`?`) and panics restore
   it; scratch set here and taken there gets cleared in its module's `reset_scratch` and
   reported by `unsettled`. Add the point to `panic_leaves_no_state`.
4. Add a case to `tests/session_state.rs` (the same script alone, after another store's,
   after another database's, after the same session's) or a golden whose statements would hit
   a stale entry.
5. `golden_shuffled` (below) runs the goldens in another order on one thread; run it with
   `GOLDEN_SHUFFLE=all` after adding one.

## Decisions already made (don't revisit)

- **Server-side file access is refused by default** for every role (`pg_read_file`,
  `pg_read_binary_file`, `pg_stat_file`, `pg_ls_dir`, `lo_import`/`lo_export` of server paths,
  `COPY ... FROM / TO '<server file>'`), with PostgreSQL's exact permission errors. This is what
  managed services (Neon, RDS, Supabase) do.
- **Opt-in, confined directory for development and testing**: `roll-engine
  --allow-server-files <dir>` (embedded: `EngineOptions::server_files`), a startup setting, not
  a GUC. The roles PostgreSQL allows (EXECUTE on the functions, which only the superuser has
  until granted; privileges of `pg_read_server_files` / `pg_write_server_files` for COPY) then
  get PostgreSQL's behaviour, inside `<dir>` only: it stands for the data directory, relative
  paths resolve against it, and every path (for every role, superuser included) gets
  PostgreSQL's check for a role without `pg_read_server_files` (`convert_and_check_filename`:
  "absolute path not allowed" / "path must be in or below the data directory"), plus a
  real-path check refusing symbolic links that lead out (PostgreSQL would follow them).
  `COPY ... PROGRAM` stays refused for everyone. Code: `src/server_files.rs`; tests:
  `tests/server_files.rs`, `tests/sql/server_files_roles.sql`. The regress harness runs with it
  (`PGREGRESS_DIR` as the directory), which is how misc_functions, largeobject and copy match.
- **Default table layout is PostgreSQL-exact**: rows under a heap-ordered row id, primary key
  as a separate index, so scans return PG's order and `ctid`s match.
- **`roll_pk_clustered`** (opt-in storage parameter) keeps rows keyed by the primary key for
  speed-critical tables, accepting key-order scans. `CREATE TABLE … WITH
  (roll_pk_clustered)` without a key is accepted and waits for one (pg_dump creates keys
  after loading data); `ALTER TABLE … SET` without a key errors. Tests: `tests/pk_clustered.rs`.

## What remains

All remaining work is planner/EXPLAIN (`src/sql/paths*`, `src/sql/binder/optimize.rs`,
`src/sql/binder/sublink.rs`, `src/sql/explain.rs`, `src/sql/costs.rs`), biggest first. Unit
numbers refer to the regress harness's per-file unit counter; `tests/regress/STATUS.md` lists
them all with the "Fixed in round N" sections describing what was already tried.

1. **PlaceHolderVars evaluated at joins** (join 360, 362/363, 368;
   partition_join 41). Round 14 (agent 8) evaluates them at joins: a pulled-up subquery's
   outputs an outer join nulls (as written) are PlaceHolderVars the join group computes at the
   lowest relation of their ph_eval_at (`lift.rs` `flatten_join_subquery`, `plan::phv_projection`,
   `joinpath.rs` `Phv` / `with_phvs`), so 328/343/388 match. Left: remove_useless_result_rtes
   moving PlaceHolderVars of RESULT relations onto their join partner (347, 366: done), one computed
   in a LATERAL subquery scan (345: the plan's shape since round 16, agent 30, PlaceHolderVar
   nested loop parameters computed by the outer side; its reltarget's column order since
   agent 28: done) or over
   a parameterized appendrel (360: a FROM-less UNION ALL arm as an appendrel member),
   PlaceHolderVars with sub-selects (362/363), a dummy Result showing a view's expressions
   (368), a LATERAL subquery over a join pulled up with PlaceHolderVars carrying ph_lateral
   (join 332, partition_join 41 / 204; STATUS.md round 16, agent 30). (744, a hash join
   parameterized by a LATERAL inner side: done in round 16, agent 17; 426, a JOIN USING alias
   as a PlaceHolderVar at its join: round 16, agent 30.)
2. **Remaining join / partition_join / partition_prune plan shapes** (join 12, partition_join 2,
   partition_prune 1 unit; round 14's agent 5 and round 16's agent 12 sections in STATUS.md have
   what changed and why the rest is open; round 16's agent 17 section has join relations
   parameterized by LATERAL references and why partition_prune 410 stays open (265 and 442 done in
   round 16, agent 27: SubPlan copies per place, reconsider_outer_join_clauses); partition_join 74/88/92 done in round 16, agent
   21: expression members of equivalence classes, IN over a join pulled up before planning,
   with why 86 stays open; join 123/410/502/503 done in round 16, agent 22: a FROM
   subquery's sorted paths, class clauses' first making, ParamPathInfo rows; join
   499/543/612/632/654/655/819/888 done in round 16, agent 23: self-join elimination order,
   SubPlan classes, semi join unique-ification, join target list order; join 345/560/620/674
   done in round 16, agent 28: the join search's enumeration (classes with constants across semi
   joins, replaced equalities in min_lefthand), scans' reltargets (`ScanPlan::reltarget`),
   with why 715 stays open (a FROM subquery holding a semi join is planned as a group of its
   own, not flattened into its parent's); join 662/663/671/706/781, partition_join 52/234/553/588/608 done in round
   16, agent 26, whose section lists the causes of the rest of join 674–808 and
   partition_join 98–300; join 691/765, partition_join 138/210/300 done in round 16, agent 40:
   class clauses made in range table order, child joins of partitioned join relations only,
   partitionwise paths of join relations inside outer-join groups, DML sublinks pulled up over
   inheritance targets, whole-row Vars passed up): cost-driven choices (join 715),
   partitionwise LATERAL joins left (partition_join 48/207 done in round 16, agent 19; 204:
   a PlaceHolderVar), partition_prune 410. (partition_join 86, a unique-ified side passing
   non-key columns through, done in round 16, agent 31: a hashed Distinct with pass-through
   columns, semi joins' clauses in classes, child scans' loop counts; partition_join 98, a
   merge clause between two expressions, in its second batch: classes of expressions only.)
3. **subselect** (3 units; rounds 13–14 fixed the row comparisons with sub-selects, most of
   PostgreSQL 18's IN / VALUES pull-up, sub-select copies, per-member EXISTS alternatives,
   reltarget orders, strict-NULL folding, early unique-ified semi joins, IN over a
   set-returning select list, LATERAL quals in subquery levels, catalog index paths in joins and
   LATERAL VALUES with sub-selects, see STATUS.md):
   - 327: a correlated aggregate IN as a LATERAL subquery (Memoize over a Subquery Scan).
   - 328 (an EC member relabeled to the class's collation: `("*VALUES*".column1)::text`), 332
     (unique-ifying a semi join's side on a computed expression).
   (merge 348, a MERGE's sub-select copies: done in round 16, `paths/sublinkcopy.rs`.)
4. **Row marks in VERBOSE target lists**: done for UPDATE / DELETE / MERGE over joins with a
   plain (or, for MERGE, partitioned) target, and for locked scans (a FROM subquery's FOR
   UPDATE), and (round 15) SELECT ... FOR UPDATE / SHARE: one LockRows at the level's top
   (`sql/paths/rowmarks.rs`), locking only the rows the level returns, EvalPlanQual rechecks
   of the whole level; its function / VALUES whole-row marks, semi join inner ctids and an
   inheritance tree's tableoid shown. Round 16 (agent 9): a FROM subquery's whole-row mark
   (`s.*`), a DML statement's joined subquery kept as its Subquery Scan, reference-marked
   relations without index-only scans, locks pushed into FROM subqueries and views, and
   rule-rewritten statements explained as Postgres's list of plans (`explain.rs`
   `rule_statements`). Left (STATUS.md, round 16 agent 9): merge 462 (Inner Unique over an
   empty partitioned target). (returning 105, a rule action over a view: done in round 16,
   agent 29: `*RULE*` placed where Postgres has OLD, the view's relations after the action's
   in the range table, NEW computed in the action's SET.) (Round 16, agent 15: the level with only `s.*` marks gets its LockRows, a
   whole-row-marked subquery keeps every column; agent 20: a unique-ified semi join under FOR
   UPDATE locks above the join, a locked LIMIT subquery under its Limit, a DML statement's
   FROM subquery's `o.*`; agent 24: a level's several locking clauses all lock, marks in
   preprocess_rowmarks' order, whole-row marks' widths in the paths and EXPLAIN, an UPDATE
   join's final target list width.) (VERBOSE lists of UPDATE /
   DELETE joined with an inherited target of several members, and a CTE's / VALUES' /
   function's whole-row mark there: round 16, agent 14.)
5. The long tail in STATUS.md. (select_parallel, select_distinct and join_hash's batch counts
   match since round 16, agent 14; with since round 16, agent 20; groupingsets and join_hash
   fully since round 16, agent 18: SubPlans numbered in subquery_planner's order for group
   keys and quals, GROUPING() argument copies, resjunk group keys up the plan; the target
   list first since round 16, agent 27; FROM subqueries after the level's expressions and
   MIN / MAX InitPlans last since agent 32, whose section lists agent 27's other leftovers.)
6. **equivclass**, **inherit** and **collate.icu.utf8**: done (round 15, agent 10).
7. **partition_aggregate**: done (137/137).
8. The 18 plan-choice units listed in STATUS.md ("Plan choice visible in results").

Smaller known differences that don't fail a unit today (round 14, agent 11 fixed the DISTINCT
`((a + 1))` Sort output listed here before): a pulled-up one-row VALUES' constants aren't
folded into the expressions over them (`SELECT a + b FROM (VALUES (1, 2)) t(a, b)`: Postgres
shows `3`); a FROM subquery not pulled up whose whole row is read above a join loses its
Subquery Scan (a table's whole row goes up as `r.*` since round 16, agent 40; a subquery's is
bound as a ROW() of its columns) (`SELECT x.a, s FROM t x, (... OFFSET 0) s`). (Read
over the subquery's own scan it is `s.*` and its `IS NULL` stays whole since round 16, agent
29.)

Not started and lower priority: `roll_pk_clustered` adds a primary key's duplicate check by raw
key bytes, so duplicates that are only equal under a nondeterministic collation aren't caught
when adding a key to a clustered table.

## Starting prompt for the next agent

Paste this into the new agent, from the repository root on branch
`docs/postgres-architecture-strategy`:

```text
You are continuing work on crates/pg-engine, roll's in-process PostgreSQL engine. The goal is
1:1 behaviour with PostgreSQL 18.6, measured by PostgreSQL's own regression suite. It is at
99.19% (50,737 of 51,150 statements). Work autonomously and don't ask questions unless a
decision is genuinely mine to make.

First read, in this order: AGENTS.md, crates/pg-engine/HANDOFF.md (setup, testing, machine
rules, decisions, the prioritized remaining work), and crates/pg-engine/tests/regress/STATUS.md
(every remaining difference, per file and unit, and what earlier rounds already tried).

Then:
1. Set up the reference PostgreSQL container exactly as HANDOFF.md says (pinned image digest,
   --locale=C, port 55499, password pw) and run the full crate test suite through
   crates/pg-engine/tools/heavy.sh to confirm everything passes on this machine.
2. Work through "What remains" in HANDOFF.md in order. For each item: probe PostgreSQL, fix the
   engine, add a golden recorded from PostgreSQL, re-run the affected regress files (subset
   runs with PGREGRESS_ONLY and PGREGRESS_REPORT_DIR), and make sure nothing that matched
   before regresses. Results and errors before EXPLAIN text.
3. Follow the machine rules: limit concurrent builds (HEAVY_SLOTS), every build/test/regress/golden run
   through tools/heavy.sh, CARGO_BUILD_JOBS=2.
4. After each verified batch (the whole `cargo test -p pg-engine` passes), update STATUS.md,
   commit with a clear message, and push the branch.
```
