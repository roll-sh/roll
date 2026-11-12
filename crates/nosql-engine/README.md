# nosql-engine

The CQL engine behind roll nosql. One `Database` is one tenant's database: schema, storage
tables, prepared statements, functions, system tables and hooks. `nosql-server` speaks the native
protocol and calls the facade; this crate never touches the network.

The goal is **1:1 client-visible behaviour** with `scylladb/scylla:2026.3.3` and `cassandra:5.0.9`.
Each database has a `Dialect`. Every message, result column name, bind marker name, warning and
`system_schema` row should match the reference of its dialect. Where the two differ, branch on
`Dialect`, never on a guess. Read `docs/nosql/contracts.md` first.

## Request path

```
Session::query / prepare / execute / batch
  → nosql_cql::parse_statement(text, dialect)       (session.rs)
  → statements::prepare  → Prepared { plan, bind specs, pk indexes, result metadata }
       select::prepare / modify::prepare / batch::prepare / DDL is planned at execution
  → cached per database, keyed by MD5(keyspace + text)   (database.rs, statements::statement_id)
  → statements::execute  → hooks.authorizer → plan execution → Response
```

Prepared plans are owned and typed (no syntax tree at execution). Values stay serialised: terms
evaluate to `Bytes`, rows are assembled from storage cells and handed to the server as bytes.
A schema change installs a new immutable `Arc<Schema>` and drops prepared statements on changed
tables; executing one returns `Unprepared` so drivers re-prepare.

## Layout

