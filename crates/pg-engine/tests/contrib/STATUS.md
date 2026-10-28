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

Last full run (round 19, agent 53, `tests/contrib/REPORT.md`): 4,153 / 4,158 units of 19 modules (99.9%), pg_stat_statements 996 / 1,013 (1,013 / 1,013 since round 19, agent 71: a run of that module). Round 19 (agent 59): dblink 218 / 218, postgres_fdw 1,664 / 2,053; agent 72: postgres_fdw 1,802 / 2,053 (87.8%). Round 20 (agent 83): postgres_fdw 1,856 / 2,053 (90.4%; dblink 218, file_fdw 187 unchanged). Round 19 (agent 66): file_fdw 187 / 187. Round 19 (agent 71), a full run of every module: 7,683 / 8,270 units of 34 modules (pageinspect not applicable, postgres_fdw 1,664 then); the 10 modules with tests added this round 346 / 350 (tsm_system_time 14 / 16, auto_explain 10 / 12, the rest whole); amcheck 203 / 204 in a run after it. Round 20 (agent 70): pgvector 915 / 915, bloom 58 / 58 (pg_trgm, btree_gist and cube unchanged at 261, 613 and 316). Baseline (before any
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
| postgres_fdw | 2053 | 1856 | see below |
| file_fdw | 187 | 187 | done (files are the server files of `--allow-server-files`; programs refused) |
| tsm_system_rows | 16 | 16 | done (round 19, agent 71) |
| tsm_system_time | 16 | 16 | done (round 20, agent 80a: the Materialize) |
| pg_prewarm | 5 | 5 | done (no buffer pool: nothing is read; the range's blocks are reported) |
| pg_freespacemap | 15 | 15 | done (the heap model's FSM) |
| pg_visibility | 99 | 99 | done (the heap model's visibility map) |
| pgstattuple | 83 | 83 | done (counts from the size model) |
| dict_int | 63 | 63 | done |
| dict_xsyn | 29 | 29 | done |
| passwordcheck | 12 | 12 | done |
| auto_explain | 12 | 12 | done (round 20, agent 80) |
| dblink | 218 | 218 | done |
| pgvector (0.8.7) | 915 | 915 | done (see below) |
| bloom | 58 | 58 | done (on the native index access method interface) |
| spi | 52 | 52 | done (round 20, agent 80a) |
| xml2 | 29 | 23 | XPath functions done; xslt_process as without libxslt (see below) |
| pg_overexplain | 37 | 36 | DEBUG / RANGE_TABLE done; internals' numbers (see below) |
| test_decoding | 1047 | 1024 | logical decoding (round 20, agents 79 and 87; see below) |

### test_decoding (round 20, agents 79 and 87)

Logical decoding (HANDOFF "Logical replication"). Its tests need the module's temp-config
(`logical.conf`: wal_level = logical, max_replication_slots = 4, logical_decoding_work_mem =
64kB): the harness now starts the engine with the `--temp-config` settings of the modules it
runs (`-c name=value`), and the reference must run with them too, so run the module on its
own with a reference started so (plus `max_prepared_transactions=5` for twophase):

```sh
docker run -d --name me-cref -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
  <pinned image> -c wal_level=logical -c max_replication_slots=4 -c logical_decoding_work_mem=64kB \
  -c max_prepared_transactions=5 -c autovacuum_naptime=1d
PGCONTRIB_ONLY=test_decoding PGREGRESS_CONTAINER=me-cref ... cargo test -p pg-engine --test pg_contrib -- --ignored --nocapture
```

1,024 / 1,047 (97.8%; round 20, agent 87, at 61214bbc; agent 79's commit measured 1,014): xact,
binary, prepared, time, messages, spill, truncate, permissions, slot, replorigin whole; ddl 251 /
252, stats 35 / 38. Agent 87 fixed: copy_replication_slot's (swapped, as in PostgreSQL) error
messages, a session origin's local position at every commit, include-rewrites (a table
rewrite's rows logged as rewrite inserts, decoded only for plugins asking), a rolled-back
prepared transaction over logical_decoding_work_mem decoded without its changes
(ReorderBufferCheckAndTruncateAbortedTXN), a user catalog table refusing a rewrite, MERGE ...
INSERT VALUES (s.*), and `-c` settings stored normalized (RESET logical_decoding_work_mem gave
0, so stats' spill counters stayed 0). What differs:
- **Streaming of rolled-back subtransactions** (stream 3, twophase_stream 2, decoding_in_xact
  1): PostgreSQL decodes a subtransaction's changes before it learns it rolled back, so a big
  one streams and then reports "aborting streamed (sub)transaction"; the engine's change
  records of a subtransaction go with its ROLLBACK TO, so a transaction is streamed only for
  what it committed.
- **Catalog-table features** (rewrite 7, toast 1): CLUSTER pg_class, `pg_relation_size` of a
  shared catalog's TOAST table, UPDATE of pg_attribute (permission).
- **Plan choices** (ddl 1, stats 3): the MERGE's join keeps the target table as the hashed
  side, so its rows come out in another order (PostgreSQL hashes the target there); three
  slot-creating function scans in one FROM are joined s3 first (PostgreSQL: s1, s2, s3), so
  the slots take other places in pg_replication_slots.
- **decoding_into_rel** (3): decoding into a table in the transaction that reads the slot.
- twophase (2): pg_locks rows of a prepared transaction (lock code).

End to end (agent 87, debug build, PostgreSQL 18.6 in a host-network container): a
PostgreSQL subscriber (`CREATE SUBSCRIPTION ... PUBLICATION p1` against the engine) copies the
tables (tablesync workers, all `r`) and then streams: PK and REPLICA IDENTITY FULL tables,
UPDATEs of the key, TRUNCATE, and about 6,000 small transactions from two concurrent sessions,
both sides identical (row counts, sums, md5 of every row). pg_recvlogical: --create-slot with
test_decoding, --start with plugin options writing a file (inserts, update, delete,
transactional message), stopped with SIGINT, the slot's confirmed position at the WAL end,
restarted from there with only the new transaction, --drop-slot.
- twophase (2): pg_locks rows of a prepared transaction (lock code).

### pgvector (round 20, agent 70)

Not a contrib module: pgvector 0.8.7's own suite (`test/sql`, `test/expected` at the v0.8.7
tag), an external module of the harness (`EXTERNAL_MODULES` in `tests/harness/regress.rs`: cloned
next to the contrib sources, run like `make installcheck` with `--load-extension=vector`). Its
reference is the pinned image with pgvector built from source as the official pgvector image
builds it (a default run leaves pgvector out when the reference doesn't have it):

```sh
git clone --branch v0.8.7 --depth 1 https://github.com/pgvector/pgvector.git ctx/pgvector
cat > ctx/Dockerfile <<'EOF2'
FROM postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
COPY pgvector /tmp/pgvector
RUN apk add --no-cache --virtual .build build-base && cd /tmp/pgvector \
 && make OPTFLAGS="" with_llvm=no && make install with_llvm=no && rm -rf /tmp/pgvector && apk del .build
EOF2
docker build -t <prefix>-pgvector:0.8.7 ctx
```

(`OPTFLAGS=""`: the official image's portable build, whose float sums the engine reproduces bit
for bit; `with_llvm=no`: the Alpine image has no clang for bitcode. Add `-DIVFFLAT_BENCH
-DHNSW_MEMORY` to OPTFLAGS for a reference whose index builds seed 42 first, what
`ROLL_VECTOR_BUILD_SEED=42` makes the engine do: HANDOFF "pgvector".)

Round 20: all 14 files match: the type files, and the ivfflat and hnsw ones (their small
tables give the same rows whatever the builds' random choices). Result-for-result checks of the
approximate indexes over bigger tables, against a reference built with `-DIVFFLAT_BENCH
-DHNSW_MEMORY`: `tests/pgvector_equality.rs` (HANDOFF "pgvector").

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
- **Round 20 (agent 80a), the contrib tail.** tsm_system_time: a sample scan whose method
  can't repeat its scans reads under a Materialize when it may run more than once, in a
  subquery planned apart or a query level of more than one relation, as
  set_tablesample_rel_pathlist has it (`explain.rs` `unrepeatable_samples`; EXPLAIN only, its
  costs not fed back to the planner). intagg: an aggregate whose transition and final functions
  are a built-in aggregate's internal ones over an `internal` state (`array_agg_transfn` /
  `array_agg_finalfn`, the array and string_agg variants), declared LANGUAGE internal by an
  extension script or named directly by CREATE AGGREGATE, runs as that aggregate
  (`binder::expr` `internal_agg_impl`, `UserAgg::internal`). spi (`ext/spi.rs`): autoinc,
  insert_username, moddatetime and refint's check_primary_key / check_foreign_key as native
  trigger functions; refint's queries run as SPI runs them (prepared with the key columns'
  types as `$n`, their errors with the query as CONTEXT, REFINT_VERBOSE's NOTICEs). xml2
  (`ext/xml2.rs`): xpath_string / number / bool / nodeset / list, xml_encode_special_chars
  and xpath_table over the engine's libxml model (documents read with XML_PARSE_NOENT under
  legacy strictness, so a malformed one gives NULL; nodes dumped as xmlNodeDump does;
  "unsupported XQuery result" NOTICEs). Golden `a80a_contrib_tail`.
- **pg_overexplain** (round 20, agent 80a; `sql/explain/overexplain.rs`): LOAD
  'pg_overexplain' gives EXPLAIN its DEBUG and RANGE_TABLE options (before it, both are
  unrecognized). DEBUG: per node Disabled Nodes, Parallel Safe, Plan Node ID (set_plan_refs'
  preorder, subplans after the main tree; extParam / allParam "none"), and the PlannedStmt
  section. RANGE_TABLE: Scan / Append / Nominal RTIs and the range table, numbered as
  PostgreSQL numbers a plan of that shape (the level's relations in FROM order, its GROUP
  entry, the partitions its scans read). An Append shows "Subplans Removed" in structured
  formats as ExplainMissingMembers does. These are planner internals the engine models from
  its plan, not a copy of PostgreSQL's range table: a query with sub-selects, joins written
  with JOIN, CTEs or set operations may number differently. Executor Parameter Types lists the
  EvalPlanQual parameters of ModifyTable and LockRows (type 0); nested loop parameters aren't
  modelled. Left: unit 30 (the nested loop over partition index scans the engine doesn't plan
  with enable_seqscan off; extParam / allParam and the parameter types of a parameterized
  scan). Nothing runs unless the library is loaded. Golden `a80a_overexplain`.
- **xml2's xslt_process** (6 units): fails as in a PostgreSQL built without libxslt
  (`xslt_process() is not available without libxslt`, the module's own `xml2_1.out`). Not
  written: matching libxslt means an XSLT 1.0 processor with its exact behaviour, not a
  subset: template conflict resolution and built-in rules, xsl:number / sort / key /
  format-number, the extension elements and EXSLT functions libxslt registers (saxon:output
  is the module test's security case), xsltSaveResultTo's serialization (declaration, indent,
  method html / text), and its error reporting. A subset that passes the module's four
  stylesheets would misbehave on the next stylesheet an application sends; this is a module
  in its own right.
- **auto_explain**: done (round 20, agent 80). ALTER SYSTEM writes postgresql.auto.conf
  (`src/sysconf.rs`), so units 5 and 10 match. The plans go to the server log
  (`src/log.rs`; `roll-engine` writes it to stderr) and to the client past
  client_min_messages, as explain_ExecutorEnd lays them out (`ext/auto_explain.rs`): the
  module's TAP test (t/001_auto_explain.pl) is `tests/server_log.rs`. Nested statements
  (log_nested_statements) and log_triggers aren't explained yet.
- **auth_delay**: done (round 20, agent 80): preloaded (shared_preload_libraries, `-c` or
  postgresql.auto.conf), a failed authentication waits auth_delay.milliseconds before its
  error (`server.rs` `auth_delay`). The module has no regression test.
- **intagg**: done (round 20, agent 80a; no contrib units, the module has no regression
  test; golden `a80a_contrib_tail`).
- **Not applicable**: pageinspect (above); pg_surgery (heap_force_kill / heap_force_freeze
  repair corrupted heap tuples by TID outside transactions: the engine's rows aren't
  PostgreSQL heap tuples, and a row is removed with DELETE ... WHERE ctid); pg_walinspect,
  basic_archive (PostgreSQL WAL records and segments; the engine's WAL is its own store's);
  pg_logicalinspect (logical decoding's snapshot files); sepgsql (SELinux).

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
  SCHEMA, EXPLAIN's Remote SQL. Agent 72: each remote INSERT / UPDATE / DELETE returns what
  deparseReturningList asks for (the RETURNING list's columns and ctid, the views' check
  options' columns, every column under an AFTER ROW trigger) and the stored row is the one
  the server returned (remote triggers' changes reach RETURNING, AFTER triggers and the
  check options, which run on it, a foreign partition's too); an UPDATE under a BEFORE ROW
  UPDATE trigger sends every column, generated ones as DEFAULT; non-direct UPDATEs (and
  DELETEs under row triggers) read the target's whole row; direct modification runs with a
  WHERE, statement triggers and RETURNING tableoid; inheritance members' "Foreign Update" /
  "Foreign Delete" lines with their Remote SQL; a foreign row's key names its table, so an
  inheritance UPDATE / DELETE finds its foreign member. Also agent 72: TRUNCATE of foreign
  tables, a server's in one remote TRUNCATE (`truncatable`, restrict_nonsystem_relation_kind);
  SELECT ... FOR UPDATE / SHARE of foreign tables only is locked by the remote query, with
  no LockRows (ROW_MARK_COPY, `pushdown::remote_lock`); EXPLAIN's Async Foreign Scan under an
  Append (`async_capable`; the executor still reads members in turn), Batch Size from
  `batch_size` (rows still go one statement each); a scan of a table with a dropped column
  returns no physical target list (build_physical_tlist punts); rows routed or inherited into
  foreign tables under transition-table triggers fail as PostgreSQL fails them.
  Round 20 (agent 83, postgres_fdw 1,846 / 2,053): joins and aggregations run remotely.
  The binder records PostgreSQL's range table indexes (`ScanPlan::rtindex`: FROM items, joins
  and subqueries in parse order, RTE_GROUP, then pulled-up sublinks' and subqueries'
  relations), which the remote SQL's `rN` aliases are. The join search offers each join
  relation of one server's tables to the wrapper once, from its first pair of inputs
  (`paths/joinpath/foreign.rs`, GetForeignJoinPaths); foreign_join_ok decides it
  (`ext/postgres_fdw/join.rs`: inner, left, full and semi joins, same server and user, no
  local conditions below, PlaceHolderVars and semi join targets checked; join clauses vs.
  remote conditions as postgres_fdw pulls them up), its Foreign Scan path costed by
  estimate_path_cost_size (base relations too: fdw_startup_cost, fdw_tuple_cost, remote
  EXPLAINs under use_remote_estimate, whose parameterized paths' equivalence clauses fix the
  clauses' order). `Plan::Foreign` (`ext/fdw.rs`, the wrapper's `ForeignRelOps`) is the Foreign
  Scan of a join or upper relation: EXPLAIN's Relations line and Remote SQL, a cursor read
  into the join row, local conditions as its Filter. Deparsing (`joinsql.rs`) follows
  deparseFromExprForRel: `(r1 INNER JOIN r2 ON (...))`, FULL JOIN sides with conditions as
  `sN(c1, ...)` subqueries, semi joins as EXISTS. An outer join's side planned as a group of its
  own that is a foreign join is taken into the join above (`fdw_adopt`). After a level is
  planned, `pushdown.rs` sends WHERE clauses over an outer join, the target list's order, the
  ORDER BY / LIMIT over a foreign join, and aggregations (`upperpath.rs`,
  foreign_grouping_ok: the grouped target list, GROUP BY by position, HAVING split into sent
  and local, aggregates with DISTINCT, ORDER BY, FILTER, WITHIN GROUP, VARIADIC; also over a
  join the search didn't choose remotely, `JoinMeta::fdw_alt`), with their ORDER BY / LIMIT.
  Second batch (1,856 / 2,053): a grouped scan's resjunk GROUP BY columns in EXPLAIN, the
  LIMIT over an ORDER BY sent, aggregates' VARIADIC; batched remote INSERTs are real
  (`modify::insert` keeps a batch_size's rows and sends one `INSERT ... VALUES (...), (...)`,
  flushed when full, at the end of the data-modifying node and before any BEFORE ROW trigger,
  as ExecPendingInserts; dropped on abort); Async Foreign Scans are real: an Append returns its
  synchronous members' rows first while the first async member's FETCH is in flight
  (`scan::async_order`, `Node::prefetch`, `Cursor::prefetch`; a command that needs the
  connection first reads the pending result and keeps it for its scan, process_pending_request);
  a cursor's DECLARE goes with its first FETCH in one message (a round trip fewer per scan).
  Left: join paths with pathkeys (merge joins over sorted remote joins), FOR UPDATE over
  joins (whole-row ROW() columns), parameterized foreign paths, some join orders of three or
  more relations, async members after the first fetched one at a time (PostgreSQL's
  round-robin of requests per connection), COPY into foreign tables, parallel_commit /
  parallel_abort, analyze_sampling, query_cancel's timeout unit.
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
