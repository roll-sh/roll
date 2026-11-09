# roll nosql — conformance triage

This is the first full run of ScyllaDB's `test/cqlpy` suite (pinned to `scylla-2026.3.3`) against
roll in both dialects, compared with the reference baselines. It measures; it fixes nothing.

- **When.** 2026-10-20, on the working tree of `feature/serverless-nosql-database`. Indexes/views/CDC
  and UDFs/functions/masking were still being written, so their numbers will move first.
- **How.**

  ```sh
  CARGO=~/.cache/roll-nosql/cargo.sh ROLL_CONFORMANCE_TIMEOUT=300 \
    scripts/nosql-conformance.sh --roll scylla --compare crates/nosql-server/tests/conformance/reference-scylla-cqlpy.txt
  # the same with --roll cassandra and reference-cassandra-cqlpy.txt
  ```

  The cassandra run's two disruptive tests ran in separate invocations (`-- <test id>`), because
  the script was edited by another agent while the main run was in progress.
- **Baselines.** Every reference-passing test that roll does not pass is listed, with its tag
  from this document, in `crates/nosql-server/tests/conformance/roll-known-failures-scylla.txt`
  (743) and `roll-known-failures-cassandra.txt` (138). `--compare` now fails only on tests that
  are not in those lists, and prints listed tests that pass now. The lists may only shrink.

## Totals

| dialect | PASS | FAIL | ERROR | SKIP | XFAIL | reference PASS | reference PASS that roll passes | regressions (known failures) | run time |
|---|---|---|---|---|---|---|---|---|---|
| scylla | 1271 | 687 | 115 | 272 | 187 | 2009 | 1266 (63%) | 743 (634 FAIL, 109 ERROR) | 2159 s |
| cassandra | 1087 | 315 | 0 | 980 | 150 | 1214 | 1076 (89%) | 138 (all FAIL) | 612 s |

Each run has 2532 tests. ERROR means a fixture failed. Most of these fixtures create a table with
`large_data_guardrails_enabled`, a `fulltext_index`, or a UDF.

Other differences from the reference:

- **Decided: roll doesn't copy ScyllaDB bugs the suite marks as bugs.** The runner records a
  strict XPASS as `XPASS` (not FAIL), and `--compare` accepts it where the reference XFAILs the
  test, listing those tests apart (48 in the scylla run of 22:00).
- **scylla dialect, 35 strict XPASS (first run).** These tests are marked xfail for a ScyllaDB bug.
  ScyllaDB fails them, and roll passes them, so pytest reports FAIL. Most are on the Cassandra
  side of a ScyllaDB-vs-Cassandra difference:
  - 64 KiB keys and elements (`test_key_length.py::test_insert_65535_*`,
    `collections_test.py::test*WithElementsBiggerThan64K`)
  - filtering and index paging with LIMIT (`test_secondary_index.py::test_filter_and_limit*`,
    `test_paging_and_create_index*`)
  - `test_group_by.py::test_group_by_static_column`
  - `json_test.py::testInsertJsonSyntaxWithNonNativeMapKeys`
  - `test_type_timestamp.py::test_type_timestamp_overflow`

  "1:1" means bug-compatible, so the scylla dialect should reproduce these ScyllaDB behaviours,
  gated on `Dialect`. `--compare` does not gate them yet, because the reference does not PASS
  them. Someone needs to decide whether roll matches ScyllaDB here.
- **Tests roll passes and the reference does not.** 5 in the scylla dialect and 11 in the
  cassandra dialect. They are FAIL->PASS or SKIP->PASS and need no action.

## Crashes, hangs and server errors (top priority)

- **No crashes or panics.** roll stayed up through both runs. `roll.log` has no `panicked at`,
  and the script now fails the run if one appears.
- **No server hangs. Five tests ran into the 300 s timeout:**
  - `test_ttl_row.py::test_row_ttl_expiration_composite`
  - `::test_row_ttl_expiration_views`
  - `::test_quoted_ttl_column_expiration`
  - `::test_row_ttl_expiration_cdc_event[True]`
  - `::test_row_ttl_expiration_cdc_event[False]`

  Each test polls until rows expire through ScyllaDB's per-row TTL column. roll never expires
  them (see `rowttl`). Together they took 25 minutes of the scylla run. roll kept answering other
  clients throughout. With the default 900 s timeout they would take 75 minutes, which breaks
  the CI job's 90-minute limit. For that reason the CI conformance job sets
  `ROLL_CONFORMANCE_TIMEOUT=300`. Revert it to the default once `rowttl` is fixed.
