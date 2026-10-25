# contrib extensions: status

The contrib modules' own regression tests (`contrib/<module>/sql` at `REL_18_6`) run through
the engine and a reference PostgreSQL 18.6 and compared unit by unit, as the main suite is
(`tests/pg_contrib.rs`; `pg_contrib` in `tests/harness/regress.rs` documents the options). `REPORT.md` is the
last run's per-file table.

## How extensions work here

- An extension is PostgreSQL's own control file and scripts (`src/ext/files/`, copied from
  REL_18_6 contrib), run by CREATE EXTENSION / ALTER EXTENSION UPDATE
  (`src/session/ddl/extension.rs`) the way `extension.c` runs them. Its objects, their OIDs,
  dependencies (pg_depend `e` for members, `n` for required extensions), update paths and
  pg_available_extension_versions all follow from that.
- `LANGUAGE C` functions are native Rust code found by library and symbol
  (`src/ext/native.rs`, one module per extension under `src/ext/`).
- Types an extension's script creates that the engine implements natively are
  `Value::Ext(kind, bytes)` (`src/ext/types.rs`, kinds stored with values): their default
  operator classes' order and equality are the value's, so sorting, grouping, hashing and
  btree index keys follow them (citext's index keys are its lowercased bytes). A
  binary-coercible cast to or from the built-in type it is stored as (citext and text)
  converts the value; an internal-language function over it (`AS 'texticlike'`) gets the
  built-in value.
- User operators win IN lists and `= ANY` as `oper` picks them (`user_op_def`), and a C
  operator function shows as its operator, so the planner matches it to btree indexes.
- Only the extensions with files in `src/ext/files/` are available. The older stand-ins
  (`session::ddl::EXTENSIONS`: CREATE EXTENSION accepted, nothing installed) remain for the
  ones not ported yet.

## Running

```sh
# a reference of the pinned image started with the modules' temp-config settings
docker run -d --name a39-ref -p 127.0.0.1:55939:5432 -e POSTGRES_PASSWORD=pw \
  -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" -v $D:$D <pinned image> \
  -c shared_preload_libraries=pg_stat_statements -c max_prepared_transactions=5
PGCONTRIB_ONLY=uuid-ossp,citext PGREGRESS_DIR=$D PGREGRESS_CONTAINER=a39-ref \
  PGREGRESS_ENGINE_CONTAINER=<psql container on the host network> PGREGRESS_PORT=55940 \
  cargo test -p pg-engine --test pg_contrib -- --ignored --nocapture
```

## Scores

Last full run (round 19, agent 53, `tests/contrib/REPORT.md`): 4,153 / 4,158 units of 19 modules (99.9%), pg_stat_statements 996 / 1,013. Baseline (before any
extension was ported; CREATE EXTENSION was a stand-in): 2,752 / 7,733
units (35.6%). The reference matches `expected/` for every module except postgres_fdw and
dblink (they connect to a loopback server).

| module | units | matching | status |
|---|---|---|---|
| uuid-ossp | 23 | 23 | done |
| pgcrypto | 369 | 369 | done (OpenSSL 3 provider behaviour: bf/des/cast5 refused as the reference refuses them) |
| citext | 544 | 544 | done |
| hstore | 309 | 309 | done |
| pg_trgm | 261 | 261 | done (GIN/GiST plans included) |
| btree_gin | 297 | 297 | done |
| btree_gist | 616 | 613 | see below |
| pg_stat_statements | 1013 | 996 | see below |
| unaccent | 34 | 34 | done |
| fuzzystrmatch | 56 | 56 | done |
| ltree | 363 | 363 | done |
| intarray | 182 | 182 | done |
| tablefunc | 91 | 91 | done |
| cube | 316 | 316 | done |
| earthdistance | 170 | 170 | done |
| isn | 51 | 51 | done |
| seg | 196 | 196 | done |
| lo | 38 | 38 | done |
| amcheck | 204 | 202 | applicable checks only (see below) |
| pageinspect | 291 | — | not applicable (not available) |
| pg_buffercache | 38 | 38 | done (no buffer pool: every buffer unused) |
| postgres_fdw | 2053 | — | assessed, not done |
| dblink | 218 | — | assessed, not done |

### What is left, per module

- **btree_gist**: rows of an unordered GiST index scan come back in PostgreSQL's GiST tree
  order (macaddr, macaddr8, bytea units without ORDER BY): the engine reads the index's rows in
  its own order. A real GiST page model for these classes would be needed.
- **seg** (unit 167, the GiST Index Only Scan for `@>`): matches since round 18.
- **seg** unit 170: done (round 19, agent 53). Grouping, DISTINCT, set operations, hashed
  SubPlans, hash joins and Memoize follow PostgreSQL's hashability
  (`catalog::userobj::type_hashable`): a type whose `=` doesn't hash (money, bit, seg, an
  anonymous record) is sorted, and a sorted grouping returns its groups in its keys' order
  (`PlanEstimate::group_sort`, round 19, agent 48).