| path | what |
| --- | --- |
| `database.rs` | `Database`: open (schema reloaded from storage), sessions, prepared cache, events, partition locks, clock |
| `session.rs` | `Session`, `SessionState` (everything per connection), parse error mapping, value ordering for named values |
| `response.rs` | facade types: `QueryParams`, `Param`, `Response`, `Rows`, `ResultMetadata`, `Prepared`, `Batch`, `Event` |
| `error.rs` | `Error` (code, message, typed `ErrorDetail`, warnings), `ErrorCode`, `Consistency` |
| `messages.rs` | **every** dialect-dependent message text; add new ones here, with a unit test |
| `config.rs` | `DbConfig` (identity, cluster facts) and `Settings` (thresholds per dialect) |
| `limits.rs` | protocol and schema limits |
| `live.rs` | what changes while open: `set_settings` / `settings()` (statements read the live settings; `RELOAD_SETTINGS` need a reload), `set_read_only` (enforced by `hooks::ReadOnly`, which wraps every database's authorizer), `recent_warnings` |
| `schema/mod.rs` | `Schema`, `Keyspace`, `Table` (dense `u32` column ids, canonical column order), `Column` |
| `schema/ddl.rs` | CREATE/ALTER/DROP KEYSPACE, TABLE, TYPE, TRUNCATE; `commit` writes the schema mutations |
| `schema/params.rs` | table options: defaults, validation, ScyllaDB extensions (`tombstone_gc`, `cdc`, …) |
| `schema/replication.rs` | replication strategies and their warnings (tablets in ScyllaDB) |
| `schema/persist.rs` | the schema as rows of real `system_schema` tables, and loading it back |
| `schema/types.rs` | resolving AST types to `CqlType` (UDTs of the keyspace) |
| `system/schema_tables.rs` | exact `system_schema` table definitions per dialect |
| `system/local.rs` | `system.local`, `peers`, `peers_v2`, `size_estimates`, `scylla_local`, `versions` |
| `system/mod.rs` | built-in keyspaces, table definition helpers, the `VirtualTable` registry |
| `system/catalog.rs` | every built-in table's columns and options, ScyllaDB's system types, Cassandra's virtual keyspaces (data generated from the reference catalogs: `catalog_data.rs`) |
| `system/views.rs` | Cassandra `system_views.*`, `system_virtual_schema.*`, `system.prepared_statements`; ScyllaDB `system.config`, `clients`, `runtime_info`, `token_ring`, `tablets`, topology tables (fixed rows: `rows_data.rs`) |
| `system/clients.rs` | the connection registry (`Database::register_client`) behind `system.clients` / `system_views.clients` |
| `describe/` | server-side `DESCRIBE` (Cassandra `DescribeStatement` port, ScyllaDB's forms, roles/grants/service levels), reusable as `describe::{schema, keyspace, table, …}` |
| `audit.rs` | ScyllaDB's audit logging: categories, the live filters (`audit_categories`, `audit_keyspaces`, `audit_tables`, `audit_rules` with fnmatch patterns), rows of CQL statements and logins in `audit.audit_log`, the API Alternator audits through |
| `trace/` | request tracing into `system_traces` (`Session::set_tracing`), trace probability, ScyllaDB's slow query log |
| `prepare/terms.rs` | typing terms and bind markers (`Constants.testAssignment`), literals, casts, function calls |
| `prepare/restrictions.rs` | WHERE analysis (port of Cassandra `StatementRestrictions`, ScyllaDB differences) and its evaluation into keys, slices and row filters |
| `prepare/selection.rs` | selectors, column names, aggregates, JSON rows, writetime/ttl |
| `statements/select.rs` | SELECT execution: partition/range/virtual sources, paging, GROUP BY, post-ordering |
| `statements/modify.rs` | INSERT/UPDATE/DELETE: updates, collections, counters, read-before-write, typed `IF` conditions |
| `statements/batch.rs` | CQL and protocol batches, size warnings, atomic apply |
| `statements/paging.rs` | the opaque paging state |
| `statements/read_warn.rs` | Cassandra's tombstone warning: `ReadCommand.toCQLString` and the read commands a SELECT becomes, counted from storage's `ReadTrace` |
| `functions/` | `Registry` (Cassandra `FunctionResolver` semantics), native, time, arithmetic, aggregates; Cassandra 5's math (`bigdec.rs`: `BigDecimalMath`'s decimal `exp`/`log`), collection, masking and `floor` functions through `params.rs` (`FunctionFactory`); ScyllaDB's `set_intersection` |
| `masking/` | dynamic data masking: column masks (DDL, `system_schema.column_masks`, read-time application, `SELECT_MASKED`), the platform policy (`DbConfig.masking`, `Database::set_masking`) |
| `storage.rs` | mapping between CQL values and `nosql-storage` cells (collection paths, UDT fields) |
| `hooks.rs` | extension points for the other subsystems; `ReadOnly` (read-only databases: only `Access::reads_only` / `hooks::reads_only` statements pass) |
| `auth/` | roles, permissions, PasswordAuthenticator, CassandraAuthorizer, identities, ScyllaDB service levels; the auth tables (`system_auth.*` / ScyllaDB `system.roles` …) |
| `lwt/` | lightweight transactions: `IF` preparation and evaluation, conditional statements and batches, `SERIAL` reads |
| `udf/` | user functions and aggregates: `CREATE/DROP FUNCTION` and `AGGREGATE`, the `UserFunctions` provider, ScyllaDB's Lua UDFs on Lua 5.4 (`nosql-lua`: ScyllaDB's libraries, type mapping, limits and messages) and WASM UDFs on `wasmi` (`wasm.rs`: ScyllaDB's ABI, fuel-bounded calls); Cassandra's Java UDFs are stored, not run |
| `index/` | secondary indexes, SAI, SASI, ScyllaDB local, vector and full-text indexes: `CREATE/DROP INDEX` (`ddl.rs`; ScyllaDB's view properties on an index apply to its `<index>_index` view), the `IndexPlanner` (`planner.rs`), index reads (`scan.rs`), SASI options, analyzers and `LIKE` (`sasi.rs`), exact ANN search (`vector.rs`), ScyllaDB vector index targets (local partition, filtering columns) and options (`vector_ddl.rs`), `BM25()` search validation (`fulltext.rs`: the reference has no Vector Store and answers "Vector Store is disabled", so do we); index tables use ScyllaDB's index view layout (in the schema as `<index>_index` in ScyllaDB, hidden storage tables in Cassandra) |
| `views/` | materialized views (`ddl.rs`) and the write observer that maintains views, index tables and CDC logs atomically with the base write (`observer.rs`: the read before the write includes the stored tombstones, so a row they shadow gets no entry; `update.rs`: port of Cassandra `ViewUpdateGenerator`), backfill (`views::build`), build status tables (`status.rs`) |
| `cdc/` | ScyllaDB CDC: the `<t>_scylla_cdc_log` schema (it follows its base while CDC is on, records the columns it loses as dropped, and refuses column changes of its own), log rows per write (`log_updates`), stream generations and their system tables (`tables.rs`) |
| `cpp.rs` | libstdc++ behaviours ScyllaDB's answers depend on: `std::unordered_map` iteration order (dropped columns in `DESCRIBE`, index options as stored and validated) |
| `tests/` | the in-crate harness and suites (see Testing) |

## Extension points (other subsystems)

Everything outside the core plugs in through `Hooks` (`hooks.rs`); `hooks::install` is the one
place each subsystem adds a line.

| subsystem | hook |
| --- | --- |
| LWT / Paxos | Implemented in `lwt/` (`Hooks.conditional`). It gets `ConditionalRequest { members: [(plan, values)], params, batch }` for every statement or batch with an `IF`; `statements::modify` calls `lwt::prepare` to type the `IF` clause (`Conditions::{NotExists, Exists, List}`) and `lwt::arith` for `c = c ± v`; `lwt::prepared_result` is ScyllaDB's `[applied]` PREPARED metadata, `lwt::check_serial_read` the `SERIAL` read rule. |
| Secondary indexes, SAI, vector search | Done (`index/`). `Hooks.index` (`IndexPlanner`: `supports`, `needs_filtering`, `plan` → an `IndexScan` that `open`s a page of base rows and carries its resume position in the paging state). |
| Materialized views, CDC | Done (`views/`, `cdc/`). `Hooks.observers` (`WriteObserver::on_write` gets every write of a statement or batch and returns `Derived { updates, guards }`, applied atomically with the base writes while the guards are held; `barrier()` waits for writes in flight before a backfill). `Table.view` holds view metadata (`computed` columns for index views). |
| Auth | Done (`auth/`). Other subsystems authorize their statements with `Access::Schema` statement names or `Access::Permission { permission, resource }` (table in `auth/authorizer.rs`); schema changes they return get creator grants / cleanup through `Authorizer::schema_changed`. Masking asks `auth::can_unmask` / `can_select_masked`; DESCRIBE reads `auth::snapshot`. Statements run by a `StatementHandler` (and core DDL) also pass `Authorizer::authorize_statement` first (the read-only guard uses it); read settings through `db.settings()`, not `db.cfg.settings`, so they apply live. |
| DESCRIBE, tracing, more virtual tables | Done (`describe/`, `trace/`, `system/{catalog,views,clients}.rs`). Generated tables: `system::Registry::add(ks, table, Arc<dyn VirtualTable>)`; every built-in table's columns and options are already in `system/catalog.rs` (a definition elsewhere overrides columns and kind). `DESCRIBE` renders whatever the schema holds: indexes (`IndexMeta`, ScyllaDB's `<index>_index` view options), views (`Table.view`), functions/aggregates (`Keyspace.functions` / `aggregates`, any language), CDC logs (`<t>_scylla_cdc_log`) and column masks (`Table.masks`). |
| UDF / UDA (Lua) | Done (`udf/`): its provider is installed at open (`Database::set_user_functions`); resolution and messages are shared with native functions. Plans record the user functions they call (`PrepareCtx::record_call` → `SelectPlan/ModifyPlan.functions`) for `EXECUTE` checks and prepared-statement eviction. |

A `StatementHandler` claims statements by kind (`handles`), and runs instead of the core. A
statement nobody handles returns `"<Kind> statements are not supported yet"`.

### Derived data for Alternator

`nosql-alternator`'s `EngineBackend` builds on these pieces, as ScyllaDB does:

- **Tables** are built as `Table`s and committed with `schema::ddl::commit` under
  `Database::lock_ddl` (Alternator names aren't CQL identifiers); items are written with
  `statements::modify::apply_with_observers` under `Database::lock_partition`.
- **GSIs and LSIs** are materialized views (`T:gsi`, `T!:lsi`) whose non-key index attributes
  are computed columns `{"type":"alternator_extract_from_attrs","attr_name":..,"desired_type":..}`:
  `views::spec::Extract` / `KeySource::Extracted` take the `:attrs` element (when it has the
  type's tag) as the view key, its cell's timestamp deciding entry liveness. `views::build`
  backfills them, the observer maintains them, and they are read as ordinary tables.
- **Streams** are the CDC log (`cdc = {'delta': 'keys', 'preimage': .., 'postimage': .., …}`);
  a tablets keyspace has `cdc::tablet_count(initial)` streams (`Streams::tablets`), and a
  partition deletion in a table without clustering columns pre-images the row it deletes.

## How to add …

**A statement.** Add the plan to `statements/` with a `prepare` (names, types, bind specs,
restrictions, errors in the reference's order) and an `execute`. Register both in
`statements/mod.rs` (`prepare` match, `Plan`, `execute`) and the `Access` it needs. Keep the
`execute` future `Send` (box it, as `select::execute` does).

**A function.** Implement `Function` (or use `Native::new` with a closure) and `Registry::add` it
in `functions/mod.rs`. Overload resolution, "none of its type signatures match" and ambiguity
errors come for free. Arguments whose types depend on the call (`token`, `toJson`,
`similarity_*`) are a `Factory` (`Registry::add_factory`). Aggregates implement `Aggregate`.
Name the Cassandra class you port in the module doc.

**A system or virtual table.** Every table of the references' built-in keyspaces is already
defined (`system/catalog.rs`, regenerated by `scripts/gen_system_catalog.py`); implement
`VirtualTable::rows` and register it (`system/views.rs` for small ones). A table without rows
reads empty. `system_schema` itself is stored; the built-in keyspaces' rows are generated and
overlaid. Cassandra's virtual keyspaces list in `system_virtual_schema` only and order
partitions by key (`LocalPartitioner`).

**A dialect message.** Add a function to `messages.rs` taking `Dialect`, with both texts copied
from the references (`docs/nosql/reference/errors.md`, `~/.cache/roll-nosql/capture/out`), and a
test. Never inline a message whose wording differs between dialects.

**An index type.** Add the `kind`/`options` it stores to `IndexMeta` rows (`schema/persist.rs`
writes `system_schema.indexes`), plan reads in an `IndexPlanner`, maintain it with a
`WriteObserver`, and handle `CREATE INDEX ... USING '<class>'` in a `StatementHandler`.

## Testing

`~/.cache/roll-nosql/cargo.sh test -p nosql-engine` (and `clippy --all-targets -p nosql-engine
-- -D warnings`). Tests run on temp-dir storage with small memtables.

- `tests/captured_*` replay the reference captures (`docs/nosql/reference/capture/*.py`, output
  in `~/.cache/roll-nosql/capture/out`) on both dialects and compare as text:
  - `captured_cases` — ~500 error/warning cases (`errors.py`): code, message, result kind,
    warnings;
  - `captured_statements` — results of `statements.py`: column names, driver type names, rows
    (rendered like the Python driver), paging, schema-change bodies, PREPARED metadata
    (bind names, partition key indexes);
  - `captured_schema` — the sample schema of `catalog.py`: DDL outcomes and the
    `system_schema` rows, also after reopening the database.
  - `captured_lwt` — the LWT capture (`lwt.py`, which runs inside `nosql-ref-cassandra` with
    cqlsh's driver against both references; see its header): conditional statements and
    batches, consistencies, bound values, protocol batches, PREPARE metadata.
  - `captured_describe` — every `DESCRIBE` of `catalog.py` (default images, and with UDFs,
    views and auth), byte for byte; rows of objects other subsystems haven't created yet are
    skipped, known differences are listed in `KNOWN`.
  - `captured_udf` — the UDF-enabled references' answers (`udf_live.py` / `udf_cases.py`, run on
    the host against `nosql-ref-scylla-auth` / `nosql-ref-cassandra-auth`): Lua conversions,
    errors, limits, UDF/UDA DDL of both dialects; roll's deliberate differences (Java isn't run)
    are in the harness's `ROLL_ANSWERS`.
  - `captured_derived` — the derived data capture (`derived.py`): index, SAI, vector, view and
    CDC DDL, `system_schema` rows, query results with every page, the log rows of every write
    kind.
  - `captured_masking` — dynamic data masking on the feature-enabled references
    (`masking_live.py` + `masking_cases.py`, run against `nosql-ref-cassandra-auth` /
    `nosql-ref-scylla-auth`): masked DDL and `column_masks` rows, DESCRIBE, reads per role and
    permission, restrictions, LWT rows, views, paging.
  - `captured_views` — Cassandra materialized views and SASI, live on the feature-enabled
    reference (`views_live.py` against `nosql-ref-cassandra-auth`): view DDL and every error,
    `system_schema` rows, maintenance per write kind, reads and paging, backfill; SASI options,
    modes, analyzers, `LIKE` forms and queries.
  Regenerate them with `scripts/gen_*.py` (run with `python3 -I`). Cases owned by other
  subsystems are listed with the reason in each harness's `NOT_IN_CORE`.
- `tests/{select,dml,ddl,system,errors}.rs` — behaviour the captures can't show: paging
  continuation, prepared execution, invalidation, events, persistence, TTL, batches.
- `tests/derived_probed.rs` — derived data cases probed on both references with the Python
  driver: view maintenance against stored tombstones and empty keys, view and index DDL
  validation and properties, CDC log ALTER / DESCRIBE / reattach, vector index targets,
  options and queries, `fulltext_index` DDL and `BM25()` queries.
- `tests/derived.rs` — index and view maintenance across write kinds, batches and LWT, prepared
  paging through indexes, backfill of flushed data, Cassandra views, persistence.
- `tests/tombstone_warn.rs` — Cassandra's tombstone warning replayed from `captured_tombstones.rs`
  (`docs/nosql/reference/capture/tombstones.py`, `scripts/gen_tombstones.py`): every page of 84
  query shapes, byte for byte, on memtables and SSTables.
- `tests/{describe,trace,system_views}.rs` — DESCRIBE paging, auth rows, name resolution;
  tracing rows per dialect; the generated system tables.
- `tests/probed.rs` — statements probed on both references with the Python driver (the
  conformance runner's image), each with its outcome per dialect (`probed_harness.rs`: rows,
  errors, warnings, prepared executions): WHERE-clause semantics, nulls and unsets, selectors,
  functions, conditional statements, schema rules behind the cqlpy categories of
  `docs/nosql/conformance-triage.md`.

To check something new, ask the references (never write to their system keyspaces; use `eng_`
keyspaces and drop them): `docker exec nosql-ref-scylla cqlsh -e "..."`,
`docker exec nosql-ref-cassandra cqlsh -e "..."`.
