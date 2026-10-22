# PostgreSQL regression suite: status after the sweep

All 230 files of `src/test/regress/parallel_schedule` (REL_18_6) compared with PostgreSQL 18.6,
unit by unit, with the harness in `crates/pg-engine/tests/pg_regress.rs` (see `REPORT.md` for the
method). The numbers below come from one run of the whole schedule in one database, in schedule
order (`PGREGRESS_TIMEOUT=400`), so every file sees the catalogs the files before it left, as
`pg_regress` gives them (round 12's engine; that run measured 50700 matching units). Nine units of
collate.icu.utf8 (370–394, 606, 609: a `GROUP BY x COLLATE c` key shown as `(x)`) were fixed after
it and re-checked in a subset run of that file; they are counted as matching below. So are the
28 units of misc_functions, largeobject and copy that server-side files fixed (see "Server-side
files" below), re-checked in subset runs, and the units round 13 fixed (see the two "Fixed in
round 13" sections), re-checked in subset runs.

A full run of commit 286306f (2026-10-22, the whole schedule in one database,
`PGREGRESS_TIMEOUT=600`) measured 50792. It confirms every file's count below except three:
join 836 (its row-order unit 433 matches in that run), test_setup 78 (the shared reference
cluster already had `regress_tblspace` from a concurrent run, so the reference errored) and
alter_table 1745 (`ALTER TABLE tt7 DROP q` stalled past the 600 s psql timeout in that run and
the 4 units after it differ; the file matches in 84 s in a subset run with the files before it in
its schedule group, so the stall needs state from earlier in the full schedule: not yet
explained).

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
| units matching | 50849 (**99.41%**) |
| units differing | 301 |
| files fully matching | 190 |
| engine output == expected/*.out (pg_regress "ok") | 165 files |
| engine crashes / panics | none |

## Remaining differences, classified

| class | units | % of differing |
|---|---:|---:|
| EXPLAIN plan shape / EXPLAIN-derived output (group C1) | 289 | 96.0% |
| plan choice showing in results (row order of joins, grouping sets, recursive CTEs; C1 area) | 11 | 3.7% |
| by design (a parallel plan's combine function) | 1 | 0.3% |
| name resolution: a nested join's LATERAL reference to an earlier FROM item | 0 | 0% |
| heap-layout simulation, qual evaluation order | 0 | 0% |

No difference is PostgreSQL nondeterminism: every remaining unit is reproducible.

### EXPLAIN (group C1) — 289 units
join (87), partition_join (35), partition_prune (38), subselect (10), collate.icu.utf8 (8), incremental_sort (11), select_parallel (7), updatable_views (5), groupingsets (9), with (5), memoize (8), equivclass (8), aggregates (7), rowtypes (6), rowsecurity (6), generated_virtual (6), join_hash (4), collate (4), window (3), returning (2), merge (2), tuplesort (2), inherit (2), gist (2), tsrf (1), select_distinct (1), select (1), rules (1), rangefuncs (1), limit (1), insert_conflict (1), hash_index (1), foreign_key (1), create_view (1), create_table (1), create_index (1), create_function_sql (1)

Includes EXPLAIN-derived values: `explain_filter`/`explain_analyze_*` helpers, `check_estimated_rows`
(stats_ext), hash-join batch counts (join_hash), `EXPLAIN (SETTINGS)`.

### Plan choice visible in results (C1 area) — 11 units
join (3), updatable_views (1), join_hash (2), partition_info (2), stats_ext (1), subselect (1), sqljson_jsontable (1)

- sqljson_jsontable, partition_info (2), updatable_views (1): which
  side a hash join builds on. join: FULL JOIN row order.
  join_hash, subselect, stats_ext: values of EXPLAIN ANALYZE-style helpers over plans
  that differ.

### Name resolution — 0 units
Fixed in round 12: join 740–743, 745 and 791 match; 744 now runs and differs only in its plan
(counted under C1).

### By design — 1 unit
aggregates (1)

- aggregates `balk` (1): a combine function returning NULL shows only under a parallel
  partial aggregation (Finalize / Partial Aggregate over Gather); the engine aggregates
  serially and its planner (C1) doesn't choose that plan here.

### Server-side files — 0 units (were 28 by design)
misc_functions (14), largeobject (13) and copy (1) match now. Server-side file access stays
refused for every role by default (`permission denied for function pg_read_file`, `permission
denied to COPY from a file`, with Postgres's DETAIL / HINT); a server started with
`--allow-server-files <dir>` (`EngineOptions::server_files`) behaves as Postgres does for the
roles Postgres allows (EXECUTE on the genfile / lo_import / lo_export functions; privileges of
`pg_read_server_files` / `pg_write_server_files` for COPY), inside that directory only: it stands
for the data directory, relative paths resolve against it, and every path gets Postgres's check
for a role without `pg_read_server_files` ("absolute path not allowed", "path must be in or below
the data directory"), plus a real-path check that refuses symbolic links leading out (Postgres
would follow them). `COPY ... PROGRAM` stays refused. The regress harness starts the engine with
the suite's directory (`PGREGRESS_DIR`: `data/`, `results/`, and a `postmaster.pid` and `base/`
for misc_functions), and runs one-line `COPY ... :'file'` server-side unless psql wrote that file
(4 statements left as `\copy`, were 45). Subset run of the 18 files with server-side COPYs (plus
test_setup): misc_functions 134 → 148/148, largeobject 138 → 151/151, copy 117 → 118/118, no
other unit changed. Code: `src/server_files.rs`; tests: `tests/server_files.rs`,
`tests/sql/server_files_roles.sql`.

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

## Fixed in round 14, agent 6 (dummy-rel classes, parallel plans, incremental sort stats)

Measured in subset runs against the engine of 7be2e83 (rebased onto 3b11900) (test_setup, create_table, create_misc,
create_index, create_view, inherit, select, union, join, aggregates, groupingsets, equivclass,
subselect, partition_join, partition_prune, partition_aggregate, incremental_sort,
select_parallel, select_distinct, limit, memoize, with, rowsecurity, updatable_views, rules,
window, sqljson, tsrf, join_hash, write_parallel, stats_ext): 15 units fixed, none broken.

| file | fixed units |
|---|---|
| incremental_sort | 26, 27, 44, 45 |
| select_parallel | 35, 75, 116, 138, 170, 203 |
| partition_aggregate | 19, 50, 67, 111, 120 |

What changed:

- A relation proven empty keeps what its quals made (`Plan::Empty`'s `DummyInfo`): its
  equivalence classes (a GROUP BY or ORDER BY key equated with a constant is dropped: a
  GroupAggregate of no keys over the dummy Result, partition_aggregate 19/67) and the
  relation each column was read from. Its columns are qualified only when the plan scans no
  relation at all (set_rtable_names names every range table entry then, only the scanned ones
  otherwise), and such relations count for the plan's relation prefixes.
- Grouping costs: the keys an equality fixes (processed_groupClause) don't count in the
  comparisons or the group estimate (one group when all are fixed); the grouping target's
  expression keys are computed again per group (create_agg_path's target cost); a grouping's
  width counts the GROUP BY keys the select list doesn't name (resjunk entries). A grouping's
  keys keep their equivalence classes for the ORDER BY above.
- Parallel plans: an aggregation's target-list SubPlans are listed before those of its
  aggregates' arguments (ExecInitAgg's order, select_parallel 35); `x = ANY ((SELECT ...)::t[])`
  shows no dummy cast in a plan (75); a projection over a Gather Merge of a sorted partial
  plan whose postponed values workers can compute goes under it as a Result (138); EXPLAIN
  ANALYZE shows each worker's sort (`Worker N:  Sort Method: ...`, 116); a UNION ALL's arms
  (a one-table arm an appendrel member) make a Parallel Append (203); partial groupings
  estimate expression keys by their columns and show them as references above (`((two +
  0))`), the group expressions costed in the workers' scan; a scan's partial index paths are
  built for the orderings the planner found useful for it (the query's pathkeys, merge
  columns: `ScanPlan::useful`), and a Gather Merge over a sorted scan or join returns the
  relation's rows.
- Partitionwise aggregation: a partition's grouping gathered from workers keeps its serial
  form (`safe_alt`, with its HAVING) for a Parallel Append sharing the groupings out whole,
  which is now the grouping relation's partial plan (the Sort of an ORDER BY can go under a
  Gather Merge over it); a full partitionwise grouping also considers finalizing all the
  partitions' partial groupings over one Parallel Append.
- `x IN (SELECT ... window function ...)` is a semi join (a WindowAgg is a plain node to the
  IN pull-up); a hashed grouping over a WindowAgg shows the window function bare; a
  WindowAgg's startup cost uses the frame optimize_window_clauses gives it.
- Incremental Sort reads its input only as far as it needs (a LIMIT above stops it), and EXPLAIN
  ANALYZE shows its full-sort and presorted groups (`Full-sort Groups: 2  Sort Methods: top-N
  heapsort, quicksort  Average Memory: ..`, and the JSON groups), incremental_sort 26/27/44/45.
- A partitioned table's whole row above its partitions shows as converted to the parent's
  type (`count(((t1.*)::pagg_tab1))`, partition_aggregate 50).

Goldens added: `tests/sql/a6_dummy_rel_keys.sql`, `a6_parallel_plans.sql`,
`a6_incremental_sort_analyze.sql`.

Still open in these files, and why:

- incremental_sort 87, 89, 99–107 and select_parallel 167, 205, 207, 208: parallel paths the
  engine doesn't build yet: only one partial path per relation (the cheapest), so no Gather
  Merge of a partial path already in order or incrementally sorted (87, 167), no Gather Merge
  below a join keeping the outer order (99–102), no parallel nested loop / Parallel Hash Join of
  index paths (103–107, 208), a UNION ALL arm with InitPlans gathered on its own (205), a
  ProjectSet's input sorted as the sort's input target (207).
- select_parallel 132 (a LATERAL subquery with ORDER BY pulled up: it stays a Subquery Scan in
  Postgres), 191 (the IN semi join over a WindowAgg: Postgres hashes it, the engine unique-ifies
  it into a nested loop), 217 (a security-barrier view's scan gathered under its Subquery
  Scan).
- Cosmetic, not failing a unit: a Gather over a relation's partial plan returns the partial
  rows times the divisor, not the relation's rows (Postgres's create_gather_path), kept because
  an Append cut down by run-time pruning is costed over its remaining members only here (all of
  them in Postgres), and the two together flipped partition_prune 355; a sort's input target
  doesn't count computed resjunk sort keys in its width; a partitionwise grouping's serial
  members show the width before their projection.

## Fixed in round 13 (subqueries over joins pulled up, parameterized scans, dummy rels)

Measured against the engine of commit 687fc9b in a 24-file subset (`test_setup`, create_misc,
create_table, create_index, create_view, create_function_sql, inherit, updatable_views,
select_distinct, select_distinct_on, subselect, union, join, aggregates, groupingsets, equivclass,
rowsecurity, with, rangefuncs, memoize, partition_join, partition_prune, partition_aggregate,
rules): 25 units fixed, none broken.

| file | fixed units |
|---|---|
| join | 244, 351, 355, 359, 382, 399, 406, 407, 438, 455, 524 (433, a row-order unit, too) |
| partition_join | 27, 76, 78, 80, 82, 84, 194, 196, 198, 200, 202, 586, 590 |
| memoize | 50 (`explain_memoize` over a partitionwise join) |

What changed:

- A FROM subquery (or view) over a join is pulled up into the parent's join group
  (`paths/joinpath/lift.rs` `flatten_join_subquery`), its WHERE (and a filter pushed down onto
  it) the group's clauses, its outputs computed where they're read. One that would need a
  PlaceHolderVar evaluated at a join keeps the subquery a leaf (the plan has no node computing
  expressions at a join), unless reduce_outer_joins made every outer join nulling it inner;
  inside such a subquery a single-table subquery's outputs follow the same rule. Its scans are
  planned again in the group. A subquery's WHERE over outer joins of its own makes its items one
  unit to an outer join above (it can't be commuted past the clauses: this fixed wrong results
  the first version gave, `COALESCE(c.f1, 0) = 0` checked above `a ⟕ d`). Projections that
  evaluate a pulled-up empty relation's PlaceHolderVars at a join (`JoinMeta::phv_eval`) aren't
  pulled up again.
- A parameterized scan takes every join clause movable into it from its outer relations
  (get_baserel_parampathinfo), not only its index conditions: `Filter: (nt2.b1 OR a1)` in the
  inner index scan instead of a Join Filter.
- Partitionwise child joins plan their partitions again (they kept the parent Append's access
  paths, so no parameterized index scans), with the parent relations' rows as the loop counts of
  parameterized paths (a child relation's paths are parameterized by the parents, then
  reparameterized by child).
- A dummy join keeps its sides' outputs (`Output: 7` over `One-Time Filter: false`, not `NULL`),
  and a left join whose nullable side a pulled-up FROM-less `WHERE false` emptied shows Postgres's
  `Join Filter: false`.
- A nested loop's Materialize refers to its input's outputs (a value computed below shown
  parenthesized).
- `x op ANY (SELECT <one row without FROM>)` in WHERE is the comparison `x op e`
  (convert_ANY_sublink_to_join, the empty relation then removed), when `x` reads the query's
  columns: a constant-false one empties the query (382).
- Join paths parameterized by relations outside them in groups with outer joins
  (add_paths_to_joinrel's param_source_rels: the left side of an outer join whose right side
  the join is in), their outer side parameterized too, the join clauses movable into them from
  there their own quals (get_joinrel_parampathinfo), read as the enclosing nested loop's
  parameters (406/407).
- One equivalence-class clause per parameterized path: the one between the class's first members
  on either side, as the join's restriction list takes it (generate_join_implied_equalities);
  a parameterized join whose inner path enforces the class's clause with the parameters checks
  `parameter = outer member` instead (244, and 123's join order).

- A LEFT JOIN's ON comparison (`<`, `<=`, `>`, `>=`) is a parameterized index condition of
  its inner side, as its equalities were (359: `t4.i4 > (1)`); a constant output of a
  projection counts one distinct value (a Memoize keyed by a constant PlaceHolderVar); Inner
  Unique through a pulled-up subquery's scan that computes PlaceHolderVars.
- apply_scanjoin_target_to_paths: the target list over a partitionwise join is computed in each
  child join (no Result over the Append), once the query level is planned (76/78/84).

- Wrong results fixed: a pseudoconstant ON clause of an inner join (an outer query's value only,
  `ON x0.f1 = 1`) inside an outer join's nullable side gated the whole join group, emptying
  rows the outer join must null-extend (join 499's query: `EXISTS` false, `count(*)` 0 where
  Postgres has true / 5). It now holds at its join (its syntactic scope); 499's plan still
  associates the left joins differently.
- A field of an outer join's nullable whole row (`(t2.*).unique1`, a SQL function's `t.unique2`
  inlined) is its column (eval_const_expressions); a scan under a nested loop's Materialize
  returns just the columns read (CP_SMALL_TLIST), none when nothing is (399).
- A LATERAL subquery level (not pulled up) takes only the join clauses over the relations its
  lateral references read; the others stay the join's (438).
- reconsider_full_join_clause: `COALESCE(l, r) = constant` over a full join on `l = r` (a USING
  column) restricts both sides to the constant, the join clause and the WHERE clause dropped
  (455).

- A partitionwise full join's partition key also includes its sides' keys merged
  (`COALESCE(l, r)`, set_joinrel_partition_key's merged expressions), so a USING column of it
  joins the next table partitionwise (80/82, 3- and 4-way full joins).

- Partitionwise joins of multi-level partitioned tables: a FROM item's partitions are the ones
  of the deepest partitioned table all its scans are in (the named table at the top; within a
  child join, a partition that is partitioned itself), so a partition with partitions of its
  own joins them as one inheritance Append (or partitionwise again when the join clauses cover
  its key); a restriction its partitions already check isn't added again; the join clauses over
  such an Append name its first partition, as Postgres shows them (194–202, 586, 590).

Goldens added: `tests/sql/gpu_join_subquery.sql`, `gpu_param_joins.sql`, `gpu_pw_targets.sql`,
`gpu_join_quals.sql`, `gpu_full_pw.sql`, `gpu_multilevel_pw.sql`.

Still open from this round's list, and why:

- join 328: the subquery is pulled up now, but Postgres keeps `a3` a PlaceHolderVar computed in
  nt1's scan and checks `(nt2.b1 AND (id IS NOT NULL))` (another PlaceHolderVar, unsplit) inside
  the parameterized index scan; here the clause is split and the AND is a Join Filter.
- join 343/347 (constant PlaceHolderVars through reduced outer joins), 360, 368 (a dummy result over catalog scans naming `c.relname`), 41 of
  partition_join (a non-strict LATERAL PlaceHolderVar evaluated at a join): PlaceHolderVars
  evaluated at joins. 366: the IN's subquery is a full join of two one-row relations Postgres
  removes.
- partition_join 41/45/46: partitionwise joins with a LATERAL item aren't made, and a LATERAL
  subquery over a partitioned table isn't pulled up (`subquery_leaf` refuses partitioned scans).
  21 (a merge join chosen in one child join) and 94 (a nested loop over a parameterized index
  scan where Postgres merge-joins) are cost differences. 86/88/92: an IN over a partitioned
  subquery isn't pulled up into a semi join (the subquery is planned, partitionwise, before the
  WHERE's sublinks are pulled up), so it stays a hashed SubPlan.
- join 410: the join order now differs only by cost (`b ⋈ a` parameterized by i1 and i2 joined
  to i1, where Postgres joins `i1 ⋈ b` first).
- join 123: plan shape and join order match; the clause between the two aggregates reads
  `a = b` where Postgres prints `b = a` (its first derivation of the class clause, during a
  parameterized join it considered, fixes the orientation for later uses; the engine's search
  order differs, so that isn't emulated). Equivalence-class members are ordered by the clauses'
  order here; Postgres's follows how distribute_qual met them, which can pick another outer
  member for a parameterized path.

## Fixed in round 13, agent 3, second batch (sub-select copies, reltarget order, per-member EXISTS)

Measured in subset runs against the engine of 433cb26 (the first batch's files plus
rangefuncs, tsrf, window, join_hash, collate.icu.utf8, incremental_sort, groupingsets, union,
select_distinct and 30 more): 8 units fixed, none broken.

| file | fixed units |
|---|---|
| subselect | 186, 189, 191, 298, 312, 329, 353, 354 |

What changed:

- A pulled-up subquery's sub-select that two entries of a projection read (pullup_replace_vars
  puts a copy in each place) is two SubPlans / InitPlans in EXPLAIN, the copy's relations range
  table entries of their own (`q2_1`); a planner-made MIN / MAX InitPlan stays one parameter.
  EXPLAIN without ANALYZE only: the executor runs one.
- An ORDER BY key that is a pulled-up output constant for the query (an InitPlan's value) is
  dropped like any pseudoconstant key.
- A WHERE clause reading no column of the query (pseudoconstant, not volatile) gates the whole
  join (distribute_qual_to_rels gives it the full qualscope), not the first scan; `now() =
  now()` stays as written (only a clause over columns becomes `IS NOT NULL`); a join under a
  gating Result returns the columns needed above it.
- Target lists below a query level's top node in Postgres's order (`tlist_layout`): a scan
  under a Sort, Hash, Materialize or Memoize (CP_SMALL_TLIST) returning plain columns returns
  them as its reltarget has them, what the level's target list reads first in that order, then
  what its clauses read; a join each relation's columns in that order (the relations where
  they are); a Sort / Hash / Materialize / Memoize / Limit / Unique its input's order. Scans
  returning computed values, and physical target lists, are as before.
- A correlated EXISTS with a hashed alternative (AlternativeSubPlan) picks one per place
  reading it, by how often it runs there (fix_alternative_subplan): an inheritance tree's
  members each their own (`EXISTS(SubPlan 1)` in one, `ANY (c1 = (hashed SubPlan 2).col1)` in
  the next, the hashed copy's members `t2_4`, `t2_5`). The hashed form works over an
  inheritance tree (an Append of its members), in the executor too.
- `x IN (VALUES (...), (outer column))`: a LATERAL semi join with the VALUES list (its values
  over the row as the join's parameters; reading only queries further up, those values in
  place). A LATERAL item that is a join Postgres would have pulled up gets no Memoize (a join
  relation has no lateral_vars to cache by).
- An equivalence class's clause in a parameterized scan is written outer member first
  (generate_join_implied_equalities), unless its column is an index key: index matching
  generated that one column first (generate_implied_equalities_for_column), so a Bitmap Heap
  Scan's Recheck Cond keeps it so.

Goldens added: `tests/sql/a3_plan_targets.sql`; `a3_sublink_pullup.sql` extended.

Still open in subselect: 218, 221, 288, 310, 322, 326, 327, 328, 332, 342, 350 (see the first
batch's list; 350 is the LATERAL VALUES case with a correlated sub-select inside the list).

## Fixed in round 13, agent 3 (row comparisons with sub-selects, IN pull-up, DML row marks)

Measured in subset runs against the engine of f30b5fe (test_setup, create_misc, create_index,
create_view, subselect, join, aggregates, union, select_distinct, inherit, groupingsets,
partition_join, partition_prune, rowsecurity, updatable_views, with, equivclass, memoize,
rowtypes, select_parallel, rules, privileges, update, merge, returning, triggers,
insert_conflict, generated_virtual, generated_stored, foreign_key): 16 units fixed, none broken.

| file | fixed units |
|---|---|
| subselect | 31, 33, 161, 321, 325, 345, 347, 348, 349, 351, 352, 355 |
| rowtypes | 226 |
| select_parallel | 192 |
| merge | 458 |
| returning | 112 |

What changed:

- `ROW(...) op (SELECT ...)` is a ROWCOMPARE_SUBLINK (it was a scalar sub-select of one
  `ROW()`): correlated, a SubPlan testing the comparison per row (`SubqueryKind::RowCompare`,
  shown `(((1 = (SubPlan 1).col1) AND (2 = (SubPlan 1).col2)))`, ordering operators as
  `(ROW(..) < ROW((SubPlan 1).col1, ..))`); uncorrelated, an InitPlan of several columns the
  comparisons read as values (`(InitPlan 1).colk`), so they are separate quals (a One-Time
  Filter and a scan filter). Column-count errors, "more than one row", set operations,
  `SELECT *`, one-column rows and the view deparse (`((1, 2) = ( SELECT ...))`) are
  Postgres's now (some of these failed before).
- convert_VALUES_to_ANY only when the sub-select is a VALUES query (not a FROM subquery's
  VALUES under a computed select list) whose items fold to constants (no parameters, nothing
  stable or volatile); a test over a constant folds as a whole (`1 IN (VALUES (1), (3))` is
  gone). Other uncorrelated IN sub-selects that aren't one table join as semi joins too: a
  VALUES list pulled up (its uncorrelated sub-selects allowed), anything else as a subquery
  level (`ANY_subquery`, its ORDER BY's order not offered above). A correlated IN's own WHERE
  clauses come before its comparisons (hash keys, the unique-ified side's group keys). An
  inner join's ON-clause sublinks are pulled up over the join when all of them can be.
- EXPLAIN: a `ROW()` of constants is a record constant (`'("(1)",2)'::record`); a constant
  in a node's reference to its input stays bare (`'..'::record`, not parenthesized); a scan
  under a gating Result (One-Time Filter) returns its physical target list.
- UPDATE / DELETE / MERGE over a join (non-inherited targets; MERGE also over partitions): the
  join returns the processed target list (new values in column order, the target's ctid, or
  a view's whole row for its INSTEAD OF triggers; MERGE: the target's identity first, or last
  for an inheritance tree, then the source columns the actions read), then each other
  relation's row mark (its ctid in range table order, an inheritance parent's tableoid after
  all of them, relhassubclass counting); every scan below returns what that list reads of it
  first, then what its join clauses read; joins their outer side's columns, then their inner
  side's. A partitioned table's unique index makes a join with its members Inner Unique, and
  a Hash over an inheritance Append shows the parent's column names.

Goldens added: `tests/sql/a3_rowcompare_sublink.sql`, `a3_sublink_pullup.sql`,
`a3_dml_rowmarks.sql`.

Still open in these files, and why:

- subselect 189/191 (a pulled-up subquery output read twice is one SubPlan, two copies in
  Postgres), 186, 218 (IN over a set-returning select list), 221, 288, 298, 310 (LockRows at the
  top of a FROM subquery's level: the engine locks at the scans), 312, 322 (an EXISTS whose WHERE
  has a correlated IN: kept as a SubPlan inside the semi join's clause), 326 / 327 (the semi
  join's unique-ified side joined with one relation first; a correlated aggregate IN as a
  LATERAL subquery), 328 (an equivalence class member relabeled to the class's collation:
  `("*VALUES*".column1)::text`), 329 (the clause from the equivalence class written outer
  side first), 332 (unique-ifying on a computed expression), 342 (a strict operator over a NULL
  constant folds to NULL), 350 / 353 / 354 (IN over a VALUES list reading the outer row: a
  LATERAL semi join).
- merge 348 (the source subquery's sub-select copied into each action), 462 (an empty
  partitioned target: its dummy Result's names and Inner Unique); returning 103 / 105
  (rule-rewritten statements run as a CTE of the rule's actions).

## Fixed in round 13, agent 2, seventh batch (unread outputs under a WITH)

Same subset, against the engine of commit 80a37d3: 1 unit fixed, none broken.

| file | fixed units |
|---|---|
| with | 210 |

What changed:

- A FROM subquery that has its own WITH keeps a NULL for each output nothing above reads in
  its target list, as one without a WITH does (the With node no longer hides its top
  projection).

Tried and reverted: substituting a pulled-up one-row VALUES / FROM-less subquery's constants
into the query above (groupingsets 199 / 201). With the outer join's nullable sides and set
operation arms excluded it still broke join 349 / 364 / 379, so those two stay open.

Golden extended: `tests/sql/a2_cte_paths.sql`.

## Fixed in round 13, agent 2, sixth batch (DML sublinks, small target lists)

Same 29-file subset (plus the files with UPDATE / DELETE: update, delete, returning, merge,
triggers, rules, plpgsql, transactions, portals, arrays, matview, generated_stored, identity,
insert_conflict, alter_table, foreign_key, unchanged), against the engine of commit c4e2775:
5 units fixed, none broken.

| file | fixed units |
|---|---|
| updatable_views | 731 |
| with | 55, 58, 79 |
| groupingsets | 213 |

What changed:

- An UPDATE's or DELETE's WHERE clause (an updatable view's conditions too) over a plain table
  target turns its EXISTS / IN sublinks into semi / anti joins with the target
  (pull_up_sublinks), as a SELECT's does.
- A CTE or WorkTable Scan sorted for a merge join returns the columns used above the join, then
  the merge keys' (CP_SMALL_TLIST in reltarget order).
- A ProjectSet's input needs only what its consumers read of its target list (plus the calls'
  arguments); a Values Scan under it returns exactly those (CP_EXACT_TLIST).

Goldens extended: `tests/sql/a2_dml_views.sql`, `a2_cte_paths.sql`, `a2_grouping_misc.sql`.

## Fixed in round 13, agent 2, fifth batch (collated sort keys, WITH CHECK OPTION SubPlans)

Same 29-file subset, against the engine of commit 6cc6e0e: 3 units fixed, none broken.

| file | fixed units |
|---|---|
| collate.icu.utf8 | 614, 617 |
| updatable_views | 730 |

What changed:

- An ORDER BY key under an explicit COLLATE shows as the column with the sort's collation
  (`Sort Key: t1.c COLLATE "C"`), not as a value computed below the sort.
- A nondeterministic hash key of an inner / semi join shows a relabelling only when its
  column's collation isn't the clause's (the equivalence class's).
- WITH CHECK OPTION's sub-selects show as SubPlans of the INSERT / UPDATE (their relations
  named), and a write through an updatable view names the result relation (and its scan) by the
  view's alias for its table.

Goldens extended: `tests/sql/a2_collation_index.sql`, `a2_dml_views.sql`.

## Fixed in round 13, agent 2, fourth batch (nondeterministic collations in index paths, CYCLE)

Same 29-file subset, against the engine of commit 73393f3: 5 units fixed, none broken.

| file | fixed units |
|---|---|
| collate.icu.utf8 | 366, 367, 409 |
| groupingsets | 53 |
| with | 85 (the recursive term's join, so its rows' order) |

What changed:

- Index conditions under nondeterministic collations: an equality under the index column's
  own collation is an index condition (match_clause), and an exact pattern
  (match_pattern_prefix) an `=` under the index's collation when the pattern's collation is
  deterministic or the index's; partial prefixes give no range on a non-"C" index. The
  byte-ordered index can't seek such an equality: the scan reads the index and checks the
  filter on each row, the rows of the one run of collation-equal keys in heap order. An
  equality's selectivity compares the most common values under the collation.
- A DISTINCT ON key carrying its implicit collation is the column: a qual under that collation
  goes into the subquery, and a sort key a constant fixes under the column's own collation is
  redundant (the DISTINCT then a Limit).
- With grouping sets, a constant key is computed below the aggregation (a reference over a
  Sort).
- CYCLE's `NOT is_cycle` is a restriction of the work table's scan (the rewritten WHERE), and a
  recursive WITH's later items are planned seeing the earlier ones' estimates; the work table
  has as many values per column as rows when small (get_variable_numdistinct).
- A join key under a COLLATE is its column to examine_variable; a single grouping key is
  unique in its aggregation's output (hash bucket sizes).

Goldens added: `tests/sql/a2_collation_index.sql`; `a2_cte_paths.sql`, `a2_grouping_misc.sql`
extended.

## Fixed in round 13, agent 2, third batch (writes through views, bitmap order, called functions)

Same 29-file subset, against the engine of commit 5d742b7: 9 units fixed, none broken.

| file | fixed units |
|---|---|
| updatable_views | 110 (MERGE row order), 205, 206, 207, 805 (notice order), 809, 842, 846 |
| rowsecurity | 52 |

What changed:

- A write through a view with INSTEAD OF triggers reads the view's query as a subquery of the
  statement: a security barrier, or a query Postgres doesn't pull up (OFFSET, grouping, …),
  stays a Subquery Scan named after the view, the statement's and outer views' quals above it,
  and the result relation takes the next name (`Update on rw_view1 rw_view1_1`).
- A Subquery Scan's filter lists the equivalence classes' equalities with constants after the
  other clauses (baserestrictinfo order).
- A bitmap heap scan reads rows in their HOT chains' root order (the TIDs the indexes give),
  not their latest versions'.
- A called (not inlined) function in a qual costs its procost, not its body's plan, and is
  estimated at function_selectivity's default (1/3).
- EXISTS drops a constant select list (simplify_EXISTS_query): no Result over an Append under
  the SubPlan; an inheritance member's column mapping over its scan counts as the member, so
  a correlated SubPlan's member scans list no columns.

Goldens added: `tests/sql/a2_dml_views.sql`.

Found, not fixed: RETURNING `old` / `new` of a write through a view reports the column type
`record`; Postgres reports the view's row type (RowDescription only, psql prints the same).

## Fixed in round 13, agent 2, second batch (collations in sorts, CTE orders, worktables)

Same 26-file subset plus window, tsrf and sqljson, against the engine of commit b58ade6: 8 units
fixed, none broken.

| file | fixed units |
|---|---|
| collate.icu.utf8 | 399, 405, 419, 659 |
| with | 52, 76, 77 (76 / 77: the recursive term's join, so its rows' order) |
| groupingsets | 48 |

What changed:

- A grouping key's or window partition key's collation (its implicit COLLATE, or its column's
  when not the default) shows in the Sort Key that groups or partitions by it (`x COLLATE ci`),
  the key itself as the column (`PARTITION BY x`); an ORDER BY under the key's collation is the
  sorted aggregation's order, so no second sort.
- A materialized CTE's scan keeps its query's order (a top Sort or an ordered index scan): a
  merge join over it needs no sort (`CTE_ORDERS` beside `CTE_ESTIMATES`).
- cost_recursive_union (`nonrecursive + 10 * recursive` rows, a tuple cost per row); the
  worktable is ten times the non-recursive term (set_cte_size_estimates), without statistics
  through the self-reference, while the recursive term is planned and in EXPLAIN; a CTE
  Scan's width counts the columns read above.
- Binary-coercible casts there and back (`(x::oid)::integer`) are the index's column.
- A hashed grouping set's Hash Key shows the expression its input computes, not a reference.

Goldens added: `tests/sql/a2_cte_paths.sql`; `a2_grouping_misc.sql` extended.

## Fixed in round 13, agent 2 (user-operator estimates, statistics privileges, names)

Measured in subset runs of 26 files (create_misc, create_table, create_index, create_view,
create_function_sql, inherit, select_distinct, select_distinct_on, subselect, union, join,
aggregates, groupingsets, equivclass, rowsecurity, with, rangefuncs, memoize, partition_join,
partition_prune, partition_aggregate, updatable_views, rules, privileges, collate.icu.utf8,
select_views, after test_setup) against the engine of commit 687fc9b: 18 units fixed (memoize 50 also by the change
above), none broken.

| file | fixed units |
|---|---|
| privileges | 238, 239, 240, 251, 252, 253, 254, 258 (all) |
| collate.icu.utf8 | 372, 382, 384, 386, 388, 392 |
| groupingsets | 124 (hashed grouping-set output order) |
| join | 684 |
| memoize | 50 |
| with | 208 |

What changed:

- Clauses of user-defined operators are estimated by their restriction estimator (eqsel,
  neqsel, scalarltsel / lesel / gtsel / gesel): var_eq_const / var_eq_non_const and
  scalarineqsel, the most common values and the histogram's entries tested with the operator's
  function (the histogram isn't sorted by it: the fraction it accepts, clamped); scalarineqsel
  with the variable on the right needs a commutator. statistic_proc_security_check: a function
  that isn't leakproof sees the statistics only with all_rows_selectable — SELECT on the column
  (or the whole table, for an index expression) as the user the scan checks as
  (`ScanPlan::check_as`: a view's owner), and no row-level security policies on the table;
  otherwise `(1 - nullfrac) * 0.5` / `1 / ndistinct`.
- A pulled-up subquery's (or view's) scan, planned for itself before, gets the join's paths
  chosen again, parameterized ones included.
- A Subquery Scan's filter is estimated over the subquery's outputs (was a fixed 0.5) and costs
  cost_subqueryscan's per-tuple qual cost.
- EXPLAIN names: relations of one query level that share a name take their numbers in range-table
  order (`ScanPlan::rti`: a pulled-up subquery's, view's or inlined CTE's relations after the
  query's own), not join order; a subquery level's relations are named after the level's own, in
  plan order, before its SubPlans and CTEs; set operation arms in plan order (an INTERSECT's
  smaller input first).
- HAVING clauses go to WHERE by Postgres 18.6's expression_has_grouping_conflict: a key under a
  nondeterministic collation moves when it is a direct operand of a btree comparison under its
  own collation (simple CASE, IN, row comparisons included); any other use stays. A key an
  equality under its own nondeterministic collation fixes groups nothing (a GroupAggregate
  without keys), and a key carrying its implicit collation shows as the column above.
- The executor's hash tables leave out grouping keys that group nothing more (functionally
  dependent ones grouped along, keys fixed by an equality), as EXPLAIN's keys do: hashed
  grouping sets come out in Postgres's order.
- Unread subquery outputs stay in its target list as NULL constants (not only over an
  aggregation); a user-defined fixed-length base type has its typlen / typalign (widths, sizes).
- The regress harness can run the engine's psql in a host-network container
  (`PGREGRESS_ENGINE_CONTAINER`), for hosts whose firewall drops the docker bridge's traffic to
  the host.

Goldens added: `tests/sql/a2_userop_stats.sql`, `a2_grouping_misc.sql`.

Still open in these files (after both batches), and why:

- collate.icu.utf8 611, 619, 622, 644: partitionwise aggregation / joins under collations
  (plan choices); 630, 647: an equivalence class of three members under a nondeterministic
  collation (Postgres derives the restriction `c = b` and one join clause); 328, 334 (hashed
  grouping of a subquery and a semi join over DISTINCT under collations: cost choices).
  817, 819, 914 (conditional rules: Postgres plans each rule action as a statement of its own,
  here a CTE), 173, 174 (an equivalence class's constant reaching the other side of a join
  through a Subquery Scan's output), 848 (a security-barrier view over an inheritance tree with
  a sub-select output: its EXISTS not pulled up, a leakproof qual not pushed down).
- groupingsets 55, 57, 189–201, 215: SubPlans in grouping expressions (numbering and where
  they are computed), a pulled-up VALUES row's constants shown as references (199: constants
  under an outer join's nullable side must stay references, PlaceHolderVars), grouping
  expressions of grouping sets computed by the input (215).
- with 55, 58, 79: VERBOSE target lists over recursive CTEs (the work table scan's columns in
  the order the query reads them; its width counts a column only its filter reads); 235, 238, 241, 283: row marks in VERBOSE target lists (another agent's
  item).

## Fixed in round 12 (subqueries as query levels, LATERAL scope, SubPlans)

Measured against the engine of commit d449c68 in round 11's subset plus `insert_conflict`,
`create_table_like`, `tuplesort`, `gist` and `hash_index` (47 files with `test_setup`): 46 units
fixed, none broken; the whole schedule was then run again for the numbers above.

| file | differing before | after | fixed units |
|---|---:|---:|---|
| subselect | 49 | 31 | 168, 171, 174, 190, 193, 205, 214, 216, 234, 237, 240, 241, 247, 251, 253, 265, 273, 313 |
| join | 115 | 102 | 125, 349, 364, 437, 448, 542, 608, 740–743, 745, 791 |
| union | 2 | 0 | 181, 183 |
| select_distinct_on | 2 | 0 | 15, 19 |
| sqljson | 2 | 0 | 237, 241 |
| memoize | 11 | 9 | 7, 9 |
| rowtypes | 9 | 7 | 204, 215 |
| select_distinct | 2 | 1 | 83 |
| aggregates | 9 | 8 | 527 |
| create_view | 2 | 1 | 331 |
| incremental_sort | 16 | 15 | 41 |
| with | 14 | 13 | 82 |
| collate.icu.utf8 (full run) | 26 | 23 | 403, 407, 421 (pushdown under nondeterministic collations) |

What changed:

- A FROM subquery (or view) Postgres doesn't pull up — ORDER BY, LIMIT / OFFSET, DISTINCT
  [ON], GROUP BY / HAVING / aggregates, locking, WITH, set-returning or volatile outputs, a
  UNION ALL tree with other set operations inside — is a query level of its own
  (`Plan::Barrier { secure: false }`, which now records its ORDER BY's key count). The planner
  plans it apart (`paths::plan_level`, with the grouping order the level above wants), and
  EXPLAIN shows its `Subquery Scan` unless setrefs would remove it as trivial: the scan is
  kept when the query above reads fewer columns than the subquery returns, when the subquery's
  plan carries resjunk sort / group keys (apply_tlist_labeling), or under a filter; a hashed
  DISTINCT of plain columns labels the physical target list instead. Its order is offered
  upward (convert_subquery_pathkeys, truncated to the ORDER BY): DISTINCT ON keys are reordered
  to match it, a Merge Append merges the arms' orders, a LIMIT's bound reaches its sort
  (ExecSetTupleBound), incremental sorts build on it. UNION ALL arms are numbered `*SELECT* n`
  and a non-simple arm is its own level.
- Clauses go into such a level by subquery_is_pushdown_safe / qual_is_pushdown_safe: none past
  a LIMIT, none on DISTINCT ON's other columns or on set-returning outputs, none comparing a
  grouped column by an operator of another btree family (`*=`) or under another collation
  than its nondeterministic one; those that go are put through the level's projection, sort
  and DISTINCT, and below its grouping when they read only group keys.
  remove_unused_subquery_outputs drops an unread aggregate of a level's top aggregation too
  (`sum(1/a)` no longer raises), shown as `NULL::type`.
- LATERAL references across join levels: a parenthesized join's items see the FROM items to
  its left (`a, b JOIN LATERAL (VALUES (a.x)) ss`); the reference is a parameter of the join
  that reads it, and `A × (L ⋈ R)` with R reading A is reassociated to `(A × L) ⋈ R`, as
  Postgres's flat join tree has it. One-row VALUES and FROM-less subqueries are pulled up
  (pull_up_simple_values, remove_useless_result_rtes, PlaceHolderVars kept where nulled; a
  PHV over a LATERAL value is a Memoize key); the join search takes groups with LATERAL leaves
  and more than two members.
- SubPlans: a SubPlan reading only values of levels further out is uncorrelated here (an
  InitPlan, or hashed: `hashed SubPlan`); one in a join equality is a hash key, computed by the
  Hash (listed under it); one in an IN's left operand stays in the semi join's clause; one in a
  VALUES list is the Values Scan's; a subquery's InitPlans are its top node's, and a
  recursive union's terms' (their CTEs') their own. An InitPlan's value is no reason to compute
  a select list after a Sort. `ANY` over a user-defined operator shows its test expression,
  `ARRAY(SubPlan n)` its form.
- Smaller ones: make_group_input_target (grouping expressions computed below the aggregation,
  a set-returning one's ProjectSet returning only what the aggregation reads); JSON_ARRAYAGG /
  JSON_OBJECTAGG under a projection as `json_agg_strict` etc., the constructor above; CYCLE's
  mark tested as `NOT is_cycle`; an anonymous row's `IS [NOT] NULL` tests each field (nested
  rows by `IS [NOT] DISTINCT FROM NULL`); a CTE that locks rows, or is read twice and reads the
  recursive query's working table, isn't inlined (contain_dml, contain_outer_selfref); a
  semi join's unique-ified side scans just the compared values (create_unique_paths); volatile
  outputs of a subquery stay in its target list.

Goldens added: `tests/sql/gc2_subquery_scan.sql`, `gc2_plan_shapes.sql`.

Still open from this round's list, and why:

- inherit 445/453: a Merge Append over UNION ALL arms by an expression of their columns needs
  equivalence members translated into each arm (445); 453 is a cost choice (Merge Join over
  Merge Appends of index scans under a LIMIT vs. our Hash Join + Sort).
- join 328, 343–347, 351–366: subqueries over several tables with PlaceHolderVars aren't
  flattened into the parent's join group (only single-table and FROM-less ones are); a dummy
  relation's Result shows `NULL` where Postgres keeps the PlaceHolderVar's expression
  (351, 368); 744 is a plan choice now (a parameterized hash join inside the nested loop).
- VERBOSE row marks (merge 348/458/462, returning 103/105/112): not started; the non-target
  relations' ctid / whole-row marks would have to be carried through every node's target list;
  rule-rewritten RETURNING runs as a CTE of the rule's actions here. A FROM subquery's
  `FOR UPDATE` likewise: the LockRows is placed at the statement's top, not inside the
  subquery's level (subselect 310).
- partition_aggregate (5): parallel partitionwise aggregation (Gather over Parallel Append of
  Partial Aggregates) isn't costed; 19: a GROUP BY key equated with a constant isn't dropped
  (GroupAggregate without keys).
- subselect 288 (a catalog's parameterized index path in a join), 298 (a lifted LATERAL Var's
  place in a join's output), 189/191 (a pulled-up subquery output read twice is one SubPlan
  here, two copies in Postgres), 186 (an EXISTS over an inheritance tree as a hashed SubPlan per
  member), 218 (an IN subquery with a set-returning select list as a semi join), 221 (a
  pushed-down LATERAL qual as the Subquery Scan's filter, not a One-Time Filter), 31/33/161
  (row comparisons with a sub-select as `(SubPlan 1).col1`, constant-folded records).
- with 52, 55, 58, 76, 77: a materialized CTE's sort order isn't offered to a merge join; the
  recursive term's join order and the rows' order follow from it.

## Fixed in round 11 (subquery pull-up, cost-driven choices)

Measured against the engine of commit 39a2047 in the same subsets: subselect 297 → 305, join
805 → 810, union 194 → 196, groupingsets +2, inherit +1, stats +1; no unit got worse in any file
of the subsets run (create_misc, create_table, create_index, create_view, create_function_sql,
select, inherit, updatable_views, select_distinct, select_distinct_on, subselect, union, join,
aggregates, privileges, collate, rowsecurity, groupingsets, generated_virtual, rules, stats_ext,
select_views, foreign_key, equivclass, sqljson, limit, rangefuncs, rowtypes, with, merge,
returning, partition_join, partition_prune, partition_aggregate, explain, memoize,
incremental_sort, tsrf, window, join_hash, select_parallel).

| file | fixed units |
|---|---|
| subselect | 192, 290, 292, 294, 296, 300, 302, 304 |
| join | 422, 607, 726, 763, 780 |
| union | 189, 195 |
| groupingsets | 61, 141 |
| inherit | 871 |
| stats | 480 |

What changed:

- Pulled-up FROM subqueries' computed outputs (`paths/joinpath/lift.rs`, Postgres 18's
  pullup_replace_vars_callback): a join group's subquery leaf over one table becomes the table
  itself, its select list's expressions evaluated where the query reads them (the join's target
  list, an upper join clause, an aggregate's arguments — make_group_input_target), unless an
  outer join could null them and they wouldn't go NULL with the table's row: a PlaceHolderVar
  (no column of the table, a non-strict construct, a LATERAL reference to a relation not under
  the same lowest nulling outer join), which stays computed in the scan, after the relation's
  Vars in its output. Outer joins count as written (reduce_outer_joins records
  `JoinMeta::written_kind`): pull-up comes first in Postgres.
- LATERAL subqueries: once their outputs are lifted, the WHERE clauses reading the left side
  become the join's clauses (deconstruct_jointree, before its ON clause) unless the relations
  they read are among those a remaining PlaceHolderVar needs (then the parameterized scan keeps
  them); a side that reads nothing of the left row any more is an ordinary join (hash and merge
  joins, the side planned again), the parameters it still reads renumbered.
- A LATERAL subquery's scan returns the columns its clauses over the outer side read (their
  attr_needed); a hashed side's scan lists its PlaceHolderVars after its Vars and the Hash shows
  them by reference; a Sort over a join returns the join's columns needed above.
- Quals over a domain without constraints: the coercion is a RelabelType the operator's own
  collapses (`relname = 'x'::name`, an index condition again); a domain with constraints shows
  `((x)::d)::integer`. Quals now go through a subquery's projection into a system catalog's scan,
  and catalog scans keep their FROM alias (`pg_class c`).
- `x = x` over an outer query's value stays as written (no `IS NOT NULL`: it isn't an
  equivalence class member there).
- An inner join's ON clauses take part in equivalence classes as WHERE clauses do: a constant
  equated with one side reaches the other (`ON a.h = 42 AND b.h = a.h` scans both with `h = 42`).
- A UNION ALL's arms under a constant LIMIT are planned for the rows it reads (the binder plans
  each arm with the limit, `plan_query_limited`; a join group under a LIMIT takes the
  fractional choice).
- Appendrel parameterized paths on expression indexes (`f(x) = outer`, each member's own index);
  join selectivity from an index expression's statistics (examine_variable), which a user only
  gets with SELECT on the whole table (or the partitioned table named); a pattern over an
  expression without statistics by its heuristic selectivity (patternsel's heursel).
- MIN / MAX InitPlans under `GROUP BY ()`; grouping sets' first rollup asks for its keys' order
  (an index scan delivers it, no Sort).
- estimate_rel_size's density is at least one row per page (a never-analyzed table of wide rows
  under a low fillfactor estimated 0 rows); a join alias naming more columns than the join has
  fails as Postgres does; an empty appendrel's Result names its columns.

Goldens added: `tests/sql/gc1_lift_subquery.sql`, `gc1_plan_costs.sql`, `gc1_plan_misc.sql`.

Still open from this round's list, and why:

- subselect 288: needs parameterized index paths for system catalogs in a join (the catalog's
  scan is a leaf of its own); 298: a join's output order for a lifted LATERAL Var.
- join 328, 343–347, 349–360: subqueries over joins (and FROM-less ones) aren't flattened into the
  parent's join group (only single-table subqueries are), and remove_useless_result_rtes; a
  PlaceHolderVar as a nestloop parameter (Memoize's `Cache Key: (1)`, memoize 9).
- VERBOSE row marks (merge 348/458/462, returning 103/105/112): the non-target relations' ctid /
  whole-row marks would have to be carried through every node's target list in reltarget order
  (the target lists are built from the plan's columns, which don't have them); rule-rewritten
  RETURNING runs as a CTE of the rule's actions here.
- union 181/183, inherit 445/453, select_distinct_on 15/19, subselect 247/251/253/265/273: a
  subquery Postgres doesn't pull up (DISTINCT, ORDER BY, LIMIT, set operations) has no node of
  its own here, so it can't show as `Subquery Scan on <alias>` nor offer its sorted paths to the
  query above (Merge Append over the arms' Unique, incremental sorts over it); the UNION ALL
  projection is computed above the Append, not in each arm.
- partition_aggregate (5): parallel partitionwise aggregation (Gather over Parallel Append of
  Partial Aggregates) isn't costed.
- sqljson 237/241: the Agg node shows the JSON constructor instead of its aggregate
  (`jsonb_agg_strict(...)`), the grouping expression is computed at the Agg (not below it), and the
  WindowAgg's sort over the grouping isn't seen as presorted.
- Index ties: already Postgres's order (indexes considered newest first, add_path keeping the
  old path on a tie); create_index 411/671 differ for other reasons (an OR of equalities with a
  join value as `= ANY (ARRAY[...])` index condition).
- inherit's `tableoid = pg_class.oid` units match in the full single-database run.

## Fixed in the planner pass

Measured against the engine from before the pass in the same 47-file subset (`create_misc`,
`create_index` and the files' prerequisites first): 15825 → 15930 units, 105 fixed, none broken.
The per-file rows for these files (and the totals above) were re-measured in that run, so they
also take in changes made since the last full run, and the subset's smaller catalogs: inherit's
`tableoid = pg_class.oid` plan-choice units (20) match there; a full run may still show them.

| file | differing before | after | fixed units |
|---|---:|---:|---|
| inherit | 29 | 3 | 415, 434, 439, 441, 468, 752, 778, 782, 784, 785, 790, 792, 793, 800, 807, 808, 813, 823, 825, 832, 840–843, 849, 850 |
| explain | 23 | 0 | 8, 9, 11, 15, 19, 20, 24–27, 29, 42, 46, 48–55, 57, 58 |
| btree_index | 14 | 0 | 33–45 (odd), 58, 65, 67, 69, 72, 74, 76 |
| rowsecurity | 15 | 7 | 133, 135, 600, 602, 606, 608, 805, 821 |
| collate.icu.utf8 | 33 | 26 | 318–332 (even, not 328) |
| rangefuncs | 7 | 1 | 319, 346, 356, 398, 408, 411 |
| rowtypes | 14 | 9 | 60, 62, 64, 66, 74 |
| select_distinct_on | 5 | 2 | 7, 13, 17 |
| index_including | 2 | 0 | 39, 42 |
| select_views | 2 | 0 | 37, 39 |
| subselect | 61 | 59 | 230, 231 |
| with | 16 | 14 | 61, 63 |
| create_index | 3 | 2 | 452 |
| partition_prune | 39 | 38 | 359 |
| union | 5 | 4 | 197 |
| updatable_views | 17 | 16 | 797 |
| window | 4 | 3 | 398 |

What changed:

- Ordered partition scans (`paths/mergeappend.rs` `partition_order`): an ORDER BY over a
  partitioned table reads the partitions one after another in bound order (backwards for DESC)
  under a plain Append when they can't interleave (partitions_are_ordered: RANGE without a live
  default, LIST without live interleaved partitions; build_partition_pathkeys with constant and
  boolean-constant key columns), costed as cost_append with pathkeys; a sub-partitioned table
  that isn't ordered keeps its own Merge Append and takes an alias (`mcrparted_6`) as its
  Append node does. A table pruned to one partition is planned as that partition (its index
  order), an inheritance tree's constant columns (the same constant in every member) make its
  sort keys redundant, and a member's sort keys skip the ones its clauses fix.
- Merge Append display: VERBOSE members compute the sort expressions (a Result above when the
  query returns other columns), member Sorts return what their input does, an appendrel member
  scan returns only the columns needed above (no physical target list), sort keys of expressions
  over the parent's columns (`(abs(mcrparted.b))`), constants in a member's sort keys stay
  constants.
- Inlined set-returning SQL functions in FROM: arguments without volatile functions or
  sub-selects are substituted into the body even when they read earlier FROM items
  (substitute_actual_srf_parameters); record functions with a column definition list inline;
  the body's target list takes the coercions to the declared result (check_sql_fn_retval); a
  set-operation body shows as its `*SELECT*` subquery.
- Row comparisons `ROW(a, b) op ROW(x, y)` stay one RowCompareExpr: btree index conditions over a
  prefix of the index (expand_indexqual_rowcompare, `<` becoming `<=` on a shorter prefix),
  selectivity of the first column pair, evaluation by the first unequal pair.
- System catalogs are planned as relations of their own (`paths/syscat.rs`): sizes from the
  catalog's rows, their indexes (`catalog/sys_indexes.tsv`) for ordered and index scans, rows
  returned in index order when the plan reads one.
- DISTINCT / DISTINCT ON: keys reordered to an index's order (get_useful_pathkeys_for_distinct), a
  descending ORDER BY over a DISTINCT read backwards, constant keys one group (a Limit 1).
- Row-level security: permissive policies are ORed in reverse name order; a locking read of an
  inheritance tree shows one LockRows over the Append; WHERE CURRENT OF keeps the policies as
  the Tid Scan's Filter.
- Nondeterministic collations: equality joins hash the collation's sort keys (not merge-joined);
  a partitionwise join is refused when the clause's collation isn't the partition key's (it
  returned wrong row counts); Inner Unique only under the unique index's collation.
- Nested-loop Join Filters and equivalence-class clauses in Postgres's written orientation; MERGE
  join clauses target first.
- Subquery pushdown: clauses go into a non-security-barrier subquery when they read none of its
  unsafe outputs (set-returning / volatile columns), and below a ProjectSet when they don't read
  the set-returning results.
- EXPLAIN: SETTINGS (the GUC_EXPLAIN settings), Query Identifier, the Planning group (buffers,
  memory), SERIALIZE, I/O timings under track_io_timing, Storage lines for WindowAgg and
  Materialize, external merge sorts past work_mem, EXPLAIN of DECLARE CURSOR, SEARCH / CYCLE
  columns (`*DEPTH*`, array_cat / int8inc), an index-only scan's conditions over its columns
  (no `(b)::text` relabeling), a one-item IN costed as its `=` in the filter order, functions in
  pg_temp qualified as `pg_temp.`.

Goldens added: `tests/sql/gpl_inline_srf.sql`, `gpl_rowcompare.sql`, `gpl_catalog_paths.sql`,
`gpl_distinct_order.sql`, `gpl_explain_misc.sql`, `gpl_n_ordered_append.sql`,
`gpl_n_rls_plan.sql`, `gpl_n_append_misc.sql`.

Still open from this pass's list, and why:

- Lateral subqueries with output expressions (subselect 288–304, join 328/343–347): needs
  placeholder (PlaceHolderVar) semantics and deferred projection in the plan.
- VERBOSE row-mark target lists of UPDATE / DELETE / MERGE (merge 348/458/462, returning 103/105):
  source relations' `ctid` / whole-row marks and rule-rewritten RETURNING; the target lists are
  built differently.
- sqljson 237/241: JSON aggregates under a WindowAgg show their underlying functions in the Agg
  node, and the window's sort over the grouping isn't recognised as presorted.
- partition_aggregate (5), union 181/183/189/195, inherit 453/871, select_distinct_on 15/19:
  cost-driven choices (fast-start nested loops inside a UNION ALL arm, merge joins over Merge
  Appends, subqueries with ORDER BY kept as Subquery Scans) and parallel partitionwise
  aggregation.
- inherit 445: a UNION ALL appendrel's projection pushed into its arms (each arm's own
  expressions), not just the parent's renamed.
- Equal-cost index choices: Postgres prefers the most recently created index on a tie
  (get_relation_info prepends indexes); the engine keeps the first.

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
| copy | 118 | 118 | 100% | yes | yes |  |
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
| create_index | 708 | 707 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| create_index_spgist | 202 | 202 | 100% | yes | yes |  |
| create_view | 343 | 342 | 100% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| index_including | 137 | 137 | 100% | yes | yes |  |
| index_including_gist | 52 | 52 | 100% | yes | yes |  |
| create_aggregate | 60 | 60 | 100% | yes | yes |  |
| create_function_sql | 168 | 167 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| create_cast | 24 | 24 | 100% | yes | yes |  |
| constraints | 607 | 607 | 100% | yes | yes |  |
| triggers | 1041 | 1041 | 100% | no | no |  |
| select | 87 | 86 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| inherit | 934 | 932 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| typed_table | 36 | 36 | 100% | yes | yes |  |
| vacuum | 328 | 328 | 100% | yes | yes |  |
| drop_if_exists | 161 | 161 | 100% | yes | yes |  |
| updatable_views | 1109 | 1103 | 99% | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (5) |
| roleattributes | 80 | 80 | 100% | yes | yes |  |
| create_am | 142 | 142 | 100% | yes | yes |  |
| hash_func | 43 | 43 | 100% | yes | yes |  |
| errors | 87 | 87 | 100% | yes | yes |  |
| infinite_recurse | 8 | 8 | 100% | yes | yes |  |
| sanity_check | 3 | 3 | 100% | yes | yes |  |
| select_into | 67 | 67 | 100% | yes | yes |  |
| select_distinct | 101 | 100 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| select_distinct_on | 23 | 23 | 100% | yes | yes |  |
| select_implicit | 44 | 44 | 100% | yes | yes |  |
| select_having | 23 | 23 | 100% | yes | yes |  |
| subselect | 356 | 345 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (11) |
| union | 198 | 198 | 100% | yes | yes |  |
| case | 64 | 64 | 100% | yes | yes |  |
| join | 925 | 835 | 90% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (86) |
| aggregates | 556 | 548 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (7) |
| transactions | 433 | 433 | 100% | yes | yes |  |
| random | 48 | 48 | 100% | yes | yes |  |
| portals | 354 | 354 | 100% | yes | yes |  |
| arrays | 519 | 519 | 100% | yes | yes |  |
| btree_index | 135 | 135 | 100% | yes | yes |  |
| hash_index | 102 | 101 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| update | 289 | 289 | 100% | yes | yes |  |
| delete | 10 | 10 | 100% | yes | yes |  |
| namespace | 42 | 42 | 100% | yes | yes |  |
| prepared_xacts | 2 | 2 | 100% | yes | yes |  |
| brin | 77 | 77 | 100% | yes | yes |  |
| gin | 59 | 59 | 100% | yes | yes |  |
| gist | 69 | 67 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| spgist | 31 | 31 | 100% | yes | yes |  |
| privileges | 1398 | 1398 | 100% | no | no |  |
| init_privs | 4 | 4 | 100% | yes | yes |  |
| security_label | 28 | 28 | 100% | yes | yes |  |
| collate | 149 | 145 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| matview | 189 | 189 | 100% | yes | yes |  |
| lock | 134 | 134 | 100% | no | no |  |
| replica_identity | 75 | 75 | 100% | yes | yes |  |
| rowsecurity | 1226 | 1220 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (6) |
| object_address | 85 | 85 | 100% | yes | yes |  |
| tablesample | 58 | 58 | 100% | yes | yes |  |
| groupingsets | 217 | 208 | 96% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (9) |
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
| merge | 572 | 570 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| misc_functions | 148 | 148 | 100% | no | no |  |
| sysviews | 29 | 29 | 100% | yes | yes |  |
| tsrf | 74 | 73 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| tid | 41 | 41 | 100% | yes | yes |  |
| tidscan | 49 | 49 | 100% | yes | yes |  |
| tidrangescan | 45 | 45 | 100% | yes | yes |  |
| collate.utf8 | 61 | 61 | 100% | yes | yes |  |
| collate.icu.utf8 | 681 | 673 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| incremental_sort | 120 | 109 | 91% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (11) |
| create_role | 144 | 144 | 100% | yes | yes |  |
| without_overlaps | 674 | 674 | 100% | yes | yes |  |
| generated_virtual | 489 | 483 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (6) |
| rules | 632 | 631 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| psql | 1264 | 1264 | 100% | no | no |  |
| psql_crosstab | 32 | 32 | 100% | yes | yes |  |
| psql_pipeline | 362 | 362 | 100% | yes | yes |  |
| amutils | 10 | 10 | 100% | yes | yes |  |
| stats_ext | 878 | 877 | 100% | yes | no | wrong result `SELECT: values differ` (1) |
| collate.linux.utf8 | 2 | 2 | 100% | yes | yes |  |
| select_parallel | 250 | 243 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (7) |
| write_parallel | 22 | 22 | 100% | yes | yes |  |
| vacuum_parallel | 14 | 14 | 100% | yes | yes |  |
| maintain_every | 16 | 16 | 100% | yes | yes |  |
| publication | 781 | 781 | 100% | yes | yes |  |
| subscription | 179 | 179 | 100% | no | no |  |
| select_views | 52 | 52 | 100% | no | no |  |
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
| window | 400 | 397 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
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
| sqljson | 242 | 242 | 100% | yes | yes |  |
| sqljson_queryfuncs | 318 | 318 | 100% | yes | yes |  |
| sqljson_jsontable | 131 | 130 | 99% | yes | no | wrong result `SELECT: row order differs` (1) |
| plancache | 97 | 97 | 100% | yes | yes |  |
| limit | 85 | 84 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| plpgsql | 941 | 941 | 100% | yes | yes |  |
| copy2 | 290 | 290 | 100% | no | no |  |
| temp | 211 | 211 | 100% | yes | yes |  |
| domain | 486 | 486 | 100% | no | no |  |
| rangefuncs | 428 | 427 | 100% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| prepare | 33 | 33 | 100% | yes | yes |  |
| conversion | 68 | 68 | 100% | no | no |  |
| truncate | 193 | 193 | 100% | yes | yes |  |
| alter_table | 1749 | 1749 | 100% | yes | yes |  |
| sequence | 265 | 265 | 100% | no | no |  |
| polymorphism | 426 | 426 | 100% | yes | yes |  |
| rowtypes | 237 | 231 | 97% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (6) |
| returning | 145 | 143 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| largeobject | 151 | 151 | 100% | yes | yes |  |
| with | 307 | 303 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| xml | 281 | 281 | 100% | yes | yes |  |
| partition_join | 614 | 579 | 94% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (35) |
| partition_prune | 771 | 733 | 95% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (30) |
| reloptions | 66 | 66 | 100% | yes | yes |  |
| hash_part | 29 | 29 | 100% | yes | yes |  |
| indexing | 645 | 645 | 100% | yes | yes |  |
| partition_aggregate | 137 | 137 | 100% | yes | yes |  |
| partition_info | 73 | 71 | 97% | yes | no | wrong result `SELECT: row order differs` (2) |
| tuplesort | 108 | 106 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| explain | 60 | 60 | 100% | yes | yes |  |
| memoize | 73 | 65 | 89% | yes | no | wrong result `SELECT: values differ` (3) |
| stats | 482 | 482 | 100% | yes | yes |  |
| predicate | 42 | 42 | 100% | yes | yes |  |
| numa | 3 | 3 | 100% | yes | yes |  |
| compression | 102 | 102 | 100% | yes | yes |  |
| compression_pglz | 21 | 21 | 100% | no | no |  |
| oidjoins | 1 | 1 | 100% | yes | yes |  |
| event_trigger | 230 | 230 | 100% | yes | yes |  |
| event_trigger_login | 14 | 14 | 100% | yes | yes |  |
| fast_default | 275 | 275 | 100% | yes | yes |  |
| tablespace | 261 | 261 | 100% | yes | yes |  |