- **seg security**: done (round 19, agent 53). PL/pgSQL, SQL functions and DO blocks bind
  each statement under the search path as it is when the statement runs (plancache's
  search_path check); CREATE SCHEMA's elements run with `search_path` set to the schema
  prepended, as a GUC at a nest level of its own (current_setting shows it); DDL a
  function called by a partition bound runs stays made (`Ddl::absorb_nested`).
- **amcheck**: the engine's storage isn't PostgreSQL's pages, so there are no nbtree, GIN or
  heap page invariants to verify; the functions check what applies (target, access method,
  validity, arguments, block ranges) and report no corruption. Units that differ: superuser
  `UPDATE pg_attribute` (check_btree 59: catalog DML the engine doesn't take, agent 51's
  area), and `pg_stat_io` bulk reads not counted by verify_heapam (check_heap 27). The
  check_gin timeout is gone: the text[] GIN build (10,000 rows) that stalled past 300 s takes
  about a second since round 19's index-build work.
- **pageinspect**: reads raw 8 kB PostgreSQL pages (heap, btree, GIN, GiST, BRIN, hash
  layouts, checksums). The engine's tables and indexes are key-value data, so there are no
  such pages to show; making the functions up would mislead. Not available
  (`CREATE EXTENSION pageinspect` fails as for an uninstalled extension).
- **postgres_fdw / dblink**: they open libpq connections from the server to other servers.
  Feasible in principle (the workspace has a PostgreSQL client; the FDW machinery exists),
  but in roll's model an engine dialing arbitrary hosts is an outbound-network (SSRF) and
  credential-handling decision, like server-side files: it should be an opt-in startup
  setting with an allow-list, the same policy as `--allow-server-files`. Not implemented; the
  suites need a loopback server too.
- **pg_stat_statements** (`src/ext/pg_stat_statements/`, `session/pgss.rs`): the engine
  starts it with `--shared-preload-libraries pg_stat_statements` (EngineOptions
  `shared_preload_libraries`; the contrib harness passes it). Statements are grouped by a
  jumble of the parsed statement and the catalog's column types (constants and aliases
  ignored, PostgreSQL 18's squashed IN / ARRAY lists), texts normalized with `$n`; calls, rows,
  plans, toplevel / nested tracking, track_utility / track_planning, privileges, the info view
  and reset. The engine has no WAL or shared buffers: blocks written report table growth,
  wal_records rows written, wal_bytes 64 per row, other buffer columns 0; plan time is the
  engine's binding and planning time. Left (17 units): two-phase commit statements (utility
  74-79: the engine has 2PC off) and utility 115, nested DO-block / function statement
  listing in level_tracking (12, 104, 131, 169, 191, 228, 232), a parallel plan's worker
  counts (parallel 8), three squashing cases (98, 99, 120). Unreferenced CTEs in EXPLAIN match
  since round 19.
