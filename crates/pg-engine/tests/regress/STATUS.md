# PostgreSQL regression suite: status after the sweep

All 230 files of `src/test/regress/parallel_schedule` (REL_18_6) compared with PostgreSQL 18.6,
unit by unit, with the harness in `crates/pg-engine/tests/pg_regress.rs` (see `REPORT.md` for the
method). The schedule was run in 12 consecutive subsets in schedule order, each starting with
`test_setup` plus the prerequisites the schedule needs (`create_misc`, `create_table`,
`create_type`, `create_index`, `create_view`, `create_am`, ... as noted in `parallel_schedule`);
every file is counted once, from the subset it belongs to. The per-subset outputs were then merged
and the report rebuilt over all 230 files (`PGREGRESS_REPORT_ONLY=1`, `PGREGRESS_REPORT_DIR` to keep
`REPORT.md` untouched).

Re-run one subset (example):

```sh
PGREGRESS_ONLY=boolean,char,name PGREGRESS_DIR=/tmp/pgregress_x PGREGRESS_DB=regression_x \
PGREGRESS_REPORT_DIR=/tmp/report_x CARGO_BUILD_JOBS=2 \
cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

## Summary

| | |
|---|---|
| files | 230 |
| units compared | 51150 |
| units matching | 50383 (**98.50%**) |
| units differing | 767 |
| files fully matching | 178 |
| engine output == expected/*.out (pg_regress "ok") | 154 files |
| engine crashes / panics | none |

## Remaining differences, classified

| class | units | % of differing |
|---|---:|---:|
| EXPLAIN plan shape / EXPLAIN-derived output (group C1) | 681 | 88.8% |
| plan choice showing in results (row order of joins, grouping sets, recursive CTEs; C1 area) | 47 | 6.1% |
| qual evaluation order across views (security_barrier not distinguished; C1 area) | 10 | 1.3% |
| by design (server-side files, large-object import/export; a parallel plan's combine function) | 29 | 3.8% |
| heap-layout simulation (ctid / physical row order after aborts, updates, triggers) | 0 | 0% |
| real engine gaps | 0 | 0% |

No difference is PostgreSQL nondeterminism: every remaining unit is reproducible.

### EXPLAIN (group C1) — 681 units
join (110), subselect (72), partition_join (48), partition_prune (46), groupingsets (35), collate.icu.utf8 (35), inherit (32), aggregates (30), explain (25), updatable_views (19), btree_index (18), incremental_sort (17), rowsecurity (16), with (15), select_parallel (14), rowtypes (14), window (11), memoize (11), partition_aggregate (10), privileges (8), select_views (8), equivclass (8), rangefuncs (8), select_distinct_on (7), join_hash (6), generated_virtual (6), select_distinct (5), union (5), tsrf (5), sqljson (5), collate (4), create_index (3), merge (3), limit (3), returning (3), create_view (2), index_including (2), gist (2), tuplesort (2), insert_conflict (1), create_table (1), create_function_sql (1), select (1), hash_index (1), rules (1), stats_ext (1), foreign_key (1)

Includes EXPLAIN-derived values: `explain_filter`/`explain_analyze_*` helpers, `check_estimated_rows`
(stats_ext), hash-join batch counts (join_hash), `EXPLAIN (SETTINGS)`.

### Plan choice visible in results (C1 area) — 47 units
inherit (20), join (11), updatable_views (3), tsrf (3), with (3), subselect (2), partition_info (2), select_distinct (1), groupingsets (1), sqljson_jsontable (1)

- inherit (20): `a JOIN pg_class ON tableoid = pg_class.oid` hashes the inheritance tree instead of
  `pg_class` (Postgres uses a memoized index scan), because system catalogs are costed as a fixed
  1000-row scan (`explain.rs`, `Plan::SysScan`); rows come out in hash-bucket order.
- join (11): the file C1 works on (name resolution errors and join row orders).
- with (3), sqljson_jsontable, partition_info (2), subselect (2), updatable_views MERGE/row order (3):
  which side a hash join builds on. groupingsets/tsrf (4): grouping-set output order.
  select_distinct: parallel plan order.

### Qual evaluation order (C1 area) — 10 units
select_views (7), updatable_views (3)

The engine orders a pushed-down qual after a view's own quals as if every view were
`security_barrier`, so cheap leaky functions (`snoop`, `f_leak`) aren't evaluated first over
plain views (`binder/optimize.rs`, `order_security_quals`). Results are right; NOTICE order differs.

### By design — 29 units
misc_functions (14), largeobject (13), copy (1), aggregates (1)

- misc_functions: `pg_read_file`, `pg_read_binary_file`, `pg_stat_file`, `pg_ls_dir` of server
  paths (14). Reading the host's files would expose the machine the engine runs on to its
  tenants: refused for every role, with the error Postgres gives a role lacking the privilege
  (`permission denied for function pg_read_file`, SQLSTATE 42501).
- largeobject: server-side `lo_import`/`lo_export` (and the units reading the imported object),
  for the same reason: `permission denied for function lo_import` / `lo_export` (42501).
  Client-side `\lo_import` / `\lo_export` work.
- copy: server-side `COPY FROM` a file (with progress reporting). There is no data directory a
  tenant's COPY could safely read; refused with Postgres's `permission denied to COPY from a
  file` and its DETAIL / HINT (`... to a file`, `... to or from an external program` likewise).
  One-line `COPY ... :'file'` runs as `\copy` on both sides in the harness.
- aggregates `balk` (1): a combine function returning NULL shows only under a parallel
  partial aggregation (Finalize / Partial Aggregate over Gather); the engine aggregates
  serially and its planner (C1) doesn't choose that plan here.

Extension, not a difference: the table storage parameter `roll_pk_clustered` (roll's own;
nothing Postgres records uses it). By default a table stores its rows under a hidden row id
in heap order, its primary key an index of its own, so scans and ctids are Postgres's.
`WITH (roll_pk_clustered = true)` or `ALTER TABLE ... SET (roll_pk_clustered = true)` keys
the rows by a non-deferrable, non-`WITHOUT OVERLAPS` primary key instead (no entries of its
own): cheaper inserts and key lookups, sequential scans streamed in key order (ctids are
still the modelled heap's). SET / RESET lays the rows out again keeping their data, xmin,
ctid and indexes; a partitioned table may have it (only it) and passes it to new partitions
(SET / RESET recurse unless ONLY); a table without a key keeps it until one is added (as
pg_dump adds keys after the data); dropping the key ends it. It shows in
`pg_class.reloptions` / `\d+`. The storage upgrade (`Engine::upgrade_storage`) leaves such
tables keyed. Tests: `tests/pk_clustered.rs`, `tests/sql/pk_layout_default.sql`.

Re-examined and emulated now (see below): `CONTEXT: parallel worker` and the worker counters
under `debug_parallel_query`, a TOAST table read directly, MERGE into pg_class.

### Heap-layout simulation — 0 units
All 13 fixed (see below).

### Real engine gaps — 0 units
All 24 fixed (see below).

## Fixed in the second sweep (engine gaps, heap layout, by design)

Real engine gaps (all 24 units):

- Row-level security: policies may read system columns (`USING (ctid IN ...)`): scans their
  quals filter carry the system columns, and the new row a WITH CHECK sees has ctid
  `(4294967295,0)` (`binder/rls.rs`, `exec/dml.rs` `check_rls`). A SQL function's statement
  about to run after `row_security` was turned off re-checks its tables' policies and fails
  as Postgres's re-planning does ("query would be affected by row-level security policy",
  with the function's CONTEXT; `binder/perms.rs` `check_row_security`). `pg_policy.polqual` /
  `polwithcheck` are node trees (`nodetree.rs` `policy_expr_tree`); `pg_get_expr` maps them
  back to the policy's SQL.
- Partitioned keys: an index-backed constraint of a partition adopted by its parent's
  (ATTACH, `ALTER INDEX ... ATTACH PARTITION`) gets `conislocal = f, coninhcount = 1`; keys and
  foreign keys made as a partition's copy of its parent's have `connoinherit = f` (new
  `Constraint::from_parent`, serde default); a partitioned table's foreign key too. ATTACH
  PARTITION doesn't adopt an invalid index (AttachPartitionEnsureIndexes); CREATE INDEX on the
  parent does, and is invalid up the tree then. "multiple primary keys" for a partition whose
  parent's key was cloned first has no position (index_check_primary_key).
- A unique index under `record_image_ops` compares images (`row(1.0)` and `row(1.00)` are
  distinct entries; `dml.rs` `find_conflict`, `ops.rs` `record_image_equal`).
- An operator whose function returns a set (`CREATE OPERATOR ... PROCEDURE = unnest`): CREATE
  OPERATOR takes a polymorphic built-in's result type, and the operator runs as a ProjectSet
  call.
- NOT IN (an uncorrelated hashed `= ANY`) over columns with conflicting implicit collations
  fails "could not determine which collation to use for string hashing" (a correlated one:
  "... string comparison").
- SERIALIZABLE transactions that may write take SIRead predicate locks, as pg_locks shows them:
  relation locks for sequential scans, tuple locks for TID scans (coarsened to page and
  relation locks past max_pred_locks_per_page / _per_relation); not for temporary tables.
- A SELECT's rows go out as they are made: those before an error reach the client before the
  ErrorResponse, so psql's FETCH_COUNT (chunked mode) prints the full chunks first.
- `\sf` / pg_get_functiondef of a BEGIN ATOMIC body whose RETURNING has sublinks over OLD /
  NEW (RETURNING's scope stays in place while its sublinks are printed).
- `pg_get_catalog_foreign_keys()` (Postgres's 219 catalog foreign keys; the oidjoins checks
  find no violations).

Heap layout (all 13 units):

- An AFTER ROW trigger's tuple slot keeps its row's page pinned (and an UPDATE's old version's)
  while the trigger runs, so the trigger's own scans can't prune it (copy2).
- Index scans mark the btree entries whose heap tuples are all dead LP_DEAD
  (kill_prior_tuple); an insertion into a full leaf page deletes them first, with the dead
  entries pointing into the same heap blocks (_bt_simpledel_pass), before deduplicating or
  splitting (mvcc).
- TRUNCATE ... RESTART IDENTITY is transactional: a rollback (or ROLLBACK TO) gives the
  sequences their state back; a rolled back TRUNCATE or rewrite drops the heap model, which is
  laid out again from the rows (truncate).
- CLUSTER with index scans disabled (plan_cluster_use_sort) sorts the rows read in heap order
  with Postgres's quicksort on the index key alone, so equal keys come out in its order
  (tuplesort).

By design, re-examined:

- Emulated now: `debug_parallel_query = on` runs a parallel-safe SELECT in a single-copy
  Gather's worker: its errors and notices carry the CONTEXT line `parallel worker` (not in
  `regress` mode), and pg_stat_database counts the worker (`parallel_workers_to_launch` /
  `_launched`); reading a TOAST table directly (`pg_toast.pg_toast_<oid>`: chunk_id,
  chunk_seq, chunk_data from the modelled out-of-line values; `pg_column_toast_chunk_id`
  gives a value's chunk_id); MERGE into pg_class (or a plain view of it) updating the
  statistics columns, as UPDATE of pg_class already did.
- Server files stay refused for every role, now with the errors Postgres gives a role without
  the privilege: `permission denied for function pg_read_file` (pg_read_binary_file,
  pg_stat_file, pg_ls_dir, lo_import, lo_export; SQLSTATE 42501), and for COPY to / from a
  file or program `permission denied to COPY from a file` (`... to a file`, `... to or from an
  external program`) with Postgres's DETAIL and HINT.

Also: a constant call that fails to fold while an inlined SQL function's body is simplified
keeps its `CONTEXT: SQL function "f" during inlining` (the deferred fold error is raised once
the statement is analysed).

Note: oidjoins runs its 219 checks as correlated sublinks over system catalogs (each a SubPlan
regenerating the catalog's rows; NOT EXISTS isn't pulled up into an anti join there). In its
schedule subset it takes about 2 minutes; run in one database after the whole schedule, where
the catalogs are much larger, it exceeds a 600 s psql timeout.

Goldens added: `tests/sql/grg_rls_engine.sql`, `grg_misc_engine.sql`, `grg_heap_engine.sql`,
`grg_part_engine.sql`.

## Fixed in this sweep

Correctness first:

- MIN/MAX through an index (the minmax InitPlan) returned the wrong value for enums and for columns
  under non-bytewise (ICU) collations: the InitPlan's sort key now carries the aggregate's own `<`
  (`paths.rs`, enum: 2 units, plus ICU columns generally).
- `CASE x WHEN y` with a user operator over a domain: operator resolution now does the exact match on
  declared (domain) types first; a SQL function isn't inlined over a CASE test value
  (contain_context_dependent_node) — the inlined body's own CASE took it for its own.
- Target-list set-returning functions run after ORDER BY / DISTINCT ON when no key needs them
  (make_sort_input_target): `DISTINCT ON` returned 3 rows instead of 9, `ORDER BY` reordered.
- DELETE turned into UPDATE (or another command) by an INSTEAD rule with RETURNING returned no rows.
- TRUNCATE ... RESTART IDENTITY skipped sequences made the table's with `ALTER SEQUENCE ... OWNED BY`;
  TRUNCATE ... CASCADE now names a referencing partitioned table's partitions, in OID order per round.
- DROP OWNED dropped another role's user mapping on the role's server without complaint.
- DROP ACCESS METHOD dropped it from under an index using its operator class (opfamily → AM dependency).
- JSON_VALUE ... RETURNING a domain with a collation lost the collation (`= 'a'` false under a
  case-insensitive one); the DEFAULT collation conflict error was missing.
- A ROW's fields and an ANY/ALL/EXISTS sublink's columns no longer merge their collations
  (spurious "collation mismatch between explicit collations").
- A custom placeholder setting vanished after its SET LOCAL ended (current_setting errored).
- Temporary tables, views and sequences outlived their session (now dropped when the database's last
  session ends, so a concurrent session's DDL never conflicts with the cleanup).
- Hash partitioning of `text[]` under a nondeterministic collation.
- Partitioned indexes: `ON ONLY` in pg_get_indexdef/pg_indexes, `relhassubclass`, validity propagating
  up the tree (also on re-attach), "is not an index", constraint-vs-constraint and exclusion-operator
  checks on ATTACH, DROP INDEX CONCURRENTLY refusal (not for temporary ones), ALTER TABLE ATTACH/DETACH
  on an index, exclusion constraints on partitioned tables (partition-key coverage, non-equal
  operators), "multiple primary keys" when cloning onto a partition, ONLY ADD PRIMARY KEY requiring
  NOT NULL partitions.
- `bpchar_ops` accepted on text/varchar (binary-coercible); `information_schema.check_constraints`
  shows `pg_get_expr(conbin)`; pretty deparse parenthesizes COLLATE operands.
- Errors: pg_read_file length check, aggregate ORDER BY collation conflict position, set-operation /
  VALUES ORDER BY missing-column DETAIL/HINT (`*SELECT* n`, `*VALUES*`), PL/pgSQL cursor-argument
  CONTEXT (`42/0 AS p1, 77 AS p2`), statement text trimmed in QUERY, empty HINT on "is not a table",
  shell types' array oid 0 no longer matched (pg_identify_object of oid 0).
- Regressions from later changes: rule definitions with data-modifying WITH queries printed as
  garbage (`pg_get_ruledef` returned NULL: the "WITH ... must be at the top level" check now skips
  stored queries being printed); `table_rewrite` event for ALTER MATERIALIZED VIEW SET ACCESS METHOD.

Goldens added: `tests/sql/gsw_minmax_order.sql`, `gsw_sweep_query.sql`, `gsw_sweep_ddl.sql`,
`gsw_part_index.sql`.

## Per file

`ref=exp` / `eng=exp`: output identical to `expected/<name>[_N].out`. `collate.windows.win1252`
produces no units on Linux (skipped by the file itself).

| file | units | match | rate | ref=exp | eng=exp | top gap (harness class) |
|---|---:|---:|---:|---|---|---|
| test_setup | 79 | 79 | 100% | no | no |  |
| boolean | 99 | 99 | 100% | yes | yes |  |
| char | 32 | 32 | 100% | yes | yes |  |
| name | 40 | 40 | 100% | yes | yes |  |
| varchar | 22 | 22 | 100% | yes | yes |  |
| text | 73 | 73 | 100% | yes | yes |  |
| int2 | 76 | 76 | 100% | yes | yes |  |
| int4 | 94 | 94 | 100% | yes | yes |  |
| int8 | 174 | 174 | 100% | yes | yes |  |
| oid | 37 | 37 | 100% | yes | yes |  |
| float4 | 100 | 100 | 100% | yes | yes |  |
| float8 | 184 | 184 | 100% | yes | yes |  |
| bit | 133 | 133 | 100% | yes | yes |  |
| numeric | 1058 | 1058 | 100% | yes | yes |  |
| txid | 47 | 47 | 100% | yes | yes |  |
| uuid | 68 | 68 | 100% | yes | yes |  |
| enum | 172 | 172 | 100% | yes | yes |  |
| money | 113 | 113 | 100% | yes | yes |  |
| rangetypes | 419 | 419 | 100% | yes | yes |  |
| pg_lsn | 31 | 31 | 100% | yes | yes |  |
| regproc | 107 | 107 | 100% | yes | yes |  |
| strings | 504 | 504 | 100% | yes | yes |  |
| md5 | 14 | 14 | 100% | yes | yes |  |
| numerology | 89 | 89 | 100% | yes | yes |  |
| point | 43 | 43 | 100% | yes | yes |  |
| lseg | 16 | 16 | 100% | yes | yes |  |
| line | 35 | 35 | 100% | yes | yes |  |
| box | 101 | 101 | 100% | yes | yes |  |
| path | 23 | 23 | 100% | yes | yes |  |
| polygon | 62 | 62 | 100% | yes | yes |  |
| circle | 22 | 22 | 100% | yes | yes |  |
| date | 271 | 271 | 100% | yes | yes |  |
| time | 44 | 44 | 100% | yes | yes |  |
| timetz | 57 | 57 | 100% | yes | yes |  |
| timestamp | 177 | 177 | 100% | yes | yes |  |
| timestamptz | 406 | 406 | 100% | yes | yes |  |
| interval | 446 | 446 | 100% | yes | yes |  |
| inet | 116 | 116 | 100% | yes | yes |  |
| macaddr | 35 | 35 | 100% | yes | yes |  |
| macaddr8 | 71 | 71 | 100% | yes | yes |  |
| multirangetypes | 619 | 619 | 100% | yes | yes |  |
| geometry | 162 | 162 | 100% | yes | yes |  |
| horology | 399 | 399 | 100% | yes | yes |  |
| tstypes | 238 | 238 | 100% | yes | yes |  |
| regex | 105 | 105 | 100% | yes | yes |  |
| type_sanity | 66 | 66 | 100% | no | no |  |
| opr_sanity | 133 | 133 | 100% | no | no |  |
| misc_sanity | 5 | 5 | 100% | yes | yes |  |
| comments | 7 | 7 | 100% | yes | yes |  |
| expressions | 81 | 81 | 100% | yes | yes |  |
| unicode | 32 | 32 | 100% | yes | yes |  |
| xid | 84 | 84 | 100% | yes | yes |  |
| mvcc | 11 | 11 | 100% | yes | yes |  |
| database | 16 | 16 | 100% | yes | yes |  |
| stats_import | 150 | 150 | 100% | yes | yes |  |
| encoding | 81 | 81 | 100% | no | no |  |
| euc_kr | 5 | 5 | 100% | yes | yes |  |
| copy | 118 | 117 | 99% | yes | no | by design `server-side COPY file` (1) |
| copyselect | 36 | 36 | 100% | no | no |  |
| copydml | 62 | 62 | 100% | yes | yes |  |
| copyencoding | 22 | 22 | 100% | yes | yes |  |
| insert | 399 | 399 | 100% | no | no |  |
| insert_conflict | 261 | 260 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| create_function_c | 10 | 10 | 100% | no | no |  |
| create_misc | 88 | 88 | 100% | yes | yes |  |
| create_operator | 90 | 90 | 100% | yes | yes |  |
| create_procedure | 115 | 115 | 100% | yes | yes |  |
| create_table | 348 | 347 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| create_type | 88 | 88 | 100% | no | no |  |
| create_schema | 30 | 30 | 100% | yes | yes |  |
| create_index | 708 | 705 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
| create_index_spgist | 202 | 202 | 100% | yes | yes |  |
| create_view | 343 | 341 | 99% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| index_including | 137 | 135 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| index_including_gist | 52 | 52 | 100% | yes | yes |  |
| create_aggregate | 60 | 60 | 100% | yes | yes |  |
| create_function_sql | 168 | 167 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| create_cast | 24 | 24 | 100% | yes | yes |  |
| constraints | 607 | 607 | 100% | yes | yes |  |
| triggers | 1041 | 1041 | 100% | no | no |  |
| select | 87 | 86 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| inherit | 934 | 882 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (32) |
| typed_table | 36 | 36 | 100% | yes | yes |  |
| vacuum | 328 | 328 | 100% | yes | yes |  |
| drop_if_exists | 161 | 161 | 100% | yes | yes |  |
| updatable_views | 1109 | 1084 | 98% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (19) |
| roleattributes | 80 | 80 | 100% | yes | yes |  |
| create_am | 142 | 142 | 100% | yes | yes |  |
| hash_func | 43 | 43 | 100% | yes | yes |  |
| errors | 87 | 87 | 100% | yes | yes |  |
| infinite_recurse | 8 | 8 | 100% | yes | yes |  |
| sanity_check | 3 | 3 | 100% | yes | yes |  |
| select_into | 67 | 67 | 100% | yes | yes |  |
| select_distinct | 101 | 95 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (5) |
| select_distinct_on | 23 | 16 | 70% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (7) |
| select_implicit | 44 | 44 | 100% | yes | yes |  |
| select_having | 23 | 23 | 100% | yes | yes |  |
| subselect | 356 | 282 | 79% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (71) |
| union | 198 | 193 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (5) |
| case | 64 | 64 | 100% | yes | yes |  |
| join | 925 | 804 | 87% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (107) |
| aggregates | 556 | 525 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (30) |
| transactions | 433 | 433 | 100% | yes | yes |  |
| random | 48 | 48 | 100% | yes | yes |  |
| portals | 354 | 354 | 100% | yes | yes |  |
| arrays | 519 | 519 | 100% | yes | yes |  |
| btree_index | 135 | 117 | 87% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (18) |
| hash_index | 102 | 101 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| update | 289 | 289 | 100% | yes | yes |  |
| delete | 10 | 10 | 100% | yes | yes |  |
| namespace | 42 | 42 | 100% | yes | yes |  |
| prepared_xacts | 2 | 2 | 100% | yes | yes |  |
| brin | 77 | 77 | 100% | yes | yes |  |
| gin | 59 | 59 | 100% | yes | yes |  |
| gist | 69 | 67 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| spgist | 31 | 31 | 100% | yes | yes |  |
| privileges | 1398 | 1390 | 99% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| init_privs | 4 | 4 | 100% | yes | yes |  |
| security_label | 28 | 28 | 100% | yes | yes |  |
| collate | 149 | 145 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| matview | 189 | 189 | 100% | yes | yes |  |
| lock | 134 | 134 | 100% | no | no |  |
| replica_identity | 75 | 75 | 100% | yes | yes |  |
| rowsecurity | 1226 | 1210 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (15) |
| object_address | 85 | 85 | 100% | yes | yes |  |
| tablesample | 58 | 58 | 100% | yes | yes |  |
| groupingsets | 217 | 181 | 83% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (35) |
| drop_operator | 12 | 12 | 100% | yes | yes |  |
| password | 53 | 53 | 100% | yes | yes |  |
| identity | 288 | 288 | 100% | yes | yes |  |
| generated_stored | 468 | 468 | 100% | yes | yes |  |
| join_hash | 279 | 273 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| brin_bloom | 40 | 40 | 100% | yes | yes |  |
| brin_multi | 172 | 172 | 100% | yes | yes |  |
| create_table_like | 191 | 191 | 100% | yes | yes |  |
| alter_generic | 336 | 336 | 100% | no | no |  |
| alter_operator | 65 | 65 | 100% | yes | yes |  |
| misc | 68 | 68 | 100% | no | no |  |
| async | 11 | 11 | 100% | yes | yes |  |
| dbsize | 25 | 25 | 100% | yes | yes |  |
| merge | 572 | 569 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
| misc_functions | 148 | 134 | 91% | no | no | by design `server-side files` (14) |
| sysviews | 29 | 29 | 100% | yes | yes |  |
| tsrf | 74 | 66 | 89% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (5) |
| tid | 41 | 41 | 100% | yes | yes |  |
| tidscan | 49 | 49 | 100% | yes | yes |  |
| tidrangescan | 45 | 45 | 100% | yes | yes |  |
| collate.utf8 | 61 | 61 | 100% | yes | yes |  |
| collate.icu.utf8 | 681 | 646 | 95% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (35) |
| incremental_sort | 120 | 103 | 86% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (13) |
| create_role | 144 | 144 | 100% | yes | yes |  |
| without_overlaps | 674 | 674 | 100% | yes | yes |  |
| generated_virtual | 489 | 483 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (6) |
| rules | 632 | 631 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| psql | 1264 | 1264 | 100% | no | no |  |
| psql_crosstab | 32 | 32 | 100% | yes | yes |  |
| psql_pipeline | 362 | 362 | 100% | yes | yes |  |
| amutils | 10 | 10 | 100% | no | no |  |
| stats_ext | 878 | 877 | 100% | yes | no | wrong result `SELECT: values differ` (1) |
| collate.linux.utf8 | 2 | 2 | 100% | yes | yes |  |
| select_parallel | 250 | 236 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (13) |
| write_parallel | 22 | 22 | 100% | yes | yes |  |
| vacuum_parallel | 14 | 14 | 100% | yes | yes |  |
| maintain_every | 16 | 16 | 100% | yes | yes |  |
| publication | 781 | 781 | 100% | yes | yes |  |
| subscription | 179 | 179 | 100% | no | no |  |
| select_views | 52 | 37 | 71% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| portals_p2 | 41 | 41 | 100% | yes | yes |  |
| foreign_key | 1267 | 1266 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| cluster | 205 | 205 | 100% | yes | yes |  |
| dependency | 68 | 68 | 100% | yes | yes |  |
| guc | 222 | 222 | 100% | yes | yes |  |
| bitmapops | 12 | 12 | 100% | yes | yes |  |
| combocid | 62 | 62 | 100% | yes | yes |  |
| tsearch | 451 | 451 | 100% | yes | yes |  |
| tsdicts | 131 | 131 | 100% | yes | yes |  |
| foreign_data | 624 | 624 | 100% | no | no |  |
| window | 400 | 389 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (11) |
| xmlmap | 40 | 40 | 100% | no | no |  |
| functional_deps | 40 | 40 | 100% | yes | yes |  |
| advisory_lock | 38 | 38 | 100% | yes | yes |  |
| indirect_toast | 31 | 31 | 100% | no | no |  |
| equivclass | 96 | 88 | 92% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| json | 469 | 469 | 100% | yes | yes |  |
| jsonb | 1090 | 1090 | 100% | yes | yes |  |
| json_encoding | 47 | 47 | 100% | yes | yes |  |
| jsonpath | 224 | 224 | 100% | yes | yes |  |
| jsonpath_encoding | 35 | 35 | 100% | yes | yes |  |
| jsonb_jsonpath | 836 | 836 | 100% | yes | yes |  |
| sqljson | 242 | 237 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (5) |
| sqljson_queryfuncs | 318 | 318 | 100% | yes | yes |  |
| sqljson_jsontable | 131 | 130 | 99% | yes | no | wrong result `SELECT: row order differs` (1) |
| plancache | 97 | 97 | 100% | yes | yes |  |
| limit | 85 | 82 | 96% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
| plpgsql | 941 | 941 | 100% | yes | yes |  |
| copy2 | 290 | 290 | 100% | no | no |  |
| temp | 211 | 211 | 100% | yes | yes |  |
| domain | 486 | 486 | 100% | no | no |  |
| rangefuncs | 428 | 420 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| prepare | 33 | 33 | 100% | yes | yes |  |
| conversion | 68 | 68 | 100% | no | no |  |
| truncate | 193 | 193 | 100% | yes | yes |  |
| alter_table | 1749 | 1749 | 100% | yes | yes |  |
| sequence | 265 | 265 | 100% | no | no |  |
| polymorphism | 426 | 426 | 100% | yes | yes |  |
| rowtypes | 237 | 223 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (14) |
| returning | 145 | 142 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
| largeobject | 151 | 138 | 91% | yes | no | wrong result `SELECT: row count differs` (11) |
| with | 307 | 289 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (15) |
| xml | 281 | 281 | 100% | yes | yes |  |
| partition_join | 614 | 566 | 92% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (48) |
| partition_prune | 771 | 725 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (37) |
| reloptions | 66 | 66 | 100% | yes | yes |  |
| hash_part | 29 | 29 | 100% | yes | yes |  |
| indexing | 645 | 645 | 100% | yes | yes |  |
| partition_aggregate | 137 | 127 | 93% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (10) |
| partition_info | 73 | 71 | 97% | yes | no | wrong result `SELECT: row order differs` (2) |
| tuplesort | 108 | 106 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| explain | 60 | 35 | 58% | yes | no | wrong result `SELECT: row count differs` (13) |
| memoize | 73 | 62 | 85% | yes | no | wrong result `SELECT: row count differs` (6) |
| stats | 482 | 482 | 100% | no | no |  |
| predicate | 42 | 42 | 100% | yes | yes |  |
| numa | 3 | 3 | 100% | yes | yes |  |
| compression | 102 | 102 | 100% | yes | yes |  |
| compression_pglz | 21 | 21 | 100% | no | no |  |
| oidjoins | 1 | 1 | 100% | yes | yes |  |
| event_trigger | 230 | 230 | 100% | yes | yes |  |
| event_trigger_login | 14 | 14 | 100% | yes | yes |  |
| fast_default | 275 | 275 | 100% | yes | yes |  |
| tablespace | 261 | 261 | 100% | yes | yes |  |
