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

Last full run (round 19, agent 53, `tests/contrib/REPORT.md`): 4,153 / 4,158 units of 19 modules (99.9%), pg_stat_statements 996 / 1,013 (1,013 / 1,013 since round 19, agent 71: a run of that module). Round 19 (agent 59): dblink 218 / 218, postgres_fdw 1,664 / 2,053 (81.1%). Round 19 (agent 66): file_fdw 187 / 187. Round 19 (agent 71), a full run of every module: 7,683 / 8,270 units of 34 modules (pageinspect not applicable, postgres_fdw 1,664); the 10 modules with tests added this round 346 / 350 (tsm_system_time 14 / 16, auto_explain 10 / 12, the rest whole); amcheck 203 / 204 in a run after it. Baseline (before any
extension was ported; CREATE EXTENSION was a stand-in): 2,752 / 7,733
units (35.6%). The reference matches `expected/` for every module except dblink (the reference
has no `regress.so`, whose functions dblink's test creates).

| module | units | matching | status |
|---|---|---|---|
| uuid-ossp | 23 | 23 | done |
| pgcrypto | 369 | 369 | done (OpenSSL 3 provider behaviour: bf/des/cast5 refused as the reference refuses them) |
| citext | 544 | 544 | done |
| hstore | 309 | 309 | done |
| pg_trgm | 261 | 261 | done (GIN/GiST plans included) |
| btree_gin | 297 | 297 | done |
| btree_gist | 616 | 613 | see below |
| pg_stat_statements | 1013 | 1013 | done (round 19, agent 71; see below) |
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
| amcheck | 204 | 203 | applicable checks only (see below) |
| pageinspect | 291 | — | not applicable (not available) |
| pg_buffercache | 38 | 38 | done (no buffer pool: every buffer unused) |
| postgres_fdw | 2053 | 1664 | see below |
| file_fdw | 187 | 187 | done (files are the server files of `--allow-server-files`; programs refused) |
| tsm_system_rows | 16 | 16 | done (round 19, agent 71) |
| tsm_system_time | 16 | 14 | see below |
| pg_prewarm | 5 | 5 | done (no buffer pool: nothing is read; the range's blocks are reported) |
| pg_freespacemap | 15 | 15 | done (the heap model's FSM) |
| pg_visibility | 99 | 99 | done (the heap model's visibility map) |
| pgstattuple | 83 | 83 | done (counts from the size model) |
| dict_int | 63 | 63 | done |
| dict_xsyn | 29 | 29 | done |
| passwordcheck | 12 | 12 | done |
| auto_explain | 12 | 10 | see below |
| dblink | 218 | 218 | done |

### What is left, per module

- **Round 19 (agent 71), the modules added** (`src/ext/`): the TABLESAMPLE methods resolve to
  the extension's handler function (`binder::select` `tsm_method`; system_rows takes a row
  count, system_time milliseconds, neither REPEATABLE; `exec::scan::Sampler`). The inspection
  modules read the engine's size model (`exec::sizing`): pg_freespacemap the heap's simulated
  FSM and BRIN's regular pages (`recorded_free_space`); pg_visibility the heap model's
  visibility map (`visibility`; COPY FREEZE inserts frozen tuples, its new pages all-visible
  and all-frozen; pg_check_frozen / pg_check_visible find nothing, the model marking only
  pages none would break); pgstattuple its tuple and page counts (`heap_tuple_stats`,
  `Btree::page_stats`; hash indexes' bucket / bitmap split and GIN's pending list aren't
  modelled page by page: the shape an index of that size has); pg_prewarm reads nothing (no
  buffer pool) and reports the range's blocks; pgrowlocks the lock manager's row locks
  (`LockManager::row_lockers`, one member per locking (sub)transaction; no MultiXactIds: a
  multi's `locker` is its first member's xid), its isolation spec matching (12 / 12
  permutations, run from a private copy with CREATE EXTENSION in its setup). dict_int and
  dict_xsyn are extension text search templates (`ext/dicts.rs`). passwordcheck's hook runs
  in CREATE / ALTER ROLE ... PASSWORD once the library is loaded (`auth::passwordcheck`).
  LOAD of auto_explain, passwordcheck and auth_delay defines their parameters
  (`session/ext_gucs.tsv`, from the reference's pg_settings). sslinfo answers for a connection
  without SSL (the server declines SSLRequest). tcn is a native trigger (its spec matches);
  intagg installs, int_array_enum runs (an internal-language set-returning function runs its
  built-in), int_array_aggregate doesn't (below). Golden `a71_contrib_round3`.
- **tsm_system_time** (units 9, 11): PostgreSQL puts a Materialize over a sample scan whose
  method isn't repeatable across scans when the query level isn't one relation
  (set_tablesample_rel_pathlist); the engine's EXPLAIN doesn't (rows match).
- **auto_explain** (units 5, 10): ALTER SYSTEM is refused (the engine has no
  postgresql.auto.conf). Logging plans (the module's purpose) isn't done: the engine has no
  server log a client sees, and the plans would go there at LOG level.
- **intagg**: int_array_aggregate's transition function is the internal
  `array_agg_transfn` over an `internal` state, which the engine's aggregates don't take
  (no contrib units; the module has no regression test).
- **Not applicable**: pageinspect (above); pg_surgery (heap_force_kill / heap_force_freeze
  repair corrupted heap tuples by TID outside transactions: the engine's rows aren't
  PostgreSQL heap tuples, and a row is removed with DELETE ... WHERE ctid); pg_walinspect,
  basic_archive (PostgreSQL WAL records and segments; the engine's WAL is its own store's);
  pg_logicalinspect (logical decoding's snapshot files); sepgsql (SELinux).
- **Not done**: test_decoding (the engine has no logical decoding: no replication slots or
  change stream, which CDC tools such as Debezium need; a project of its own); bloom (an
  index access method: waits for the pluggable index AMs agent 70 is adding); xml2 (its
  xslt_process needs an XSLT processor; the xpath_* functions could use the engine's XPath);
  pg_overexplain, spi (refint, autoinc, insert_username, moddatetime), auth_delay's delay
  (its parameter only).

- **btree_gist** (3 units): rows of an unordered GiST index scan come back in PostgreSQL's
  GiST tree order (macaddr, macaddr8, bytea units without ORDER BY): the engine reads the
  index's rows in its own order. Accepted as an internal difference (round 19, agent 71):
  matching it needs a model of GiST's page splits for these classes (picksplit's order of the
  keys over the pages), and a query without ORDER BY has no defined order; the rows are the
  same.
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
  area). verify_heapam counts its pages in pg_stat_io's bulkread context (check_heap 27
  matches since round 19, agent 71: reads when SET TABLESPACE, a TRUNCATE or the table's
  creation gave it new storage in this transaction, which SET TABLESPACE now marks as
  ATExecSetTableSpace's RelationAssumeNewRelfilelocator does; hits otherwise). The
  check_gin timeout is gone: the text[] GIN build (10,000 rows) that stalled past 300 s takes
  about a second since round 19's index-build work.
- **pageinspect**: reads raw 8 kB PostgreSQL pages (heap, btree, GIN, GiST, BRIN, hash
  layouts, checksums). The engine's tables and indexes are key-value data, so there are no
  such pages to show; making the functions up would mislead. Not available
  (`CREATE EXTENSION pageinspect` fails as for an uninstalled extension).
- **dblink / postgres_fdw: outbound connections** (round 19, agent 59). Both run over the
  engine's own client (`src/remote/`: libpq's connection strings and option table, a
  blocking wire client on `postgres-protocol` with trust / cleartext / md5 / SCRAM, extended
  queries, asynchronous sends, notifications and cancel requests). No libpq, no C.
  Connections go only where the operator allows: `roll-engine --allow-outbound <list>`
  (`EngineOptions::outbound`, HANDOFF "Decisions"); without it every connection fails with
  libpq's "connection to server ... failed: outbound connections to this destination are not
  allowed by this server". A connection without a host (libpq's default Unix socket) is the
  engine itself (`local`); the contrib harness runs with `--allow-outbound local`.
  PostgreSQL's own rules stay on: non-superusers must give a password and the server must use
  it (dblink_connstr_check / dblink_security_check, postgres_fdw's check_conn_params /
  pgfdw_security_check, password_required); the engine's environment (PGPASSWORD, service
  files) is never read. Tests: `tests/outbound.rs`.
- **dblink**: done (`src/ext/dblink.rs`): named and unnamed connections, cursors, async
  queries, notifications, cancel, the remote session's DateStyle / IntervalStyle applied to
  results, the build_sql functions, dblink_fdw's validator, dblink_connect_u as its owner (a
  SECURITY DEFINER C function's `symbol#secdef` code, `native_def`).
- **postgres_fdw** (`src/ext/postgres_fdw/`): the validator (option.c), the connection cache
  per user mapping with remote transactions and savepoints following the local ones (commit,
  abort, RELEASE / ROLLBACK TO, PL/pgSQL exception blocks; PREPARE TRANSACTION refused),
  invalidation on ALTER SERVER / USER MAPPING, keep_connections, postgres_fdw_get_connections
  / disconnect / disconnect_all, scans through cursors fetched `fetch_size` rows at a time
  with shippable conditions sent (foreign_expr_walker's shippability and collation rules,
  deparse.c's text) and the rest checked locally, INSERT / UPDATE / DELETE per row by remote
  ctid, UPDATE / DELETE sent whole when PostgreSQL would (Foreign Update / Foreign Delete),
  constraints left to the remote server, ANALYZE sampling the remote table, IMPORT FOREIGN
  SCHEMA, EXPLAIN's Remote SQL. Left: join, aggregate, ORDER BY / LIMIT and FOR UPDATE
  pushdown (Foreign Scans of joins and upper relations: the planner's foreign paths and
  postgres_fdw's costs), use_remote_estimate, batch inserts, async append, TRUNCATE.
- **file_fdw** (round 19, agent 66; `src/ext/file_fdw.rs`): done. The validator (options by
  catalog with getClosestMatch's hint, the filename / program privileges, COPY's own option
  checks through `CopyFormat::parse`), scans read by COPY FROM's parser
  (`copy::parse_copy_data`, a 64 kB chunk at a time: header match, FORCE_NOT_NULL /
  FORCE_NULL from column options, DEFAULT markers, ENCODING, ON_ERROR ignore with its
  NOTICE and REJECT_LIMIT), estimate_size / estimate_costs from the file's size (rows by the
  scan's reltarget width), EXPLAIN's Foreign File / Foreign Program / Foreign File Size,
  ANALYZE sampling the file, `ctid` (4294967295,0) and tableoid only, and CheckValidResultRel's
  "cannot insert into / update / delete from / truncate foreign table" (`fdw_modify_error`).
  Files are the server's files: with `--allow-server-files <dir>` relative names are in
  `<dir>` and absolute ones must be inside it (as COPY FROM's); without it no file opens
  ("could not open file ...: Permission denied"). A `program` table is accepted where
  PostgreSQL accepts it but never runs ("could not execute command ...: Permission denied"),
  as COPY ... PROGRAM is refused. Tests: golden `a66_file_fdw`, `tests/server_files.rs`
  `file_fdw_tables`.
- **Query Identifier**: the contrib servers preload pg_stat_statements, which turns query ids
  on; the harness runs the other modules with `compute_query_id = off`, as their own test
  configurations have it (their EXPLAIN VERBOSE shows no Query Identifier).
- **pg_stat_statements** (`src/ext/pg_stat_statements/`, `session/pgss.rs`): the engine
  starts it with `--shared-preload-libraries pg_stat_statements` (EngineOptions
  `shared_preload_libraries`; the contrib harness passes it). Statements are grouped by a
  jumble of the parsed statement and the catalog's column types (constants and aliases
  ignored, PostgreSQL 18's squashed IN / ARRAY lists), texts normalized with `$n`; calls, rows,
  plans, toplevel / nested tracking, track_utility / track_planning, privileges, the info view
  and reset. The engine has no WAL or shared buffers: blocks written report table growth,
  wal_records rows written, wal_bytes 64 per row, other buffer columns 0; plan time is the
  engine's binding and planning time. Unreferenced CTEs in EXPLAIN match since round 19.
  Round 19 (agent 71) took the last 17 units: two-phase commit runs (the contrib servers,
  both, have `max_prepared_transactions = 5`, the harness passing
  `--max-prepared-transactions 5`); nested statements are counted where they run (PL/pgSQL
  PERFORM, a SQL function's statement run as a subquery, `FnShow::pgss`, set only with the
  library preloaded), EXPLAIN of DECLARE nests the cursor's query, CREATE TABLE AS EXECUTE the
  prepared one under its PREPARE entry, COPY's query takes the statement's location (no
  trailing `;`); CALL's constants take the procedure's parameter types (`jumble::Procs`: a
  NULL argument jumbles as the others do); an IN list's common type prefers oid over
  integers (select_common_type); a plan's Gather workers count as parallel_workers_to_launch
  / launched (`explain::gather_workers`, computed only for a tracked statement); an operator
  whose function is a user SQL function works over `= ANY (ARRAY[...])` (each element's
  comparison ORed, ALL ANDed: there is no function to call per element).