- **Server error (code 0000) when re-creating a dropped index or view (`dropped_id`, 8 tests).**
  The engine derives an index's backing-table id deterministically from its name, so the new
  table gets the same id as the dropped one. The storage layer refuses to reuse that id. The
  failed CREATE also leaves partial state behind: a later `CREATE INDEX IF NOT EXISTS` succeeds
  without creating anything. Reproduction (cassandra dialect, suite config):

  ```sql
  CREATE TABLE ks.t (p int PRIMARY KEY, v int);
  CREATE INDEX ON ks.t (v);
  DROP INDEX ks.t_v_idx;
  CREATE INDEX ON ks.t (v);              -- ServerError: table 258ba64d-...-181191b2a3dc was dropped
  CREATE INDEX IF NOT EXISTS ON ks.t (v); -- OK, but nothing is created
  ```

  The same error happens for `CREATE MATERIALIZED VIEW ... WITH ID = <previously dropped id>`.
  Representative tests:
  - `test_secondary_index.py::test_create_index_if_not_exists`
  - `secondary_index_test.py::testCreateIndexWithQuotedColumnNames`
  - `test_materialized_view.py::test_creating_mv_with_given_id`

  Modules: `nosql-engine` `index/ddl.rs`, `views/ddl.rs`, and the table-id handling in
  `storage.rs`. A new index needs a fresh id (ScyllaDB and Cassandra use a timeuuid), or the
  storage layer must allow an id to be reused after a drop.
- **Silent wrong results.** These return data instead of an error, so clients cannot notice:
  - `counter_batch`: three `c = c + 1` updates to one counter in one `COUNTER BATCH` leave
    `c = 1`.
  - `paging_groupby`: `GROUP BY ... PER PARTITION LIMIT` counts rows instead of groups.
  - `filtering`: `NOT IN` filtering returns the rows it should exclude.
  - `frozen`: a frozen set sent out of order is not normalized, so it never matches.

## Root causes owned by in-progress subsystems

Counts are regressions as scylla / cassandra. They are ordered by total.

