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

## Where it stands

As of commit `e890fb0` (2026-10-21), plus server-side files (below) and rounds 13–14 (STATUS.md):

- **Regression suite: 50,971 of 51,150 statements match (99.65%).** 179 differ. The full
  classified list, per file and per unit, is in `tests/regress/STATUS.md`.
  - 172 are `EXPLAIN` plan text (96%). Query results already match.
  - 6 are plan choices that show up in results (mostly which side a hash join builds on,
    grouping-set output order).
  - 1 differs **by design** (aggregates `balk`, a parallel plan's combine function: planner
    work), so the ceiling is about 99.998%.
- No engine crashes or panics in a full run. A full run of 286306f measured 50,792: STATUS's
  per-file counts hold except that run's artifacts (see STATUS.md). The "stalls" in full runs
  (`ALTER TABLE tt7 DROP q` in alter_table, fast_default 177–179) were no hang: every ALTER TABLE
  copied the whole catalog once per table (`column_using`), seconds per statement over a full
  run's catalog, so those files ran past the per-file timeout. Fixed in round 15;
  `tests/ddl_scaling.rs` counts the catalog copies DDL makes.
- The full crate test suite (`cargo test -p pg-engine`) passes: unit tests, 350+ golden
  files in engine and wire mode, the wire tests (including a concurrency stress test), the
  storage-format and layout tests.

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
  - `tests/pg_regress.rs`: the regression-suite harness. `tests/regress/STATUS.md`: the
    classified list of every remaining difference. `tests/regress/REPORT.md`: last run report.
  - `tools/heavy.sh`: the one-heavy-job-at-a-time lock (see "Machine rules").
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

**Regression suite** (`tests/pg_regress.rs`, `#[ignore]`d):

```sh
# a subset (test_setup always runs first; add prerequisites such as create_misc,create_index)
PGREGRESS_ONLY=join,subselect PGREGRESS_DIR=/tmp/pgregress PGREGRESS_DB=regression_x \
PGREGRESS_PORT=55843 PGREGRESS_REPORT_DIR=/tmp/pgregress/report \
  cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

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
  ones older than 4 hours and drops `target/debug/incremental` below 15 GB free.
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

1. **PlaceHolderVars evaluated at joins** (join 328, 343, 345, 347, 360, 362/363, 366, 368;
   partition_join 41). Round 13 pulls subqueries over joins up into the parent's join group
   (`lift.rs` `flatten_join_subquery`) when no output needs a PlaceHolderVar evaluated at a
   join (or the outer joins nulling it were reduced away); join 351 now keeps the dummy
   relation's expressions. What's left needs a plan node computing expressions at a join (a
   PlaceHolderVar whose `ph_eval_at` is several relations), constant PlaceHolderVars through
   remove_useless_result_rtes (343/347), qualified names in a dummy Result over catalog scans
   (368), a full join of one-row relations removed (366). join 744 differs only in plan choice
   (a parameterized hash join inside a nested loop).
2. **Remaining join / partition_join / partition_prune plan shapes** (join 71, partition_join 21,
   partition_prune 7 units; round 14's agent 5 section in STATUS.md has what changed): cost-driven
   choices (join 123/410/424/450/543, partition_join 21/74), equivalence classes with scan-level
   members (join 609/610), partitionwise joins with a LATERAL item
   (partition_join 41/43/45/46/48/204–207), IN over a partitioned subquery as a semi join
   (partition_join 86/88/92: the subquery is planned before the WHERE's sublinks are pulled up),
   a pulled-up UNION ALL's alias as a range table entry
   (partition_prune 392/393/399), partition_prune's parallel plans (377–383, 652), 265 / 410 /
   746.
3. **subselect** (6 units; rounds 13–14 fixed the row comparisons with sub-selects, most of
   PostgreSQL 18's IN / VALUES pull-up, sub-select copies, per-member EXISTS alternatives,
   reltarget orders, strict-NULL folding and early unique-ified semi joins, see STATUS.md):
   - 218: IN with a set-returning select list as a semi join over a Subquery Scan (its filter
     derived from the equivalence class).
   - 221: a pushed-down qual that reads a LATERAL reference stays the Subquery Scan's filter
     (qual_is_pushdown_safe refuses lateral Vars; the engine can't tell them from an outer
     query's values at that point).
   - 288: a system catalog's parameterized index path inside a join (catalog leaves are opaque
     to the join search).
   - 327: a correlated aggregate IN as a LATERAL subquery (Memoize over a Subquery Scan).
   - 328 (an EC member relabeled to the class's collation: `("*VALUES*".column1)::text`), 332
     (unique-ifying a semi join's side on a computed expression), 350 (a LATERAL VALUES list
     with a correlated sub-select inside).
   merge 348 needs the sub-select copies in a MERGE's action conditions and SET lists.
4. **Row marks in VERBOSE target lists**: done for UPDATE / DELETE / MERGE over joins with a
   plain (or, for MERGE, partitioned) target, and for locked scans (a FROM subquery's FOR
   UPDATE), and (round 15) SELECT ... FOR UPDATE / SHARE: one LockRows at the level's top
   (`sql/paths/rowmarks.rs`), locking only the rows the level returns, EvalPlanQual rechecks
   of the whole level; its function / VALUES whole-row marks, semi join inner ctids and an
   inheritance tree's tableoid shown. Left: merge 462 (Inner Unique over an empty partitioned
   target), returning 103/105 (rule-rewritten statements: EXPLAIN now shows the action's
   ModifyTable with the statement's RETURNING over its old / new rows; still differing: which
   relation takes the plain name, the target's restriction from the equivalence class with the
   statement's rows, the non-target rows' ctids), a FROM subquery's whole-row mark under a
   locking level (`s.*`, PG keeps its Subquery Scan), inherited UPDATE / DELETE targets.
5. **collate.icu.utf8** (23), **incremental_sort** (6: a LATERAL subquery over a function
   scan not pulled up, see STATUS.md's round 14 agent 7), **select_parallel** (1),
   **updatable_views** (13), **groupingsets** (12, including the sorted-vs-hashed choice and
   output order), **with** (10; 52/55/58/76/77 need a materialized CTE's sort order offered to a
   merge join), **memoize**, **equivclass**, **privileges** (8 each), then the long tail in
   STATUS.md.
6. **inherit 445** (a Merge Append ordered by an expression mapped into each UNION ALL arm),
   **inherit 453** (Merge Join over Merge Appends under a LIMIT).
7. **partition_aggregate**: done (137/137).
8. The 18 plan-choice units listed in STATUS.md ("Plan choice visible in results").

Smaller known difference that doesn't fail a unit today: `SELECT DISTINCT a + 1` by sorting
shows the Unique's output as `(a + 1)` where Postgres refers to the Sort's (`((a + 1))`). (The
two listed before round 14 are fixed: a plain DISTINCT's column lists, and unread subquery
outputs as NULLs in the subquery's select list, shown as constants above.)

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