| tag | count | owner | what fails | module |
|---|---|---|---|---|
| `udf` | 73 / 5 | UDF/functions | `CREATE FUNCTION/AGGREGATE` returned "CreateFunction statements are not supported yet" at run time. That also broke `DESC FUNCTION/AGGREGATE`, permissions on functions, and UDT arguments (`test_udf_with_udt`, `uf_types_test.py::test_types_with_and_without_nulls`). | `nosql-engine/src/udf/` |
| `fulltext` | 71 / 0 | indexes | **Fixed** (see [Fixes](#fixes)). ScyllaDB's `USING 'fulltext_index'` gets "Non-supported custom class 'fulltext_index' provided". BM25 and analyzer options are missing too. | `index/ddl.rs`, `index/fulltext.rs` |
| `similarity_fn` | 30 / 27 | UDF/functions | **Fixed** (see [Fixes](#fixes)). The `similarity_cosine/euclidean/dot_product` functions: "Unknown function" with bind markers (expected "Cannot infer type of argument ?"); generic type errors instead of "Function system.similarity_X requires a float vector argument"; and the arity error text. | `functions/native.rs`, `prepare/` |
| `vector_index` | 34 / 3 | indexes | **Fixed** (see [Fixes](#fixes)). Validation of vector/SAI indexes: options (`source_model`, `index_version`, numeric ranges), local and multi-column vector indexes ("Only CUSTOM indexes support multiple columns"), duplicate indexes, the errors for ANN ordering without an index, and DESCRIBE. | `index/vector.rs`, `index/vector_ddl.rs`, `index/ddl.rs` |
| `index_props` | 32 / 0 | indexes | **Fixed** but 3 (see [Fixes](#fixes)). `CREATE INDEX ... WITH caching/compaction/compression/id/gc_grace_seconds/synchronous_updates/...` (ScyllaDB's view properties for indexes) gets "Unknown property"; DESCRIBE INDEX does not show them, nor the local-index `full(c)` form. | `index/ddl.rs`, `schema/params.rs` |
| `mv` | 18 / 9 | views | **Fixed** but 2 (see [Fixes](#fixes)). Missing rejections: invalid, slash and 500-character names; `WITH` options; filtering on a non-PK column in the SELECT. Rows are missing after a range delete plus insert, a partition tombstone, future tombstones, or an empty-string key. `ALTER TABLE DROP` of an unselected column is wrongly refused. TRUNCATE of a view gives "does not exist" instead of "Cannot TRUNCATE materialized view directly". `synchronous_updates` trace events are missing. | `views/` |
| `functions` | 13 / 1 | UDF/functions | **Fixed** (see [Fixes](#fixes)). `mintimeuuid` outside the 60-bit range (expected clamping), `set_intersection` (already passes on a later build), `fromjson` of an empty string or a blob map key, `avg(decimal)` scale, `counterasblob` typing, `blobasint` into bigint. | `functions/` |
| `cdc` | 8 / 0 | CDC | **Fixed** (see [Fixes](#fixes)). ALTER/RENAME of a CDC log table's columns is not refused or has the wrong message; the log table's DESCRIBE after the base table is altered. | `cdc/` |
| `dropped_id` | 2 / 6 | indexes + views | **Fixed** (see [Fixes](#fixes)). See above. Server error. | `index/ddl.rs`, `views/ddl.rs`, `storage.rs` |

Alternator: this run was cqlpy only. `--roll ... --suite alternator` is being wired up by the
Alternator work.

## Fixes

### Derived data: `fulltext`, `vector_index`, `index_props`, `mv`, `cdc`

Re-run of the full scylla-dialect suite on the working tree (2026-10-20 21:10, every subsystem's
changes included; the two disruptive tests in their own invocations): PASS 1877, FAIL 192,
ERROR 17, SKIP 272, XFAIL 174. 1872 of the 2009 reference-passing tests pass (93%); the other
137 are 136 known failures and one regression outside these categories
(`test_vector_similarity.py::test_cassandra_documentation_compatibility[cosine]`: a cosine
similarity of `5.55e-17` where the reference gives `0.0`, `similarity_fn`). The cassandra-dialect
runs of the same test files pass every listed derived-data test but the two below.

- **`fulltext` (all 71).** `USING 'fulltext_index'` (any case; the class is stored as written,
  shown in lower case) on one text/varchar/ascii column, static columns included: `analyzer` and
  `positions` options, tablets keyspaces only, the CDC log the Vector Store reads (created with
  the first index; CDC options must allow it, and CDC cannot be turned off or weakened while the
  index exists), unnamed duplicates refused, DESCRIBE, no view properties. `WHERE BM25(c, 't') > 0
  ORDER BY BM25(c, 't') LIMIT n` is validated with every message of the reference (selection,
  function relations and `ORDER BY` functions, arguments and types, operators, the literal 0,
  one restriction, same column and term, LIMIT, aggregation, PER PARTITION LIMIT, other
  restrictions, ANN, the index, a user function named `bm25`, bound terms at execution).
  **Decision:** the reference image has no Vector Store and answers a valid search with
  "Vector Store is disabled"; roll does the same. Native BM25 scoring over the base rows would be
  possible, but would return rows where the reference errors, so it is not done
  (`index/fulltext.rs`).
- **`vector_index` (all 34 / 3).** Targets with a local partition and filtering columns, stored as
  the reference's JSON (`{"tc":..,"pk":[..],"fc":[..]}`) and shown by DESCRIBE, with their
  validation and the 255 key column limit; named duplicates allowed, unnamed refused; IF NOT EXISTS
  on a taken name does nothing; the class name in any case (SAI's class only in its exact case,
  both dialects); every option with the reference's ranges, number parsing and check order;
  `index_version` / `class_name` refused as options; `source_model` kept; SAI `ENTRIES` messages;
  Cassandra's `allow_custom_parameters` checks (`DbConfig.settings.sai_vector_allow_custom_parameters`,
  on in the suite configuration). ANN queries: "Undefined column name", the missing index before
  other restrictions, "do not support per-partition limits".
- **`index_props` (29 of 32).** ScyllaDB's view properties on `CREATE INDEX` (COMPACT STORAGE,
  CLUSTERING ORDER BY, `cdc`, `default_time_to_live`, `id` and the rest) go to the index view,
  with the reference's messages and order; `synchronous_updates` is stored and shown (also
  `= false`); the local index of a frozen collection is described as `full(c)`; a local index view
  expires tombstones by timeout only with tablets. `caching = {'enabled': true}` drops `enabled`.
- **`mv` (16 of 18 / 7 of 9).** View maintenance read only live rows, so a row shadowed by a stored
  partition, range, row or cell tombstone (or by another statement of the same batch) got a view
  entry: the read before the write now includes the stored tombstones. A view row with an
  empty-string partition key is kept (ScyllaDB gives it token 0, Cassandra the minimum token).
  ScyllaDB filters a view on its key columns; checks the id, COMPACT STORAGE, `cdc` and the name
  first; refuses a taken id; drops only the columns no view needs; stores `synchronous_updates`
  and names the synchronous views in the write's trace.
- **`cdc` (all 8).** ADD/DROP/ALTER/RENAME on a log table are refused (longer texts while the
  base has CDC off); a log is detached while CDC is off and records the columns it lost as dropped
  when re-attached (also when the base drops or renames a column); `DESCRIBE TABLE <log>` is the
  `CREATE TABLE` in a comment while CDC is on. Dropped columns are listed in the order of
  ScyllaDB's `std::unordered_map` (`cpp.rs`), as index options are stored.

Still listed (5 scylla, 2 cassandra), all waiting for `table_options` validation in
`schema/params.rs`, which views and index views share: `test_create_view_compaction_options`,
`test_alter_view_compaction_options` and `test_create_index_compaction_options`
(`tombstone_threshold` and other compaction sub-options), `test_create_index_with_invalid_intervals`
("max_index_interval must be greater than min_index_interval") and
`test_create_index_with_invalid_memtable_flush_period` ("memtable_flush_period_in_ms must be 0 or
greater than 60000").

Alternator `tablets_cdc` (`test_streams_tablets.py`) needs `system.tablets` rows for CDC logs
(`base_table`) and tablet-count changes splitting stream shards: it waits for the tablets work.
`tombstone_paging` is the read path's `query_tombstone_page_limit`, not derived data.

### Guardrails, table options and ScyllaDB extensions

The tags are `sysconfig`, `large_data_guardrail`, `table_options`, `compact_semantics`,
`size_limits`, `keyspace`, `tablets`, `rowttl`, `using_timeout`, `service_levels`, `describe`,
`mutation_fragments`, `metrics` and `native_transport`. Full runs with `--compare`, 2026-10-20
22:00 (the cassandra run used the same server binary):

| dialect | PASS | FAIL | ERROR | SKIP | XFAIL | XPASS | reference PASS that roll passes | regressions | run time |
|---|---|---|---|---|---|---|---|---|---|
| scylla | 1989 | 32 | 17 | 272 | 174 | 48 | 1984 of 2009 (98.8%) | 0 | 418 s |
| cassandra | 1218 | 64 | 0 | 980 | 137 | 133 | 1209 of 1214 (99.6%) | 0 | 320 s |

The lists went to 25 ids (scylla) and 5 (cassandra). Left from these tags:
`test_compaction.py::test_tombstone_gc_with_*` (3: major compaction doesn't purge tombstones
against the memtable's timestamps, and the REST compaction always flushes),
`test_compactionstats_after_major_compaction` (3: no pending-compaction stats),
`test_bloom_filter.py` (bloom filter false-positive metrics),
`test_tombstone_limit.py::test_filtering_query_tombstone_suffix_last_position` (trace text) and
`test_mutation_dump_range_tombstone_changes` (2: auto-compaction merges the test's four
SSTables before it reads them; passes when it doesn't).

- **`sysconfig`.** `UPDATE system.config` of the live options (`crate::guardrails::config_table`)
  with ScyllaDB's failures; the replication, write-consistency and large-data guardrails, the
  tombstone page limit, the bind-variable switches, CassIO compatibility, and the `scylla_cql_*`
  metrics.
- **Options and limits.** Compaction, compression, caching, speculative_retry and interval
  validation per dialect (views and indexes too), keyspace rack lists, STORAGE and tablets
  options, COMPACT STORAGE semantics, key, batch, relation, vector and name limits.
- **ScyllaDB extensions.** Per-row TTL with its expiration service (`crate::rowttl`, CDC
  service deletions), `USING TIMEOUT` validation and zero timeouts, `restrict_future_timestamp`,
  `USING SERVICE LEVEL`, the scheduling-group limit, `t$paxos` and generic DESCRIBE, the
  `scylla_tags` property, `MUTATION_FRAGMENTS()` (`crate::mutation_fragments`), the
  Prometheus endpoint and the REST native-transport switch.

### Core query semantics

The tags are `dropped_id`, `counter_batch`, `paging_groupby`, `filtering`, `frozen`,
`allow_filtering`, `lwt`, `elem_writetime`, `selectors`, `null_handling`, `unset`, `prepare`,
`functions`, `similarity_fn`, `system_tables` and `schema_misc`. Every one of their ids is out of
both lists. They are marked **Fixed** in the tables.

These are the numbers from the full runs with `--compare`, 2026-10-20. The scylla run was at
20:00 and the cassandra run at 21:10. Both include other agents' work from the same day.

| dialect | PASS | FAIL | ERROR | SKIP | XFAIL | reference PASS that roll passes | regressions | run time |
|---|---|---|---|---|---|---|---|---|
| scylla | 1724 | 316 | 42 | 272 | 178 | 1719 of 2009 (86%) | 3 | 2163 s |
| cassandra | 1216 | 66 | 0 | 980 | 137 (and 133 XPASS) | 1208 of 1214 (99.5%) | 0 | 315 s |

The lists were pruned against these runs. They went from 615 to 287 ids (scylla) and from 46 to
6 ids (cassandra). The 6 cassandra ids left are `udf` (5) and `native_transport` (1).

The scylla run had 3 regressions:
- the cosine one, fixed after the run (see below);
- `compact_storage_test.py::testCompactCollections`;
- `test_secondary_index_properties.py::test_create_index_obsolete_view_properties`.

The later derived-data run above includes the last two.

- **`dropped_id`.**
  - DDL is atomic. New tables and indexes get their storage before any schema row is
    written, and on a failure the attached tables are dropped or restored.
  - A table or index whose id was dropped gets a fresh storage generation
    (`schema::storage_id`). The CQL-visible id does not change.
- **Wrong results.**
  - `counter_batch`: increments in one batch add up.
  - `paging_groupby`: PER PARTITION LIMIT counts groups. ScyllaDB pages also end at 1 MiB.
  - `filtering`: NOT IN with a null matches nothing.
  - `frozen`: bound frozen sets and maps are normalised.
  - `lwt`:
    - CAS results include the static row;
    - a ScyllaDB conditional write with no clustering targets is refused.
- **`allow_filtering`.**
  - ScyllaDB's `strict_allow_filtering` is a setting (default `warn`). The conformance server
    sets it to `true`, as the suite does.
  - A restricted partition plans through its local index first.
- **Errors and texts per dialect.** The tags are `null_handling`, `unset`, `selectors`,
  `prepare`, `functions`, `similarity_fn` and `schema_misc`. Among the changes:
  - `writetime`/`ttl` of elements and fields (`elem_writetime`);
  - ScyllaDB literal and integer typing;
  - the `NOT IN(c)` marker names;
  - `DROP ... USING TIMESTAMP ?`;
  - `ALTER TYPE` refused when the type is used in a partition key;
  - tracing ids on error frames.
- **`similarity_fn`.** The arity and type messages are now right. ScyllaDB computes in f64.
  The cosine of opposite vectors uses one square root, so it is exactly 0. This fixes the
  `[cosine]` regression.
- **`system_tables`.** `size_estimates` spreads the partition estimate over the token ranges.

The cases are probed on both references. Their tests are in
`crates/nosql-engine/src/tests/probed.rs`.

### Known failures to zero

Full runs with `--compare`, 2026-10-21 (one run at a time):

| suite | PASS | FAIL | ERROR | SKIP | XFAIL | XPASS | reference PASS that roll passes | listed | run time |
|---|---|---|---|---|---|---|---|---|---|
| cqlpy, scylla | 2014 | 24 | 0 | 272 | 174 | 48 | 2009 of 2009 | 0 (was 25) | 417 s |
| cqlpy, cassandra | 1217 | 65 | 0 | 980 | 137 | 133 | 1209 of 1214 | 5 (was 5) | 301 s |
| alternator | 1253 | 4 | 0 | 48 | 119 | 2 | 1253 of 1253 | 0 (was 36) | 420 s |

Every FAIL left is a test the reference fails too (or a strict xfail roll passes).

- **Audit logging** (`audit`, 16 alternator). ScyllaDB's audit (`crate::audit`,
  `nosql-alternator` `audit.rs`): the image's `audit: table` and `audit_categories:
  DCL,DDL,AUTH,ADMIN`, the live `audit_categories` / `audit_keyspaces` / `audit_tables`
  (`alternator.T` names table `T`) / `audit_rules` (fnmatch with `FNM_EXTMATCH`), rows in the stored
  `audit.audit_log`. CQL statements are logged after they run (refusals for missing permissions
  are not, as probed), logins as `AUTH`/`LOGIN`; Alternator requests per
  `docs/operating-scylla/security/auditing.rst` (categories, `<Op>|<JSON>`, batch filtering and
  stripping, stream tables `T|T_scylla_cdc_log`, consistency per operation).
- **Alternator options through CQL** (`sysconfig`, 12). `UPDATE system.config` accepted the
  options but Alternator kept its own copy: the engine now tells listeners
  (`Database::on_option_set`), Alternator follows them, and `system.config` shows Alternator's
  actual flags (a test that read `alternator_enforce_authorization` and wrote it back turned
  enforcement off for the rest of the run). Behind them: the expression cache with its metrics
  (`expr/cache.rs`), `max_concurrent_requests_per_shard` shedding, BatchWriteItem's consumed
  capacity under `alternator_force_read_before_write`, and `system.scylla_table_schema_history`
  stored and writable (the suite inserts into it with CQL).
- **Tombstone paging** (2). Alternator scans and queries stop after
  `query_tombstone_page_limit` tombstones and resume at the storage's page cut.
- **Tracing** (2), **service-level metrics** (1). Alternator requests are traced (`Alternator
  <Op>`, `alternator_op`, `query` cut at `alternator_max_users_query_size_in_trace_output`,
  `table` / `table[i]`) and slow-logged; a whole trace probability reads `1`; storage latencies
  carry the role's `scheduling_group_name`.
- **Tablet resizes of CDC streams** (`tablets_cdc`, 3). A CDC table and its log are one tablet
  group: `ALTER TABLE … WITH tablets` changing its count starts a stream generation (recorded in
  the altered table's `extensions`, key `roll_cdc_tablet_resizes`), shown in `system.tablets`
  (`base_table`), `system.cdc_timestamps` / `cdc_streams` and DescribeStream (`ParentShardId`,
  `CHILD_SHARDS`, closed shards' `EndingSequenceNumber`).
- **Compaction** (`test_tombstone_gc_with_*`, compactionstats, bloom filter, mutation dump). REST
  compaction honours `flush_memtables=false`; only the memtable's live data holds back purging
  (row markers for shadowable and view deletions, ScyllaDB's #20423/#20424), an expired TTL'd
  marker is dated at its write; pending compactions per table; per-table auto compaction;
  `NullCompactionStrategy` never compacts in the background (the mutation dump test was flaky
  because roll compacted its SSTables); bloom filter false positives counted per SSTable.
- **Smaller ones.** "Reusing querier" trace events on continued pages; a UDF call inside a
  collection argument resolves user functions; `CREATE OR REPLACE` of an existing function needs
  ALTER and CREATE; WASM UDFs (`udf/wasm.rs`, 13 tests); Cassandra takes a tuple literal for a
  frozen UDT (a non-frozen one is its ClassCastException, probed).

Still listed (5, cassandra): `java_udf` — `test_udf_with_udt`, `test_udf_permissions_with_udt`,
`test_uda_subfunc_permission_enforced[sfunc|finalfunc]` and
`uf_types_test.py::test_types_with_and_without_nulls` run Java UDF bodies. Cassandra compiles
them with the JDK and runs them in its JVM sandbox; roll stores Java UDFs and answers calls with a
`FUNCTION_FAILURE` (compatibility.md). They pass only with a JVM, which roll doesn't embed.

## Core gaps

| tag | count | what fails (observed, then expected) | module |
|---|---|---|---|
| `sysconfig` | 150 / 0 | **Fixed** (see [Fixes](#fixes)). `UPDATE system.config SET value=? WHERE name=?` gets "system keyspace is not user-modifiable". ScyllaDB lets you change live-updatable options. Through the suites' `config_value_context`, this blocks: `enable_create_table_with_compact_storage` (the `compact_storage` fixture, 60 `compact_storage_test.py` tests), the replication-strategy and RF guardrails (`test_guardrail_replication_strategy.py`), `write_consistency_levels_*` (`test_guardrail_write_consistency_level.py`), `query_tombstone_page_limit` (`test_tombstone_limit.py`), the replica large-data warnings, the bind-variable switches in `test_prepare.py`, and `enable_cassio_compatibility`. Representative tests: `test_tombstone_limit.py::*`, `test_guardrail_write_consistency_level.py::*`, `compact_storage_test.py::testAlterWithCompactStaticFormat`. Fixing this mostly exposes the behaviour behind each option, so expect follow-up failures. | `system/` (`system.config`), `config.rs` settings applied live |
| `large_data_guardrail` | 71 / 0 | **Fixed** (see [Fixes](#fixes)). The ScyllaDB 2026 table option `large_data_guardrails_enabled` gets "Unknown property". The large cell/row/collection coordinator guardrails, their warnings, and `BYPASS LARGE DATA GUARDRAILS` are missing. Representative tests: `test_coordinator_guardrail_large_cell.py::*`, `test_bypass_large_data_guardrails.py::test_insert_bypass_rejected`. | `schema/params.rs`, `statements/modify.rs`, `limits.rs` |
| `table_options` | 32 / 4 | **Fixed** (see [Fixes](#fixes)). Bad options are not rejected: compaction strategy sub-options, `chunk_length_in_kb` (must be a power of two, positive, not huge), `speculative_retry` percentiles (`'dogPERCENTILE'` gives "sstring out of range" ServerError instead of ConfigurationException), and `caching`/`bloom_filter_fp_chance` ranges. `compression = {'enabled': false}`-style maps get "Missing sub-option 'sstable_compression'/'class'", where the references accept them. Representative tests: `test_compaction_strategy_validation.py::test_size_tiered_compaction_strategy_options`, `test_sstable_compression.py::test_chunk_length_must_be_power_of_two`, `collections_test.py::testMapWithTwoSStables`. | `schema/params.rs` |
| `mutation_fragments` | 35 / 0 | **Fixed** (see [Fixes](#fixes)). `SELECT * FROM MUTATION_FRAGMENTS(t)` gets "line 1:32 : Syntax error". ScyllaDB returns the mutation dump (partition, clustering, range-tombstone and row fragments with metadata). This also covers the `test_compaction.py::test_tombstone_gc_with_*` tests. | `nosql-cql` grammar, `statements/select.rs` |
| `elem_writetime` | 18 / 16 | **Fixed** (see [Fixes](#fixes)). `writetime(m[k])`, `ttl(s[e])`, `writetime(u.f)` and tuples get "Invalid argument to writetime: only column names are supported". Both references support element and field selectors in writetime/ttl, with their own errors for PK columns and wrong types. Representative tests: `test_select_collection_element.py::test_writetime_map_element`, `test_udt.py::test_writetime_udt_field`. | `statements/select.rs`, `prepare/` |
| `rowttl` | 25 / 0 | **Fixed** (see [Fixes](#fixes)). ScyllaDB per-row TTL: `ALTER TABLE ... TTL c` gets "AlterTable statements are not supported yet", the `TTL` column in CREATE TABLE and DESCRIBE is missing, and rows never expire, so 5 tests time out. Representative tests: `test_ttl_row.py::test_alter_table_add_ttl`, `::test_row_ttl_expiration[int]`. | `schema/ddl.rs`, read-path expiry, `describe/` |
| `compact_semantics` | 0 / 22 | **Fixed** (see [Fixes](#fixes)). Cassandra dialect, COMPACT STORAGE semantics: dense tables require every clustering column ("Some clustering keys are missing"). Setting a dense value to null should delete the row, but the row stays. Restrictions on indexes over compact tables are not enforced, and their messages differ. CAS results on compact tables are missing the current value. Representative tests: `compact_storage_test.py::testUpdate[True]`, `::testInsertWithCompactStorageAndTwoClusteringColumns[False]`, `compact_table_test.py::testCompactStorageSemantics`. In the scylla dialect the same tests are blocked by `sysconfig` first. | `schema/ddl.rs`, `statements/modify.rs`, `statements/select.rs` |
| `size_limits` | 15 / 7 | **Fixed** (see [Fixes](#fixes)). These are accepted but should be rejected: clustering keys over 64 KiB, batches over `batch_size_fail_threshold`, 500-character keyspace/index/view names, vector dimensions over the limit, and WHERE clauses with too many relations (grammar overflow). Representative tests: `test_key_length.py::test_insert_65k_ck`, `batch_test.py::testOversizedBatch`, `test_name.py::test_keyspace_name_length_500`. | `statements/modify.rs`, `batch.rs`, `schema/ddl.rs`, `nosql-cql` |
| `allow_filtering` | 12 / 5 | **Fixed** (see [Fixes](#fixes)). Clustering-key restrictions that need ALLOW FILTERING under `strict_allow_filtering` (multi-column, token ranges) run without it. Index plus filtering: "Cannot execute this query..." is raised where the reference plans through the index, and a frozen-collection index returns 0 rows. Representative tests: `test_allow_filtering.py::test_allow_filtering_clustering_key`, `select_test.py::testFilteringWithSecondaryIndex`, `frozen_collections_test.py::testSecondaryIndex`. | `statements/select.rs`, `index/planner.rs` |
| `filtering` | 10 / 5 | **Fixed** (see [Fixes](#fixes)). `NOT IN` filtering returns excluded rows (wrong results). The error texts for `!=`, subscripted non-collections and non-frozen UDT restrictions differ. ORDER BY with IN on a skipped clustering column is wrongly refused, and ORDER BY with IN has the wrong row count. Representative tests: `test_filtering.py::test_filtering_with_not_in_relation`, `select_order_by_test.py::testOrderByForInClause`. | `statements/select.rs` |
| `selectors` | 10 / 5 | **Fixed** (see [Fixes](#fixes)). The selector column name for a cast to the same type should be `c`, not `cast_c_as_int`. Literal selectors should infer their type, and `count(1, 2)` should give "only valid when argument types are known". `token()` with the wrong arity or types is not rejected. `infinity`/`NaN` literals are refused in aggregates. A tuple from a string literal, a timestamp `'2011-02-03  Z '`, and `token([1, 2])` on vector keys are rejected but should be accepted. | `prepare/`, `statements/select.rs`, `nosql-types` literals |
| `paging_groupby` | 7 / 6 | **Fixed** (see [Fixes](#fixes)). Wrong results: GROUP BY with paging or `PER PARTITION LIMIT` limits rows instead of groups (`SELECT p, c1, count(*) ... GROUP BY p, c1 PER PARTITION LIMIT 1` returns `count=1` for a 2-row group). PER PARTITION LIMIT with static-only rows and paging returns 13 rows instead of 9. `test_paging.py::test_page_size_0` gets a different page split. Representative tests: `select_group_by_test.py::testGroupByWithPaging`, `test_group_by.py::test_group_by_count_with_pplimit`, `select_limit_test.py::testPerPartitionLimitWithStaticDataAndPaging`. | `statements/paging.rs`, `statements/select.rs` |
| `metrics` | 13 / 0 | **Fixed** (see [Fixes](#fixes)). Nothing listens on `:9180/metrics`. ScyllaDB serves Prometheus metrics there, and the suite reads protocol-exception and client-timestamp metrics. Representative tests: `test_protocol_exceptions.py::test_no_protocol_exceptions`, `test_metrics.py::test_client_timestamp_drift_metric`. | `nosql-server` (new listener) and `examples/conformance_server.rs` |
| `keyspace` | 12 / 0 | **Fixed** (see [Fixes](#fixes)). Rack-list replication (`'datacenter1': ['rack1']`) gets "Invalid property value". With the default replication options or the default RF, `datacenter1` is missing from the replication map. Keyspace `STORAGE` options are not validated. `ALTER KEYSPACE` without options and the Cassandra-ported `create_test.py::testKeyspace` rejections are missing. | `schema/replication.rs` |
| `null_handling` | 9 / 2 | **Fixed** (see [Fixes](#fixes)). ScyllaDB returns empty or false for `m[null]`, `CONTAINS null` and `CONTAINS KEY null`, where roll errors. The `IF l = [...]` LWT path and malformed lists give "Invalid NULL element in list" or "Null value read when not allowed" instead of the reference's result or message. DELETE with a null key is not rejected. Representative tests: `test_null.py::test_filtering_contains_null`, `test_lwt_prepared.py::test_lwt_update_prepared`. | `statements/select.rs`, `modify.rs`, `lwt/` |
| `unset` | 5 / 5 | **Fixed** (see [Fixes](#fixes)). Unset values inside collections, tuples, UDTs and function arguments are accepted, or rejected with null-value wording instead of "unset". Representative tests: `insert_test.py::testInsertWithUnset`, `tuple_type_test.py::testTupleWithUnsetValues`. | `statements/modify.rs`, `prepare/` |
| `tablets` | 7 / 0 | **Fixed** (see [Fixes](#fixes)). `ALTER KEYSPACE ... tablets = {'initial': N}` gets "Cannot alter replication strategy vnode/tablets flavor". `tablets = {'min_tablet_count': ...}` table options are missing from DESCRIBE, and vnodes->tablets is not rejected. | `schema/replication.rs`, `describe/` |
| `schema_misc` | 5 / 1 | **Fixed** (see [Fixes](#fixes)). The message for dropping a UDT that is still in use. ALTER TYPE used in a partition key is not rejected. `ALTER TABLE DROP c USING TIMESTAMP ?` (bind marker) gets a syntax error. The message for `c = c + 1` on a non-counter. | `schema/ddl.rs`, `nosql-cql` |
| `frozen` | 3 / 3 | **Fixed** (see [Fixes](#fixes)). A frozen set sent out of order by the client is stored as sent, so lookups miss it. It must be normalized (sorted) on write. | `nosql-types` serialization, `statements/modify.rs` |
| `lwt` | 4 / 1 | **Fixed** (see [Fixes](#fixes)). CAS results on static columns miss the current static value. A conditional batch with nulls returns the wrong row. `IF` on an empty clustering range is not rejected. | `lwt/` |
| `using_timeout` | 4 / 0 | **Fixed** (see [Fixes](#fixes)). ScyllaDB `USING TIMEOUT`: invalid values are not rejected, bind markers in batches fail, and TRUNCATE ... USING TIMEOUT does not time out. Far-future timestamps are not rejected (`test_futuristic_timestamp`). | `statements/` |
| `service_levels` | 4 / 0 | **Fixed** (see [Fixes](#fixes)). `ALTER SERVICE LEVEL default`, `SELECT ... USING SERVICE LEVEL x`, the scheduling-group limit, and DESCRIBE after the `driver` SL is dropped. | `auth/service_levels.rs`, `nosql-cql` |
| `describe` | 4 / 0 | **Fixed** (see [Fixes](#fixes)). `DESC TABLE t$paxos`, UDT dependency order, generic `DESC <name>` of a UDT, `synchronous_updates`. | `describe/` |
| `system_tables` | 2 / 2 | **Fixed** (see [Fixes](#fixes)). `system.size_estimates` reports 0 partitions. | `system/` |
| `counter_batch` | 1 / 2 | **Fixed** (see [Fixes](#fixes)). Wrong results: repeated increments of one counter inside a batch are applied once. | `statements/batch.rs` |
| `prepare` | 3 / 0 | **Fixed** (see [Fixes](#fixes)). Prepared `pk_indexes` are missing when named markers repeat. The `NOT IN` bind marker is named `IN(c)` instead of `NOT IN(c)`. | `prepare/` |
| `native_transport` | 1 / 1 | **Fixed** (see [Fixes](#fixes)). The REST disable/enable of the native transport (`test_enable_disable_binary`) has no effect. | `nosql-server/src/rest.rs` |

## Suggested dispatch

1. **`dropped_id`.** A server error that leaves partial schema behind (indexes + views owners).
2. **Silent wrong results.**
   - `counter_batch`, `paging_groupby`, `filtering` (NOT IN), `frozen`
   - The `mv` row-loss cases (views owner)
3. **`sysconfig`.** One table unblocks 150 scylla-dialect tests, then the guardrail behaviours
   behind it.
4. **The ScyllaDB 2026 table options.** `large_data_guardrail`, `rowttl` (also removes the
   timeouts), `index_props`, `tablets`.
5. **Validation sweeps.** Mostly "DID NOT RAISE":
   - `table_options`, `size_limits`, `allow_filtering`
   - `unset`, `null_handling`, `keyspace`
6. **Smaller items.**
   - Selectors and functions: `elem_writetime`, `selectors`
   - `mutation_fragments`, `metrics`
   - The remaining small tags
7. **Decide on the 35 strict XPASS tests** in the scylla dialect (bug compatibility).

## Reproducing a category

```sh
CARGO=~/.cache/roll-nosql/cargo.sh scripts/nosql-conformance.sh --roll scylla \
  --compare crates/nosql-server/tests/conformance/reference-scylla-cqlpy.txt -- cqlpy/test_null.py
grep '# null_handling' crates/nosql-server/tests/conformance/roll-known-failures-scylla.txt
```

The script reports listed tests that pass now. Delete those lines in the same change as the fix.
