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
alter_table 1745 (`ALTER TABLE tt7 DROP q` "stalled" past the 600 s psql timeout in that run and
the 4 units after it differ). Explained and fixed in round 15 (agent 9, below): no statement
hung; every ALTER TABLE copied the whole catalog once per table, so over the catalog a full run
builds up each took seconds, and alter_table as a whole ran past the per-file timeout, which
happened to fall on that statement.

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
| units matching | 51121 (**99.94%**) |
| units differing | 29 |
| files fully matching | 227 |
| engine output == expected/*.out (pg_regress "ok") | 188 files |
| engine crashes / panics | none |

## Remaining differences, classified

| class | units | % of differing |
|---|---:|---:|
| EXPLAIN plan shape / EXPLAIN-derived output (group C1) | 29 | 100% |
| plan choice showing in results (row order of joins, grouping sets, recursive CTEs; C1 area) | 0 | 0% |
| by design | 0 | 0% |
| name resolution: a nested join's LATERAL reference to an earlier FROM item | 0 | 0% |
| heap-layout simulation, qual evaluation order | 0 | 0% |

No difference is PostgreSQL nondeterminism: every remaining unit is reproducible.

### EXPLAIN (group C1) — 29 units
join (19), partition_join (9), partition_prune (1)

Includes EXPLAIN-derived values: `explain_filter`/`explain_analyze_*` helpers, `check_estimated_rows`
(stats_ext), hash-join batch counts (join_hash), `EXPLAIN (SETTINGS)`.

### Plan choice visible in results (C1 area) — 0 units

- sqljson_jsontable 101 (the join order: a join with an unsatisfied LATERAL reference) matches
  since round 16, agent 17. (join's FULL JOIN row order units 77, 85 and 459 match now; join_hash's batch
  counts 72 / 237 since round 16, agent 14.)

### Name resolution — 0 units
Fixed in round 12: join 740–743, 745 and 791 match; 744 now runs and differs only in its plan
(counted under C1).

### By design — 0 units
aggregates `balk` matches since round 16, agent 14: an aggregation planned as a Finalize
Aggregate over partial ones runs the user-defined aggregates' combine functions.

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

## Round 16, agent 29 (rule actions over a view: range table order, NEW in the action's SET; whole-row Vars of subqueries)

Subset runs of 45 files (returning, rules, updatable_views, with, triggers, merge, insert,
update, delete, insert_conflict, portals, foreign_key, privileges, matview, generated_*, the
planner-heavy files and their prerequisites), unit by unit: on e7e37e6 19297 -> 19298 with this
batch (returning 105 fixed, nothing else moved); rebased onto a8de4bd, a run of that commit
against one of this batch: 19306 -> 19307, only returning 105 moved (returning also equals
`expected/returning.out` now). Rebased onto 35d7dbb (agent 27's join 442): 19307 -> 19308 against that commit's run, again only returning 105.
Rebased onto dc5a6a2 (agent 26's batch): 19311 -> 19312 against that commit's run, only returning 105 (rowsecurity re-run alone: a concurrent run on the reference held its roles). Rebased onto ad16655 (agent 26's second batch): against the dc5a6a2 run, exactly agent 26's join 662, 663, 671, 706 and returning 105 moved (19316). The table goes to
51121. Crate tests and clippy pass.

| file | fixed units |
|---|---|
| returning | 105 (returning matches fully) |

What changed:

- **The rule action's range table** (`binder/rules.rs`). rewriteRuleAction puts the statement's
  range table first, the action's after it, and a view the statement names is expanded and
  pulled up only later (fireRIRrules, pull_up_subqueries): its relations are appended after
  the action's own entries, and inheritance children after all of them. The `*RULE*` FROM item
  (OLD / NEW) now stands where Postgres has OLD: last in the action's FROM list when it is OLD
  alone (an UPDATE / DELETE without FROM items of its own: transformRuleStmt appends OLD to the
  action's jointree), first when it also carries the statement's FROM items (adjustJoinTreeList
  puts the statement's jointree first) or an item of the action reads OLD / NEW (LATERAL). The
  relations of the statement's views and FROM subqueries are stamped after the action's own
  (`Binder::restamp_rti_below`), so the names follow (the action's `joinme`, then the view's
  `joinme_1`; the target `foo_1`, the view's `foo_2`), and with them the join search's order:
  the cost tie between the two `joinme` scans goes the way Postgres's does, the first pair made
  winning, with no tie-breaking change needed.
- **NEW values in the action's SET** (`paths/joinpath/lift.rs`, now agent 25's composition of
  stacked projections, which this batch had too): `*RULE*`'s projection of OLD / NEW over the
  view's select list over its join is pulled up into the action's join tree, so `new.f3` is
  `(foo_2.f3 + 1)` computed by the top join (ReplaceVarsFromTargetList, pullup_replace_vars),
  not below it; EXPLAIN shows a SET value read from a projection merged into the join as that
  join computes it (`(x + 1)`, not the reference `((x + 1))`; `computed_names`), also for a
  plain `UPDATE ... SET a = v.x FROM (SELECT f + 1 AS x ...) v`.
- **The processed target list over an inheritance parent** (`explain.rs` `expanded_tlist`,
  preprocess_targetlist then expand_inherited_rtentry): the new values, the row marks' ctids,
  what RETURNING reads of the other relations, then, per inheritance parent in range table
  order, a row mark's tableoid or the target's row identity (tableoid, ctid). RETURNING's
  columns were missing from an inherited target's join, and another parent's tableoid came
  before the target's; a DELETE's join now carries what RETURNING reads too.
- **A data-modifying statement's join target lists** (`junk_layout`): a join's columns in its
  relations' range table order (build_joinrel_tlist from the first input pair the join search
  makes, which pairs relations in that order), not its outer side's first.
- **relhassubclass in estimate_rel_size** (`costs.rs` `rel_size`): a never-vacuumed table
  whose relhassubclass is set (a child was dropped, no VACUUM since) gets no 10-page minimum
  (table_block_relation_estimate_size), so its scans estimate its real pages (107 rows where
  the engine had 1070).
- **Whole-row Vars of a subquery not pulled up** (`binder/select.rs`, `explain.rs`): a FROM
  subquery Postgres keeps (OFFSET 0, LIMIT, DISTINCT, GROUP BY, ...) has its whole row as a Var
  of its Subquery Scan, `s.*` (`row_to_json(s.*)`), where the engine showed `ROW(s.a, s.b)`, and
  its IS [NOT] NULL test stays whole (`(s.* IS NULL)`), as a CTE's does; a pulled-up one's is
  still a ROW() of its outputs with the test expanded, as in Postgres.

Goldens: `a29_rule_view_actions`, `a29_whole_row_subquery` (recorded from PostgreSQL 18.6).

Left, and why:

- A FROM subquery's whole row read above a join (`SELECT x.a, s FROM wt x, (... OFFSET 0) s
  WHERE ...`): Postgres keeps its Subquery Scan and passes `s.*` up as one value (`Output:
  s.*, s.a` on the scan and the Sort); the engine's whole row is a ROW() of the columns, which
  go up one by one, so the subquery's scan is trivial and goes. Needs a whole-row column in the
  plan (or EXPLAIN's target lists carrying one), HANDOFF's "a whole-row Var isn't passed up".
- A base relation's columns in its reltarget order when a join clause reads them
  (build_base_rel_tlists after the final target list, then the clauses as deconstruct_jointree
  distributes them: a pulled-up view's ON clause before the WHERE's): `a29_foo_1.ctid,
  a29_foo_1.f2, a29_foo_1.f1` where the engine shows `ctid, f1, f2` (agent 28's per-scan order).
- A rule-rewritten DELETE's RETURNING column of another relation (`a29_plain.a`) is missing
  from the joins and scan below the top join (EXPLAIN's needed masks see the action's own
  RETURNING, not the statement's over it).
- RETURNING reading a pulled-up subquery's computed output (`RETURNING v.x` for `x = f + 1`):
  Postgres computes the expression in the ModifyTable over `f`, which the join returns; the
  engine shows the join returning the computed value, referenced as `((f + 1))`.
- A SELECT scan of a table whose relhassubclass is set but has no children returns its
  physical target list; Postgres's is an appendrel member (no physical target list).

Units this batch moves for other agents: none (the rebased run shows only returning 105).

## Round 16, agent 21 (expression members of equivalence classes; IN over a join pulled up, unique-ified joins)

Measured in full runs of the whole schedule in one database (`PGREGRESS_TIMEOUT=400`), unit by
unit: the base (9b0ac33) measured the table's 51069; the class change alone 51070 (partition_join
74 fixed, nothing else moved); with the IN pull-up 51072 (88, 92 too), nothing broken; rebased
onto b8a205b 51085 (the other agents' fixes plus these three, nothing that matched in the base
broken); rebased onto cce1f9c and again onto e7e37e6 51105, the summary's count: these three over the
others' 51102, nothing that matched in the base broken; rebased onto 01368ea, a subset run of the
planner-heavy files (agent 17's list) unit by unit against the e7e37e6 run: only the other agents'
fixes changed (join 424 430 440 450 548, partition_prune 265), so 51111. Crate tests and clippy
on each.

| file | fixed units |
|---|---|
| partition_join | 74, 88, 92 |

What changed:

- **Expression members** (`paths/joinpath.rs` `XMember`, equivclass.c's add_eq_member with a
  non-Var em_expr): a mergeable `col = expr` clause whose expression reads one other FROM item
  (nothing volatile, no PlaceHolderVar, no sub-select) puts the expression in the column's
  class; the same expression in another such clause is the same member (the classes merge).
  Members are numbered after the group's columns (`mem_id`, `m_ec`, `m_leaf`), so a class of
  columns only behaves exactly as before. They take part in what classes drive:
  generate_join_implied_equalities' derived clauses (`t2.b = (t3.a + t3.b) / 2` from `t1.a =
  t2.b` and `t1.a = (t3.a + t3.b) / 2`), one join clause per class between the sides' best
  members (a Var before an expression, generate_join_implied_equalities_normal's score),
  generate_base_implied_equalities_no_const within one item, a parameterized index scan's
  clauses on an expression index (match_eclass_clauses_to_index, the clause oriented indexed
  member first, as its first derivation has it, also when the join search makes it first:
  agent 22's `derives`), the columns kept up to a class's last join. Within one item a
  restriction between an expression member and the member before it takes the `=` of their
  types when no clause has them; failing that the class keeps no expression members (its
  clauses stay as written), since the join takes one clause per class (a cross-type
  `coalesce(thousand, 123) = q1` in `a5_oj_classes` lost its filter that way during
  development). partition_join 74: the index scan of `prt1_e` is parameterized by `t2` (`Index Cond: (((a + b)
  / 2) = t2_1.b)`) under the Join Filter on `t1_1.a`, as Postgres's.
- **IN over a join** (`binder/sublink.rs` `any_join`, `binder/expr.rs`): an IN / ANY sub-select
  that is one plain SELECT is planned right after binding, as before, but its plan as bound is
  kept (`Binder::unplanned_subs`): pull_up_sublinks pulls a join tree up from that one
  (Postgres pulls sublinks up before planning anything, so a partitioned join isn't joined
  partitionwise yet), the sub-select's relations the semi join's right side
  (pull_up_simple_subquery over convert_ANY_sublink_to_join's subquery entry); it was a
  subquery of its own (`ANY_subquery`) or, planned partitionwise, a hashed SubPlan.
  `sub_join` takes an inheritance / partition tree's members too (also for EXISTS).
- **A unique-ified join**: the semi join's right side made unique when it is a join of
  several items, and a semi join's clauses parameterizing its left side, landed in agent 23's
  round 16 section while this was being verified (this round had the same create_unique_path
  over a join relation and the same movability; its version is kept). Added here: a nested
  loop over a unique-ified side takes as parameters only the side's keys (the Unique returns
  nothing else: an inner path reading another of its columns, an expression member's, read a
  column the side doesn't return). 88: Nested Loop over the HashAggregate of each partition's
  `prt2 ⋉ prt1_e`, the index scan of `prt1` under it.
- A semi or anti join merges on an expression of its side too (the engine kept them to columns
  while it couldn't unique-ify on expressions): 92's inner `Merge Semi Join`.
- A Merge Append of partitionwise child joins wins over sorting their Append when its total is
  fuzzily as cheap and it starts sooner (add_path), 92's top.
- EXPLAIN names (`explain.rs` `inherited_parents_in_order`): relations of one query level that
  share a reference name with two or more inheritance parents among them (a pulled-up sub-select
  reusing an alias) are named in range table order, parents (partitioned tables joined
  partitionwise too, with no Append of their own) first, then each parent's children as
  expand_inherited_rtentry appends them, parent by parent, in bound order (`t1`, `t1_1`, `t1_2`,
  then `t1_3`…).

Goldens: `tests/sql/a21_ec_expr_members.sql`, `a21_in_join_pullup.sql` (plans and rows,
partitionwise and not, merge-only, correlated IN / NOT IN / EXISTS over joins).

Left open, and why:

- partition_join 86: now pulled up and joined partitionwise (names as Postgres's), but each
  child is a Hash Right Semi Join where Postgres runs a Nested Loop over a HashAggregate of the
  right side's join with an index scan parameterized by the expression member `(t2.a + t2.b) /
  2`. Postgres's HashAggregate passes the non-key columns `t2.a`, `t2.b` through (a unique
  path's target is the relation's reltarget); the engine's hashed Distinct compares every
  column it returns (agent 15's note), so a unique-ified side returns only its keys and only
  parameters among them may come from it. Needs a hashed DISTINCT ON (pass-through columns) in
  Plan::Distinct, its executor, EXPLAIN and costs.
- Expression members only in groups of inner and semi joins: with an outer or anti join in the
  group none are made (an expression of a nullable side's columns would need its nullingrels
  to stay above the join); the clauses stay as written there.
- Class member order (agent 24's finding: `FROM t1, t2, t3 WHERE t2.x = t1.a AND t3.p = t1.a`
  joins `{t1, t2}` to `t3` on Postgres's `t3.p = t2.x`, the engine's `t1.a = t3.p`): Postgres's
  members come in the order the WHERE clauses as written added them, each clause's left
  operand first (process_equivalence), and generate_join_implied_equalities_normal takes each
  side's first. The join search gets the clauses rebuilt from the binder's join tree: an
  inner join's equalities from its hash keys, the join's left side first, and the clauses of
  the deepest joins first, not in WHERE order. Tried here: putting a clause written the other
  way round (`hash_swapped`) back in its written order fixes that query but breaks
  `a24_rowmark_order_widths` (`s.x = rm1.a AND rm3.p = rm1.a`, whose `rm3.p = rm1.a` sits in a
  deeper join and so comes first) and `a22_ec_derives`. Needs each clause's place in the
  WHERE list kept when `add_filter` distributes it (a per-key position in JoinMeta beside
  `hash_swapped`), then members ordered by place and orientation.
- Agent 10's nondeterministic-collation constant across a join: a constant, not an expression
  member (`ec_constants`, binder side); classes of nondeterministic collations take no
  expression members here.

## Round 16, agent 27 (SubPlan copies per place, AlternativeSubPlan choices per copy, numbering and naming order)

Subset runs of the planner-heavy files (create_index, create_misc, create_view, select, union,
subselect, join, aggregates, inherit, insert_conflict, limit, returning, updatable_views,
merge, with, rules, rowsecurity, select_views, select_distinct, equivclass, partition_join,
partition_prune, partition_aggregate, select_parallel, incremental_sort, memoize, join_hash,
explain, stats_ext, groupingsets, window, tsrf, rangefuncs, sqljson_jsontable, collate and
test_setup), unit by unit against a baseline run of the same subset on 773f83c (13383 ->
13384 of 13441), then again after rebasing onto e7e37e6 and deee52e against their own
baselines (13393 -> 13394, 13398 -> 13399 of 13442): 1 unit fixed, none broken, no other
agent's units moved. Crate tests and clippy on each.

| file | fixed units |
|---|---|
| partition_prune | 265 |
| join | 442 (second batch) |

What changed:

- **A pulled-up subquery's sub-select output is copied into each place reading it**
  (pullup_replace_vars; make_subplan for each copy). A join group's subquery over one table
  or over a join (`lift.rs`, `flatten_join_subquery`) now moves an output holding a sub-select
  up like any other when no outer join of the group nulls it (as written: else it stays a
  PlaceHolderVar, one SubPlan), and `copy_moved_subselects` gives every place after the first
  (the group's target list, then the join tree's quals in preprocess_qual_conditions' order) a
  copy: an id of its own and plan nodes of its own, its own sub-selects copies too
  (`sublinkcopy::make_copy`, `paths::make_subquery_copy`). A WHERE clause pushed through a
  FROM subquery's projection reads a copy of its own (`optimize::output_copy`; `projectable`
  takes sub-select outputs outside PlaceHolderVar semantics, nothing volatile in them). EXPLAIN's
  split of a sub-select a projection reads twice uses the same deep copies (its InitPlans were
  shared). Each copy runs where it is read: `EXPLAIN ANALYZE` shows the loops of each.
- A sub-select in a join clause is no volatile function to a parameterized path
  (contain_volatile_functions looks at a SubPlan's arguments: `param_paths`, `param_cands`
  use `expr_volatile_deep`): `id > (EXISTS(...))::int` is an index condition of the inner
  side, as with a column.
- **AlternativeSubPlan per copy** (setrefs.c fix_alternative_subplan): each place picks plain
  or hashed by its NUM_EXEC: a join's clauses (Join Filter, Hash / Merge Cond) for twice the
  join's rows, an index scan's shown Index Cond (indexqualorig) for twice its rows, an
  index-only scan's (indexqual) for one run, a Bitmap Heap Scan's Recheck Cond for twice its
  rows, a Bitmap Index Scan's (indexqualorig) for twice its rows.
- **Where a SubPlan shows** (ExplainSubPlans prints each plan_id once, under the first node
  whose planstate initialized it): a nested loop parameter holding a sub-select (the inner
  clause's copy, which the engine passes in as the parameter's value) is shown and registered
  at each place reading it (`inline_sub_params`), not where the parameter is named; an
  index-only scan's index condition and a Bitmap Index Scan's are labeled but not attached
  without ANALYZE (`Builder::detach`, `unattached`: ExecInitIndexOnlyScan stops before the
  runtime keys under EXPLAIN_ONLY), while the index-only scan's recheckqual registers its own
  choice, attached; an inheritance Append with run-time pruning registers its exec pruning
  steps' values (the partition key's clauses over the nested loop's values, fixed for one
  run, register_partpruneinfo), attached to the Append, which ExecInitAppend initializes
  before its members. A scan's and a join's target-list SubPlans come before their quals'
  (ExecInit* assigns the projection first).
- **Numbering: the target list first** (`Subquery::tlist`, set by the binder for the select
  list, ORDER BY, DISTINCT ON, window clauses, an UPDATE's SET, a one-row INSERT's values,
  RETURNING; `renumbering`): a level's target-list sub-selects (each with the sub-selects in
  it and its hashed alternative) are numbered before its other own ones (quals, HAVING, ...),
  as subquery_planner preprocesses the target list first. Not a MERGE's own level (its
  actions are preprocessed after the quals); GROUPING() argument copies count as the target
  list's.
- **Naming in number order**: setrefs flattens the SubPlans' range tables after the main
  query's, in plan_id order, and EXPLAIN names only the relations of plans it shows. Once a
  build knows the numbers, the sub-selects' relations are named in that order
  (`assign_refnames_skip`'s `ranks`, `Builder::sub_ranks`): a nested one before the one it is
  in, an unshown plain alternative not at all. A hashed alternative shown beside its plain
  one is a range table entry of its own (`fresh_hashed`): `tinner_2` under `tinner_1`.

Golden: `tests/sql/a27_subplan_copies.sql`.

Second batch (join 442; subset run on deee52e plus both batches against deee52e's baseline:
13398 -> 13400, then rebased onto a8de4bd against its baseline: 13402 -> 13403; none broken,
no other agent's units moved; crate tests and clippy on each):

- **reconsider_outer_join_clauses** (`optimize::join`, `Search::oj_const_redundant`): a left
  (or right) join's `outer = inner` whose outer column its side equates with a constant, in
  its own join domain or a lower one (`restrictions(.., lower)`: the nullable sides' filters
  too, reconsider_outer_join_clause matching the column's class whatever its domain), gives
  the nullable side `inner = constant` (the ON clause pushed into it as any other), valid in
  every join order. The clause itself stays in the plan; the join search drops it wherever
  it is Postgres's constant-TRUE replacement: both columns equated with the same constant in
  their relations' own restrictions and neither relation null-extended by an outer join made
  inside its side (`tt3 ⟕ tt4` made first, the outer join identity 3 order; joined above
  `tt1 ⟕ tt3` the clause stays, as the clone with tt3's nulling does in Postgres). Its
  selectivity is 1 there, so the join sizes are Postgres's, and `tt4.f1` equal to a constant
  is one distinct value for the Memoize above (442's `Memoize` over the LATERAL subquery
  follows from that).
- **A constant class through an outer join's preserved side and over a LATERAL join**
  (`input_restrictions`, add_filter's join case): an inner join's inputs' restrictions now
  include a left join's left side's (a right join's right side's), the same join domain, and
  a LATERAL join takes its left side's (not the LATERAL side's, which it doesn't look into):
  `tt1.f1 = 'foo'` below `tt1 ⟕ ...` and the WHERE's `tt1.f1 = ss1.c0` give `ss1.c0 =
  'foo'`, the LIMIT subquery's Subquery Scan filter, and the join clause goes
  (generate_base_implied_equalities_const).

Golden: `tests/sql/a27_outer_join_constants.sql` (rows checked for null-extended outer rows
too).

For agent 30 (join 362 / 363, PlaceHolderVars with sub-selects), how a pulled-up output gets
its own copy:

- In a join group, `lift` (`paths/joinpath/lift.rs`) decides per output of a pulled-up
  subquery whether it moves up (`up`): one holding a sub-select moves when no outer join of
  the group nulls it as written (`nulled == 0 && movable_fromless`), else it stays computed
  in the subquery's projection (a PlaceHolderVar: one SubPlan, read by column). `note_moved`
  records the moved outputs' sub-select ids, and the map (`Lifted::map`) puts the output's
  expression in every place reading it (`subst`), at first all with the same id.
- `copy_moved_subselects` (called from `lift_group`) then walks the places in
  subquery_planner's order, the group's target list (the map) first, then the join tree's
  quals (a join's inputs before its own clauses), and every occurrence of a recorded id after
  the first becomes a copy: `sublinkcopy::make_copy` gives it a fresh id and deep-copies its
  plan, its own sub-selects included (planner MIN / MAX InitPlans keep planner ids), so each
  copy is its own SubPlan / InitPlan in EXPLAIN and its own cache entry in the executor. The
  copy's `Subquery::tlist` / `qual` say which clause it is in, for the numbering.
- Elsewhere (`binder::optimize::output_copy` for a qual pushed into a FROM subquery,
  `explain::split_shared_subqueries` for a projection reading one twice) the same
  `paths::make_subquery_copy` makes the copy. For a PlaceHolderVar whose expression holds a
  sub-select, Postgres plans a copy per place too (preprocess_expression processes the
  PlaceHolderVar's contents in each clause: 362's unused `InitPlan 2`); calling
  `make_subquery_copy` on each place's PlaceHolderVar expression besides the one evaluated
  gives that, numbered in place (target list first). 362 / 363 are unchanged by this batch.

Left, and why (none fails a unit of the subset):

- EXPLAIN ANALYZE of a pulled-up EXISTS read as an index condition: Postgres evaluates the
  index condition's plain SubPlan in each member's scan and in the Append's pruning
  (`loops=15` for 5 outer rows over 2 partitions), the engine once per outer row as the nested
  loop's parameter (`loops=5`); and the hashed alternative EXPLAIN picks is planned for the
  display only, so ANALYZE shows it `never executed` (the executor runs the plain plan).
- Copies are made where the engine substitutes the output (the join group's lifting, a
  qual pushed into a FROM subquery). A projection reading the same sub-select output twice
  over a single relation still computes it once in the executor (EXPLAIN splits the copies
  for display only), and an ORDER BY over an output a join clause also reads sees one value
  (Postgres's two copies are different equivalence-class members: `Sort Key: ((SubPlan 2)),
  t2.a`, the engine drops `t2.a`).
- An index condition's other operand is costed nothing (index_other_operands_eval_cost adds
  its evaluation, a SubPlan's or `x + 1`'s, to the index scan's startup), and a Bitmap Heap
  Scan's recheck of a parameterized clause isn't charged per row: Postgres prefers an index
  scan over a bitmap for `id > touter.f1 + 1` (startup 0.16), the engine the bitmap.
- A FROM subquery's plain output nothing above reads stays in its scan's target list when the
  subquery also has a sub-select output (`Output: (SubPlan 1), t1.a`), before this batch too.
- reconsider_outer_join_clauses for anti joins and FULL JOIN's merged columns over a lower
  domain's constant: not done (only left / right join clauses; FULL JOIN's USING column with
  a WHERE constant was done before, `reconsider_full_join_clause`).
- The rest of subquery_planner's numbering order (FROM subqueries planned after the level's
  expressions, MIN / MAX InitPlans after both): not applied, no unit of the subset differs by
  it (the target list's move is the one that did, 265).
- A multi-key hashed EXISTS's test expression shows `(a = (hashed SubPlan n).col1 AND b = ...)`
  for Postgres's `((a = ...) AND (b = ...))`.

## Round 16, agent 25 (column-selecting subqueries pulled up, WHERE over pulled-up subqueries, LATERAL join restrictions, constant classes, `name = text` merge keys)

Measured in subset runs of 35 planner-heavy files (create_index, create_misc, create_view,
select, union, subselect, join, aggregates, inherit, insert_conflict, limit, returning,
updatable_views, merge, with, rules, rowsecurity, select_views, select_distinct, equivclass,
partition_join, partition_prune, partition_aggregate, select_parallel, incremental_sort,
memoize, join_hash, explain, stats_ext, groupingsets, window, tsrf, rangefuncs,
sqljson_jsontable, collate, with test_setup; 13442 units), unit by unit against a run of the
base cce1f9c (13394 -> 13399): 5 units fixed, none broken. Crate tests and clippy on this state.

| file | fixed units |
|---|---|
| join | 424, 430, 440, 450, 548 |

What changed:

- **A subquery selecting some of its table's columns is pulled up** (`lift.rs`, join 424):
  `(select unique2 as j from tenk1) ss` stayed a leaf planned for itself (lift kept an
  all-columns projection "as it is"), so the join search had no parameterized paths into it:
  `int4_tbl s join (select unique2 from tenk1) ss on ss.unique2 = s.f1` was a Hash Join over
  an Index Only Scan of all 10000 rows where Postgres runs 5 index probes. Like any simple
  subquery (pull_up_simple_subquery), it is now the table in the join tree, its columns
  read through the map.
- **A WHERE over a pulled-up subquery** (join 548): a clause pushed down onto a single-table
  subquery (`Filter` over its projection) kept the subquery a leaf; it is now pulled up, the
  clause over its outputs as the query reads them (a PlaceHolderVar's restricting the
  relation it is evaluated at, distribute_qual_to_rels), so the outer join's own clause reads
  the expression itself (`Join Filter: (t0.f1 = CASE ...)`) and the WHERE is t1's filter. A
  join subquery planned for itself first (a parameterized nested loop inside) was refused:
  `reads_outer` counted the nested loop's own parameters as outer values (now only values
  from outside it, `Outer` of a deeper level, its parameters' and clauses' own), its several
  projections (column order, a removed join's constants) are composed, and a CASE's operand
  placeholder in its own WHEN tests no longer makes it unmovable. Whether an output needs a
  PlaceHolderVar is judged after its own subqueries were pulled up into it.
- **has_legal_joinclause with join_is_legal's LATERAL checks** (`outerjoins.rs`, join 440):
  a clause to an item whose LATERAL references can't be met from there (`t1.f1 = ss2.f1`
  with `ss2` reading `ss1`, which reads the nullable `i8`: join_plus_rhs) is no legal one, so
  the left join's order restriction makes `t1 ⟕ i8` at level 2 and the search finishes with
  all four items (it fell back to planning `ss2` above the rest, without a Memoize).
- **Memoize keys over a subquery's outputs** (`joins::memo_ndistinct_keyed`, join 440):
  estimate_num_groups takes a subquery output's relation to be the subquery (examine_variable's
  vardata->rel), its tuples for the distinct count when there are no statistics (fewer than
  DEFAULT_NUM_DISTINCT) and as the clamp: `Cache Key: (i8.q1), t2.f1` over a one-row LIMIT
  subquery is one value.
- **A PlaceHolderVar's join selectivity** (`Search::join_var`, join 430): examine_variable
  over a PlaceHolderVar takes the relations of its ph_eval_at; evaluated at one, that
  relation without statistics, else none, the default 200 (it counted a constant one as one
  value, which is estimate_num_groups', not eqjoinsel's): `(NULL::integer) = i8b1.q2` keeps 5
  rows, not 25, and `i8 ⟕ i4` stays at the top.
- **Classes with a constant relate their items** (`Search::const_ecs`, join 450):
  `i8.q1 = ss1.c2` with `ss1.c2 = 0` puts `i8` and `i43` in one class with `0` and no join
  clause; have_relevant_eclass_joinclause still counts them joinable (has_eclass_joins), so the
  search makes `i8 × i43` as Postgres does (only in a group without outer joins: one join
  domain). A PlaceHolderVar a join computes comes after the join relation's columns in that
  relation's own order (build_joinrel_tlist over the first pair it was made from).
- **`name = text` as a merge key** (agent 19's open item): the clause (nameeqtext /
  texteqname, in the btree text_ops family) is mergeable; its class sorts under the comparison's
  collation, the name side's C, so a text side sorts `COLLATE "C"` (`Sort Key: tx.i COLLATE
  "C"`), an outer join's sides too (initialize_mergeclause_eclasses makes each side's class with
  the clause's collation). A text column's own order (an index, an ORDER BY) isn't that class's
  unless the column is `COLLATE "C"` (`Search::order_ec`), and an index of a text column of
  another collation doesn't take the clause as an index qual (IndexCollMatchesExprColl).

Goldens: `tests/sql/a25_join_parity.sql`, `a25_name_text_merge.sql`.

Still open, and why:

- join 426: the USING column `x` of `ss1 left join ss2 using (x)` (`(ss1.x)::bigint`, ss1's
  `123`) read above `i0 left join (...) ss0` is Postgres's PlaceHolderVar over the join alias
  expansion (flatten_join_alias_vars wraps a non-Var expansion of an alias Var with
  varnullingrels, phrels the join's relations: `'123'::bigint` computed at `i1 ⟕ i2`). The
  engine binds join aliases as expressions of the joined columns, so `123` is ss1's
  PlaceHolderVar at `i1`'s scan and the cast is above. Treating merged columns as a
  projection over the join would make every USING join with a cast or COALESCE an opaque
  subquery leaf; not done.
- join 332: the WHERE `ss1.loc = ss1.lat` (`lat` a LATERAL value, a PlaceHolderVar with
  ph_eval_at `{i81, i42}` under the right join as written) is pushed into `i42`'s scan by the
  binder (add_filter_phv takes a LATERAL value as a plain value), and even kept above the
  subquery it becomes `i42.f1 = i41.f1` when the outer level gathers the planned join again:
  the join clause needs a required-relations mark that survives regathering. Tried and
  reverted.
- join 345, 360, 362 / 363 (362 / 363 now also wait for agent 27's per-place SubPlan copies),
  partition_join 41 / 204: as agent 19 left them. 41 / 204 need PlaceHolderVars with
  ph_lateral (a LATERAL join subquery's `least(t1.a, t2.a, t3.b)` evaluated at `t2 ⋈ t3`,
  that join parameterized by `t1`); the engine doesn't pull a LATERAL join subquery up, and
  its PlaceHolderVars have no lateral set to make their evaluation join LATERAL-dependent.
- Seen while probing: `name = text` with a text column declared `COLLATE "C"` shows its
  merge / hash condition relabeled (`(tc.i)::text = nk.s`; Postgres `tc.i = nk.s`: the
  column has the class's collation already); EXPLAIN's `name_text_relabel` doesn't see column
  collations.

## Round 16, agent 26 (partial partitionwise grouping by add_path, filters over joins, parallel-safe nested loop parameters, ANY over outer values)

The units no other agent owned (join 662–808, partition_join 52–608). Measured in subset runs
of 33 planner-heavy files (test_setup, create_misc, create_index, create_view, select, union,
subselect, join, aggregates, inherit, partition_join, partition_prune, partition_aggregate,
equivclass, memoize, with, rowsecurity, updatable_views, select_distinct, groupingsets,
incremental_sort, merge, returning, rules, insert_conflict, join_hash, select_parallel,
rangefuncs, tsrf, limit, window, stats_ext, sqljson_jsontable), unit by unit against a run of
the branch head it is on (f517f1c: 13121 -> 13125; rebased onto e7e37e6: 13132 -> 13135, the
one loss test_setup 7, the shared reference's `regress_tblspace` from a concurrent run; onto
deee52e: 13137 -> 13141; onto 01368ea: 13138 -> 13142; onto a8de4bd: 13141 -> 13145; onto 35d7dbb: 13142 -> 13146): 4 units fixed, none lost; no other agent's units moved. Crate tests
and clippy on each.

| file | fixed units |
|---|---|
| join | 781 |
| partition_join | 52, 588, 608 |

What changed:

- **A filter merged into a join** (a WHERE over a full join's merged column) costs its quals on
  the tuples the join's other quals are checked on, final_cost_mergejoin's mergejointuples
  (hashjoin's hashjointuples, nestloop's outer x inner rows), not on the rows the join puts
  out (`ExNode::qual_tuples`): the Merge Full Join under `WHERE a BETWEEN 490 AND 510` costs
  12.74, not 13.35.
- **Partitionwise grouping competes as add_path has it** (`pwagg.rs`): the serial grouping's
  paths, then the partitionwise one, through compare_path_costs_fuzzily and the pathkeys; one
  fuzzily as cheap that starts sooner removes them, and the cheapest of the paths left wins
  (partition_join 52: the partial grouping over a full join, 31.77..32.63, beats the serial
  Group's 32.17..32.43). A Group finalizing partial groups has no per-group tuple cost
  (cost_group), and the partitions' groupings are named as the serial grouping's sort keys,
  a key computed below them wrapped (`(COALESCE(prt1.a, p2.a))`).
- **Nested loop parameters are no parallel hazard** (`parallel::plan_hazard`): an inner side's
  `Outer` references bound by the join's parameters are PARAM_EXECs set in the process running
  the join (max_parallel_hazard restricts only those set outside the plan), so
  debug_parallel_query's single-copy Gather goes over parameterized nested loops and Memoize
  (partition_join 608).
- **convert_ANY_sublink_to_join over a sub-select reading only queries further up** (join
  781): a sub-select that isn't simple (OFFSET 0) and reads none of the query's own rows, only
  an outer query's values (here a LATERAL subquery's reference to the VALUES list), joins as a
  subquery of its own, ANY_subquery, those values in place (`sublink::params_in_place`); the
  Subquery Scan goes as trivial, as in Postgres: a Nested Loop Semi Join over a Materialize of
  the index scan parameterized by `"*VALUES*".column2`.
- **A qual over a PlaceHolderVar is its relation's restriction** (pull_varnos counts its
  ph_eval_at): a pulled-up subquery's constant output an outer join nulled as written, the join
  then reduced by the WHERE, shows `Filter: (1 = 1)` on the scan, not a gating Result's
  One-Time Filter (no unit by itself: join 674 / 681 then differ by self-join elimination only).
- **Sort keys over a partitionwise join's Append** name a sub-partitioned child's members by
  their topmost parent (ruleutils descends to each Append's first child, and a Var whose
  topmost inheritance parent is among the Appends' apprelids prints as that parent's:
  get_variable's appendparents), `Sort Key: t1.a` where the child join shows `t1_2.a`
  (partition_join 588).

Golden: `tests/sql/a26_pw_parallel_sublinks.sql`.

Second batch (the same subset against the first batch's binary on 35d7dbb: 13146 -> 13150,
none lost; plus 33 other files with joins, create_table, collate.icu.utf8, create_table_like,
gin, gist, indexing, jsonpath, numeric, tablesample, plpgsql, stats_import, tidrangescan,
arrays, json, jsonb, sqljson, xml, foreign_key, triggers, update, delete, privileges,
create_view, select_views, join_hash, write_parallel, explain, tuplesort, generated_virtual,
select_distinct_on, create_function_sql: unchanged):

| file | fixed units |
|---|---|
| join | 662, 663, 671, 706 |

- **join_collapse_limit** (deconstruct_recurse): an inner join written as JOIN syntax
  (`JoinMeta::explicit`, set by the binder) whose sides together are more items than
  join_collapse_limit isn't folded into the join problem above it; a side of several items is
  a leaf of its own, planned as its group (`joinpath::gather`, `joinlist_len`). At 1, join
  662 / 663 keep `(k1 ⋈ k2) ⋈ j2`, j1 removed by self-join elimination inside its sub-problem.
  (A FROM list's items are folded within from_collapse_limit, still not read: the suite's
  uses of it don't show a difference.)
- **Self-join elimination runs a group again while a pass removes a relation**
  (remove_self_joins_recurse's loop): join 671's c2 goes into c3, and the clause
  `c3.id * c3.id = c1.id * c1.id` then pairs c1 with c3 on the unique index over `(id * id)`,
  `Filter: ((id * id) IS NOT NULL)` on the one scan left. (A small change in agent 23's
  `self_joins`; their units are unchanged.)
- **neqjoinsel**: a built-in `<>` between two items' values is one minus the negator's
  eqjoinsel (for a semi / anti join, one minus the outer side's null fraction), not
  `1 - DEFAULT_EQ_SEL` (`Search::clause_sel_raw`): `n1.a <> n2.a` sizes the join 3 rows, not 4.
- **make_rels_by_clauseless_joins**: a rel with no join clause joins every base rel at the
  first level, those before it too (make_rels_by_clause_joins alone starts past it), in an
  inner group or one whose make_join_rel checks join_is_legal; a two-item outer join keeps its
  sides' order (trying the other order there swapped an anti join's sides: wrong rows the subset
  run caught). join 706: `sl ⋈ n2` first, 2.15 for 2.17.

Golden: `tests/sql/a26_collapse_sje_neq.sql`.

Looked at and left (why):

- join 674 / 681: Postgres removes the self join although one side is a pulled-up subquery
  computing a PlaceHolderVar ("PHVs should not impose any constraints"); the engine's pair
  test takes plain scans only (its leaf is the PlaceHolderVar projection over the scan).
  675 / 676 (the kept scan's target list after SJE: its own Vars, then the PlaceHolderVars
  add_placeholders_to_base_rels adds later) are agent 23's self-join target lists (620).
- join 691: the rule action's range table order (the old relation `sj` before the target
  `sj_1`, so the class clause is `sj.a = sj_1.a`): agent 24's returning 105.
- join 765: a whole-row Var passed up as one value (`Output: i.f1, i.*` under the nested
  loop that reads `i.*` as a parameter), HANDOFF's known gap.
- join 776: the LATERAL subquery with an outer join inside isn't flattened into the query's
  join tree, so `t1.f1 = 1` gates a Result inside it where Postgres makes it t1's filter.
- join 783: a LATERAL nested loop's rows are the outer rows times the inner path's, not the
  join relation's size (agent 14's note); here it decides the join order.
- join 808: the PlaceHolderVar `least(t1.a, t2.a, t3.a)` of a LATERAL join subquery, the same
  shape as partition_join 204 (agent 25's).
- partition_join 98: a merge clause between two expressions (`t1::text = t2::text`), sorted
  on each side: expression members of equivalence classes (agent 21's area).
- partition_join 138 / 234: Postgres builds partitionwise paths for every join relation of the
  search (try_partitionwise_join in populate_joinrel_with_paths), so `t1 ⋈ t3` joins
  partitionwise below a join that can't; the engine tries it for a whole join group only.
- partition_join 210: an EXISTS over a join with a LATERAL item (`int4_tbl, LATERAL (... LIMIT
  2)`) isn't pulled up (`sublink::sub_join` takes joins without parameters), so the DELETE's
  partitions aren't pruned by the pushed-down `c IS NULL`.
- partition_join 300: the child joins' orders tie exactly on cost (5.30, 7.18); Postgres keeps
  the path it made first.
- partition_join 553: a three-way join with a FULL JOIN over merged list bounds isn't joined
  partitionwise (the engine joins its LEFT JOIN alone first).

## Round 16, agent 24 (row marks: several locking clauses, preprocess_rowmarks' order, whole-row mark widths)

No regress unit changes: a 46-file subset (returning, updatable_views, merge, with, rules,
triggers, update, delete, insert, insert_conflict, portals, limit, select, union, subselect,
join, rowsecurity, matview, inherit, partition_join, partition_prune, select_parallel,
aggregates, equivclass, groupingsets, select_distinct, incremental_sort, memoize, join_hash,
explain, stats_ext, foreign_key, privileges, generated_*, window and their prerequisites)
on cce1f9c and on cce1f9c plus this batch: 18653 both, compared unit by unit, none moved. The
table's counts stand (51102). The crate tests pass.

- **A correctness fix: several locking clauses of one level** (`binder/select.rs`
  `Locking::mark_for`). Only the last clause locked anything: `FOR UPDATE OF t1 FOR SHARE OF
  t3` left t1 unlocked. Now each clause applies (transformLockingClause per clause,
  applyLockingClause): a relation takes the strongest strength and the strictest wait policy
  (NOWAIT over SKIP LOCKED) of the clauses naming it or naming none, and a lock pushed down
  from the level above is combined with the subquery's own the same way. Checked with a
  second session's NOWAIT locks against PostgreSQL (`tests/wire.rs`
  `several_locking_clauses`).
- **LockRows marks in preprocess_rowmarks' order** (`paths/rowmarks.rs`, `explain.rs`):
  the locked relations' ctids in the order the clauses name them (parse->rowMarks: the clause
  first naming the relation, then its place in the clause's list; a pulled-up subquery's
  after the level's own), then the other relations' ctids and whole rows by range table
  index: `s.*, t2.ctid` for `FROM (...) s, t2 FOR UPDATE OF s`, `t1.ctid, t3.ctid, t2.ctid`
  for `FOR UPDATE OF t1, t3`. FROM subqueries, functions, VALUES and CTE scans carry an `rti`
  now (`Plan::Barrier` / `FunctionScan` / `Values` / `CteScan`), taken where the FROM item is
  bound, so a pulled-up VALUES list (its RTE appended by pull-up) comes after the level's
  own; a data-modifying statement's whole-row marks are ordered with its ctids the same way.
- **Whole-row marks' widths, in the paths and in EXPLAIN** (set_rel_width): a whole-row
  Var is 24 bytes plus the widths the relation knows of its columns (a subquery's plain
  column outputs, else the columns read), counted in the relation's and every join's width
  above it. The join search sees it (`Planner::copy_marks`, `rowmarks::copied`, a leaf's
  `whole`), and a subquery scan returning it is no trivial one (cost_subqueryscan's
  cpu_tuple_cost), so `UPDATE t1 ... FROM (SELECT x FROM t2 OFFSET 0) s` takes Postgres's
  join order. EXPLAIN knows the marks without VERBOSE too (`copy_mark_leaves`; a semi join's
  inner relations' ctids too): `Subquery Scan on s (... width=32)`. A data-modifying
  statement's subqueries keep every output for their whole row (`whole_marked`).
- **An UPDATE over a join shows its final target list's width** (apply_scanjoin_target_to_paths):
  the new values (a column's width, else its type's), the target's ctid (and tableoid when
  inherited), the other relations' row marks and what RETURNING reads of them: 44 for
  `UPDATE t1 SET b = 'z' FROM t3 ...`, where the engine showed 12.
- **Locked subqueries**: a subquery locking its rows keeps its Subquery Scan (its plan's
  target list has the marks: trivial_subqueryscan; also under an `OFFSET 0`), whose width is
  its reltarget's; a LockRows keeps its input's column statistics and order
  (create_lockrows_path), so a join over a locked subquery's key estimates as Postgres (10
  rows where the engine had 1); a level's LockRows is as wide as its target list with the
  marks (`width=10` for `x` and the ctid).
- **Smaller EXPLAIN gaps** (agent 20's): the statement's select-list constants read only by a
  Sort / Limit / LockRows, with no outer join, appendrel or grouping set below, are plain
  Consts (`'a'::text`, not `('a'::text)`: search_indexed_tlist_for_non_var never references a
  Const; `plain_const_lists`); a projection over a pulled-up one-row subquery of constants is
  folded (`SELECT b || 'y' FROM (SELECT 'x' b) s`: `'xy'::text`, `pullup.rs`). A constant
  GROUP BY key was done by agent 18 (`fixed_group_keys`); this batch's own version was dropped
  in the rebase.
- VERBOSE: a Hash or Materialize over a join under a data-modifying statement's or a
  LockRows's junk layout returns its input's target list (it listed unread columns).

Goldens: `a24_rowmark_order_widths` (recorded from PostgreSQL 18.6).

Left, and why:

- **returning 105**: three parts, none a name. (1) rewriteRuleAction puts the original
  statement's range table first, but the view `joinview` in it is expanded later and its
  relations, pulled up, are appended after the rule action's: the target and the action's
  `joinme` are `foo_1` / `joinme`, the view's `foo_2` / `joinme_1`; the engine names the
  view's first. (2) The join order is a cost tie between the two `joinme` scans, broken by
  range table order. (3) The NEW values are the view's expressions put into the action's SET
  (`(foo_2.f3 + 1)` computed in the top join's target list); the engine computes them in the
  view's projection below the join. And the top join clause's member is an equivalence-class
  choice (below).
- **Equivalence class member order** (found here, not changed: agents 21 / 22's area):
  generate_join_implied_equalities takes the first outer member in ec_members order, the
  order the clauses as written added them (`t2.x = t1.a AND t3.p = t1.a`: `t2.x = t3.p`);
  the clauses the join search gets are already re-derived, `t1.a` first
  (`FROM t1, t2, t3 WHERE t2.x = t1.a AND t3.p = t1.a` shows `t1.a = t3.p`).
- **A join's target list order under row-mark junk**: Postgres's is its joinrel's reltarget
  (build_joinrel_tlist from the first input pair the join search made: rtindex order), the
  engine's its inputs' order (`t1.ctid, t1.a, s.*` for the engine's `s.*, t1.ctid, t1.a`);
  and a scan's columns in attr_needed order (`rm3.q, rm3.p` when the join clause reads q
  first).
- A whole-row Var of a FROM subquery in the select list (`SELECT s FROM (... OFFSET 0) s`)
  still loses the Subquery Scan (HANDOFF's "ROW() of its columns").

## Round 16, agent 23 (self-join elimination order, SubPlan classes with outer values, semi join unique-ification, join costing)

Measured in subset runs of the planner-heavy files (the 33 of agent 22's list), unit by unit
against a baseline run of the same subset on 68498b3 (13102 -> 13112),
then again after rebasing onto b8a205b, f517f1c and 773f83c against their own baselines
(13112 -> 13122, 13121 -> 13131, 13122 -> 13132 of 13180): 10 units fixed, none broken. Crate
tests and clippy on each rebased state.

| file | fixed units |
|---|---|
| join | 499, 543, 612, 632, 654, 655, 771, 774, 819, 888 |

(771 / 774 are agent 19's nested PlaceHolderVar units: they follow from the join target list
order below.)

What changed:

- **remove_self_joins_one_group order** (`analyzejoins.rs` `self_joins`; join 632): the
  candidates by range table index (`ScanPlan::rti`: a pulled-up subquery's relations after the
  query's own), each removed into the first later one it pairs with: `t4` goes into the
  subquery's `t1`, not the other way round. (A removed relation before the kept one made the
  sort width read its columns at the kept leaf's offset: an overflow panic, also before this
  round for `FROM t1, i, t4 WHERE t1.a = t4.a ... ORDER BY`.)
- **A pulled-up subquery's column selection over a scan** (join 654 / 655): Postgres has no node
  for it, so a scan a join reads as it is (create_nestloop_plan's inputs, a merge join's
  unsorted ones, a hash join's outer side: flags 0) returns its physical target list
  (use_physical_tlist), not the selected columns (`ExNode::physical`, set in the projection's
  conversion, applied in `join_built`). So does a scan under a semi join's side hashed unique
  of plain columns (create_unique_plan keeps the subplan's list).
- **SubPlan classes with outer values** (`optimize::subplan_classes`; join 612):
  generate_base_implied_equalities_const in a sub-select: the outer values are Params,
  pseudo-constant members of its classes; the class's first Const, else its last
  pseudo-constant, is compared with every other member, two pseudo-constants giving a gating
  qual (`One-Time Filter: (t2.a = t2.a)` over `Filter: (a = t2.a)`). Done once the level
  reading the sub-select is planned (`plan_level`), so only for sub-selects that stay SubPlans
  (agent 16 found doing it at bind time changed EXISTS pull-ups); for a sub-select level that
  is one sequential scan; an EXISTS that also gets a hashed form keeps its quals (Postgres
  hashes the query as written).
- **Pseudoconstant join quals** (join 499): clause_selectivity_ext counts a pseudoconstant
  clause that isn't a Const 1.0 (`optimize::pseudo_gate`, in the join search's `clause_sel`
  and EXPLAIN's join rows): `x0.f1 = 1` gating `t2 ⋈ t3` sized it 1 row.
- **An equality with a PlaceHolderVar** (costs.rs `binary_op`; join 543): a join-level `var =
  column without statistics` is eqjoinsel's, `(1 - nullfrac) / max(nd, 200)`, not
  DEFAULT_EQ_SEL: `tenk1.unique1 = (42)` keeps 1 of 10000 rows.
- **Foreign keys with constants** (`fk_selectivity`; join 819): an FK column pair in one class
  with a constant (match_eclasses_to_foreign_key_col, nconst_ec) is matched without a join
  clause, and get_foreign_key_join_selectivity divides out the referencing column's `=
  constant` restriction (it already shrank that side): 100 rows for `f1 ⋈ f2`, not 1.
- **The semi join's unique-ified side** (join 888, subselect 326 stays matching):
  - add_paths_to_joinrel's JOIN_UNIQUE_INNER is inner-unique only when the outer side has the
    whole minimal left side (`min_lefthand ⊆ outerrel`); a part of it may meet several
    unique-ified rows.
  - create_unique_path (PostgreSQL 18) sorts its input whole: EXPLAIN's DISTINCT for a semi
    join's side (`unique_sides`) no longer reads the input's order, an incremental sort, a
    Gather Merge or a partial DISTINCT, and hashes when that is enabled where the sort isn't or
    cheaper in total.
  - join_clause_is_movable_to: a semi join's clauses have no outer_relids, so they parameterize
    scans of its left side too (`outerjoins::movable_to`), which the unique-ified side then
    drives (888's Index Only Scan on `t2` by `t3.tenthous`).
  - adjust_rowcount_for_semijoins: such a scan's loop count is at most the unique-ified rows
    of the semi join's right side (`Search::semi_loop_rows`, estimate_num_groups of its
    semi_rhs_exprs over approximate_joinrel_size); subselect 326's Memoized index scan on `a`
    cost 2.43 for Postgres's 3.71 once the movable clauses gave it.
  - The right side may be a join of several relations (`unique_rel`, `join_is_legal_u`'s
    uniqueable; estimate_num_groups per relation, `Search::join_groups`).
  - EXPLAIN's estimate_num_groups scales a relation's distinct values to that relation's rows
    (rel->rows), not the rows of a join above it.
- **A join relation's target list order** (`JRel::tl`, `JoinMeta::tlist`; join 888): Postgres
  builds it from the first pair make_join_rel makes the relation from (build_joinrel_tlist over
  the first input, then the second), whichever input a path puts outside; a join now returns
  its columns in that order (`Output: t1.unique1, t3.tenthous` over `HashAggregate(t3) ⋈
  Hash(t1)`). Not for the level's topmost join, which returns the scan/join target. A
  PlaceHolderVar a scan below computes stays with its relation's columns in agent 19's
  `tlist_layout` regrouping (add_placeholders_to_base_rels). A Hash over a join returns the
  join's target list (it listed every column of the join's row).

Goldens: `tests/sql/a23_sje_subplan.sql`, `a23_join_costs.sql`.

Looked at and left (why):

- join 620: after self-join elimination the kept relation's reltarget is both relations'
  (`t3.c`, only the removed join clause read, stays: `Output: t3.c, t3.b`). The columns could
  be kept, but the order is the reltarget's (the final target list's Vars, then each clause's
  as distribute_qual_to_rels meets them), which EXPLAIN reconstructs as needed columns then
  clause columns in column order; the same gap shows without self-join elimination (`Output:
  t2.b, t2.c` under a Materialize for Postgres's `t2.c, t2.b`). Needs reltarget order
  modelled.
- join 560 (agent 22's join-search order): the joinrel `{t1, t3, t4, t5, t6}` is sized from
  its first pair (Postgres: `{t1, unique(t4 t5 t6)}` then `t3`, 1 row; here `t1` with
  `{unique, t3}`, 124 rows), which decides the plan; the remaining text is the columns of
  classes with constants a join still carries up (`t5.b`, `t4.a`).
- join 715: the unique-ified first semi join matches now (`HashAggregate` over `t3 ⋈ t4`, an
  Index Only Scan on `t2_1` by it), but Postgres's plan runs the second semi join as a Nested
  Loop Semi Join whose inner side is the join `t3_1 ⋈ t4_1` parameterized by the first's `t3`
  (the class `t1.id = t2.id = t3.id = t3_1.id`): a parameterized join path as a semi join's
  inner side, not built here (870.77 for Postgres, 917.40 here).
- Seen while probing, results matching: an EXISTS pulled up and self-join-eliminated shows its
  class's restriction as `b = a` for Postgres's `a = b` (the class's member order); an IN
  whose sub-select's quals read the outer query shows `sj.a = t1.b` for `t1.b = sj.a`; a
  Memoize over a Function Scan whose output nothing reads shows `Output: gs.i` (Postgres
  none); `IN (...)` with hash and merge joins off is shown over a Subquery Scan on
  `"ANY_subquery"`; a pseudoconstant join qual costs per row (Postgres: once, at startup);
  an EXISTS sub-select's `SELECT *` columns count in its scans' widths (simplify_EXISTS_query
  drops the target list); `a = 1 AND b = 2 AND a = b` isn't folded to false (a class with two
  Consts); a sub-select over a join keeps both copies of a class's outer-value equalities.

## Round 16, agent 18 (SubPlan numbering in subquery_planner's order, GROUPING() copies, resjunk group keys)

Subset runs of 33 planner-heavy files (test_setup, create_index, create_misc, create_view,
inherit, select, union, join, aggregates, groupingsets, subselect, select_distinct, window,
with, partition_aggregate, partition_join, partition_prune, join_hash, updatable_views,
returning, merge, limit, tsrf, equivclass, incremental_sort, select_parallel, rowsecurity,
rules, memoize, rangefuncs, insert_conflict, stats_ext, sqljson_jsontable, select_views), unit
by unit against a run of the branch head the commit is on (b8a205b): 13164 → 13173, +9, none
lost; on the earlier heads 5cfa72a (28 of those files) +10, 68498b3, 3fb95cb, b963500 and
f334753 +9
(b963500's base run also lost select_parallel 112 / 116 to the reference's parallel workers
starting late under load), none lost; crate tests and clippy on each.
groupingsets 55, 57, 189, 191, 195, 197, 215 and join_hash 269, 271 match: both files
match fully. (aggregates 116 matched with this change too; agent 16's second batch fixed it
first: the two resjunk-key mechanisms are merged, `group_tlists` deciding which nodes carry
the keys.)

- **SubPlan numbering** (`explain.rs` `renumbering`, `next_plan_id`). Postgres numbers
  sub-selects as subquery_planner makes them (make_subplan / build_subplan: a sub-select's
  own sub-selects first): the target list, the join tree's quals in their order, HAVING, ...,
  the RTEs' expressions, and in Postgres 18 the GROUP BY expressions last (RTE_GROUP's
  groupexprs, preprocessed after the target list; flatten_group_exprs puts them back in it
  after that). The engine numbers them as EXPLAIN meets them, child first. EXPLAIN now builds
  the plan once recording each number taken (the sub-select's query level, whether it is a
  group key's, whether it was bound in a WHERE / ON clause: `Subquery::qual`), and when
  Postgres's order differs builds it again with the numbers in that order: a level's group
  keys' sub-selects (each with the ones inside it) after its other own ones, and its quals'
  in the order they were bound, which is the join tree's (ON clauses as the FROM list is
  bound, then WHERE; a pulled-up IN / EXISTS subquery's quals where it was written). A level's
  InitPlans are then listed in their numbers' order (SS_attach_initplans). Nothing else
  moves: the target list's sub-selects against the quals' (Postgres: target list first; the
  engine's met order has the scans' first) and FROM subqueries planned apart (Postgres: after
  the level's expressions) keep the engine's order, as no unit of the measured subset differs
  by them.
- **GROUPING() arguments' copies.** A GROUPING() argument that is a sub-select group key is a
  copy of its own in Postgres (the parser leaves GROUPING()'s arguments alone;
  SS_process_sublinks plans each copy; build_subplan for a sub-select's GROUPING() parameter),
  numbered with the target list, so before the key's: `GROUPING((SubPlan 1))` over the key
  `(SubPlan 3)` (195 / 197), `InitPlan 1` for an uncorrelated one, listed with the level's
  InitPlans though nothing runs it (189 / 191). One copy per GROUPING() call (keyed by the
  call), shown as the copy's reference; a correlated copy's SubPlan isn't shown (the executor
  never initializes it). The grouping's input returns what the copies read
  (make_group_input_target's pull_var_clause recurses into GROUPING()'s arguments: `t1.id`).
- **GROUPING() without grouping sets** stays a GROUPING() call over the keys (always 0), no
  longer the constant 0: shown as `GROUPING(q1)`, kept in HAVING (contain_agg_clause), a
  correlated parameter of a sub-select (`(SELECT grouping(q1))` a SubPlan, 191), and it makes
  the grouping an Agg (GroupAggregate, not Group).
- **Pseudoconstant group keys** (a constant, an InitPlan's output, a pulled-up one-row
  VALUES' such value) are redundant group pathkeys (make_pathkeys_for_sortclauses_extended),
  out of processed_groupClause: no Group Key, and all of them redundant a GroupAggregate of
  one group (191, `GROUP BY 1 + 1`).
- **Resjunk GROUP BY keys** (`group_tlists`). A key the select list doesn't name is a resjunk
  target-list entry (findTargetlistEntrySQL99), after the select list and the ORDER BY's
  entries: every node returning the level's target list returns it, not only the statement's
  top aggregation: a Sort, Limit, Unique, ProjectSet or WindowAgg above the grouping, a
  sub-select's or FROM subquery's grouping (aggregates 116, groupingsets 215, 195 / 197's
  `t1.v`). The grouping's input computes the keys in the target list's order
  (make_group_input_target: `(q2 + 1), q1`). A ProjectSet computing the target list over a
  grouping reads what its expressions use, in that order (split_pathtarget_at_srfs), not the
  grouping sets' mask column.
- **A FROM-less subquery whose output is a sub-select is pulled up** (is_simple_subquery takes
  it when nothing in it is volatile; `lift.rs` `movable_fromless`) when no outer join nulls it:
  the sub-select computed where the query reads it (55 / 57: the key computed by the scan
  under the grouping, its SubPlan attached to that scan, the Sort above referring to it).
- **A scan reading a system column** (`tableoid`) has no physical target list
  (use_physical_tlist refuses): it returns its reltarget, the columns used above, then the
  hash keys' and the join quals' (join_hash 269 / 271's outer side).

Golden: `tests/sql/a18_grouping_subplans.sql`.

Left (none fails a unit of the subset):
- A pulled-up subquery's sub-select output read in several places is one value computed once
  (the lifted group's projection), where Postgres copies it into each place, each a SubPlan of
  its own (`SELECT ss.x, ss.x ...`: Postgres `(SubPlan 1), (SubPlan 2)`).
- Under grouping sets a pulled-up subquery's output is a PlaceHolderVar in Postgres, so a
  GROUPING() argument matches the key's target-list entry: VERBOSE shows
  `GROUPING(((InitPlan 3).col1))`, the engine the copy, `GROUPING((InitPlan 1).col1)` (189's
  VERBOSE form; the suite's is COSTS OFF only).
- `GROUP BY` a correlated sub-select alone (`GROUP BY s` without grouping sets): Postgres sorts
  (a Group over a Sort), the engine hashes (a cost difference of the sub-select key).
- The general order (target list before the quals, FROM subqueries after the level's
  expressions, MIN / MAX InitPlans at createplan time) isn't applied: it needs every
  sub-select's clause recorded as `qual` is.

## Round 16, agent 22 (a FROM subquery's several paths, class clauses' first making)

Measured in subset runs of the planner-heavy files (test_setup, create_misc, create_index,
create_view, select, union, subselect, join, aggregates, inherit, partition_join,
partition_prune, partition_aggregate, equivclass, memoize, with, rowsecurity, updatable_views,
select_distinct, groupingsets, incremental_sort, merge, returning, rules, insert_conflict,
join_hash, select_parallel, rangefuncs, tsrf, limit, window, stats_ext, sqljson_jsontable),
unit by unit against a baseline run of the same subset on 9b0ac33 (13099 -> 13102 of 13180),
then again after rebasing onto b963500 against its own baseline (13105 -> 13108): 3 units
fixed, none broken, no other agent's units moved.

| file | fixed units |
|---|---|
| join | 123, 410, 502, 503 |

What changed:

- **A FROM subquery's several paths** (set_subquery_pathlist; join 502 / 503). Postgres makes
  a Subquery Scan path of each of the subquery's final paths, and its grouping keeps a sorted
  path beside the cheapest hashed one (add_paths_to_grouping_rel over every input path, a
  presorted index scan's too; create_final_distinct_paths): a merge join may read the Group
  over the index scan, or the Unique over the sorted scan, while the hashed one is cheaper on
  its own. The join search now gives a subquery leaf (`Plan::Barrier`, not LATERAL) those
  paths when one of its columns is useful for a merge join or the query's order: its plan as
  planned and one per index path of its table that leads with a grouping key
  (`Planner::grouping_variants`), each costed with the paths EXPLAIN keeps beside the
  cheapest (`explain::subquery_paths`). The path the search takes is recorded in the plan,
  not chosen again: `Plan::Barrier`'s new `path` (`plan::SubPath`: its keys, cheapest or
  cheapest to start); EXPLAIN shows that one of the subquery's paths (`take_sub_path`), and
  the executor returns the subquery's rows in its order.
- examine_simple_variable through a subquery (`explain::subquery_var_stats`): a subquery with
  set operations or grouping sets passes nothing about its outputs to the query above, one
  with DISTINCT or GROUP BY only that its sole such key is unique (as many values as rows).
  The engine passed the grouped column's statistics up, unique index included, so the outer
  join over `group by b.id, b.c_id` was sized as unique on `s.id` (2260 rows for Postgres's
  25538) and hashed.
- **ec_derives** (join 123): a class's join clause keeps the operand order of its first making
  (create_join_clause finds an earlier one either way round). Besides the index paths' clauses
  before the search (round 14), the search notes each class clause as it makes it
  (`Search::derives`): a new join relation's (build_joinrel_restrictlist, as shown already)
  and a parameterized join path's for a class whose clause the inner path enforces
  (get_joinrel_parampathinfo's dropped classes: the parameters' member first). That order is
  put on the clause after the search: `sum(i4b.f1) = (sum(i4a.f1) + 1)`, first made for `t`'s
  index scan parameterized by both subqueries, joined to `a`.

Goldens: `tests/sql/a22_subquery_paths.sql`, `a22_ec_derives.sql`.

Second batch (join 410; subset run on f334753 against the first batch's: 13108 -> 13109, none
broken, and again rebased onto b8a205b and f517f1c against their baselines: 13112 -> 13113,
13121 -> 13122; a full run of the whole schedule in one database on it measured 51074: every file as
the table has it, join at 881, and stats 387 / 388 / 392 / 415 / 418 lost to the shared
reference, whose `regress_tblspace` a concurrent run had dropped, so its ALTER ... SET
TABLESPACE errored):

- **ParamPathInfo rows** (get_joinrel_parampathinfo, get_parameterized_joinrel_size): every
  path of a join relation with one parameterization has the rows estimated from the first
  pair of input paths that made one (at most the relation's own rows); the engine estimated
  each path from its own inputs (`Search::ppi`). Agent 17's try of this made other plans
  worse because the pairs came in another order than Postgres's: the cause was index
  parameterizations Postgres never makes. consider_index_join_clauses works per index, on
  the outer relations of that index's own clauses (and their unions); the engine took the
  unions over every index's clauses, so `b` (indexes on `unique2` and on
  `(thousand, tenthous)`) had paths parameterized by `{a, i2}` and `{a, i1, i2}` (an index
  scan on one index checking the other's clause as a filter), and those made the first
  pairs. Now each index has its own outer sets (`param_paths`), its paths only those; the
  bitmap ANDs per parameterization as before. With that the first pairs are Postgres's:
  join 410's `b` parameterized by `i2` joined to `i1` (1 row), and its plan matches,
  costs included.

Golden: `a22_param_rows.sql`.

Looked at and left (why):

- A trivial Subquery Scan's cost: Postgres charges cpu_tuple_cost per row when the outer query
  reads fewer columns than the subquery returns (cost_subqueryscan, trivial_pathtarget), even
  where setrefs then removes the scan; the engine charges nothing for one EXPLAIN doesn't show
  (costs only: join 502's Merge Right Join total 634.93 for 657.53).
- A Subquery Scan under a Hash (`Hash -> Subquery Scan on s -> HashAggregate`) Postgres keeps
  and the engine drops; seen while testing, not this round's.
- join 442: three separate gaps, none of them the join search's. reconsider_outer_join_clauses
  (the left join's `tt3.f1 = tt4.f1` with `tt3.f1 = 'foo'` below it gives `tt4.f1 = 'foo'` on
  the nullable side and the join clause goes; Postgres then has to keep `tt3 ⟕ tt4` joined
  first, which the engine's join search doesn't know of), a constant class member pushed as a
  LATERAL LIMIT subquery's Subquery Scan filter (`ss1.c0 = 'foo'`), and a Memoize over that
  Subquery Scan keyed by its LATERAL reference.
- partition_prune 265: fix_alternative_subplan per copy of the AlternativeSubPlan, by each
  copy's num_exec (an index qual 1, so the plain SubPlan; the recheck qual and the target
  list rows, so the hashed one), plus where each SubPlan shows: an Append's run-time pruning
  expressions hold their own copy (SubPlan 3 under the Append), and EXPLAIN prints a SubPlan
  only where it is first met. The engine has one sub-select per expression and decides
  hashing once; this needs SubPlan copies with their own ids (agent 18's SubPlan numbering
  touches the same code).

## Round 16, agent 20, second batch (agent 15's row-mark leftovers: locks over unique-ified semi joins, RETURNING sub-selects, SET costs)

No regress unit changes (none of these failed one); checked in the full runs below the first
batch's section and with a golden. Agent 15's leftovers, one by one:

- **Wrong locks: a semi join unique-ified under FOR UPDATE** (`rowmarks::key`): the level's
  rows were locked by its scan of the outer relation below the join, so every row that scan read
  was locked, the ones the join discards too (`SELECT * FROM t1 WHERE a IN (SELECT x FROM t2)
  FOR UPDATE` locked all of t1). A Distinct of the level (only a unique-ified semi join's inner
  side can be one: a DISTINCT of the level itself takes no lock) is now keyed as a semi join's
  inner side is: its relations' reference marks are nothing to lock, so the LockRows goes on
  top of the join and locks only the joined rows (checked with a second session's `FOR UPDATE
  NOWAIT`, as Postgres). EXPLAIN VERBOSE shows Postgres's HashAggregate carrying `t2.ctid`
  through, ungrouped (`lock_level_marks`). No pass-through column in `Plan::Distinct` was needed:
  the executor's recheck (EvalPlanQual) treats the inner side as it does a semi join's.
- A LIMIT subquery with a pushed-down lock: since agent 15's LockRows work its rows are locked
  under the Limit, as Postgres (a second session can lock the rows the Limit discards); its
  Subquery Scan's Limit now returns the marks' ctids too when no Sort is between
  (`lock_junk`).
- **RETURNING's sub-selects** show without VERBOSE (`SubPlan 1` under the Insert / Update /
  Delete; they only showed with it).
- **An UPDATE's SET costs**: the node computing the new values (the scan, or the join) pays
  for their sub-selects per row (cost_subplan in the target list's cost, as a SELECT list's:
  124.17 where the engine showed 1.06) and for their operators by cost_qual_eval (an I/O cast
  is two calls: `SET v = k::text` 1.09).
- MERGE per-row SubPlan costs: Postgres charges nothing for the actions' target lists (the
  ModifyTable's cost is its source's, create_modifytable_path), and the engine matches it (a
  MERGE whose UPDATE SET or WHEN condition has a correlated sub-select: the same 22.95 in
  both). The difference agent 9 saw was the plain UPDATE's, above.

Goldens: `tests/sql/a20_lock_returning_costs.sql`.

Left, and why:

- returning 105: the rule action's range table (rewriteRuleAction puts the rule's relations
  after the statement's, so the view's `foo` is `foo_2` and the target `foo_1` in Postgres),
  then a different join order and the NEW values computed in the ModifyTable's target list.
  That is the rule rewriter's range table and the action's own join planning, not names.
- A whole-row-marked subquery's width in the planner and EXPLAIN (set_rel_width: a whole-row
  Var is 24 bytes plus its columns' widths, which can swap a hash join's sides), and the width
  of an UPDATE / DELETE join's target list (Postgres: the new values, then 6 per ctid and a
  whole row's width per whole-row mark, 44 where the engine shows 10): the paths
  (`joinpath::width_of`) don't know the row marks, and EXPLAIN knows the whole-row marks'
  junk only under VERBOSE. Also agent 15's row estimate over a locked subquery's key.
- A LockRows level's marks put the whole rows after every ctid; Postgres orders the relations
  not locked at the level by range table index (`s.*, t2.ctid` for `FROM (...) s, t2 FOR
  UPDATE OF s`). The engine's non-table leaves (subqueries, CTEs, functions, VALUES) carry no
  range table index to order by.

## Round 16, agent 20 (untyped literals in a select list are text; DML subqueries' whole-row marks)

Two full runs of the schedule in one database (`PGREGRESS_TIMEOUT=300`), f8c1317's engine and
f8c1317 plus this batch: 51042 -> 51045, the units below fixed, no unit broken (compared unit by
unit). Both runs share the full-run artifacts the table doesn't count (oidjoins, domain 482 and
alter_table time out; select_views' 8 units over the catalog earlier files leave). Re-checked
after each rebase with this round's second batch: full runs on 77bc60e, 5cfa72a and 68498b3,
unit by unit against the run before (only the rebased commits' units changed; the last one
measured 51075 once tablespace and stats are counted from a subset run: a concurrent run
dropped `regress_tblspace` on the reference mid-run, 165 units, and select_parallel 112 once
differed on the reference's side, a Gather worker starting late: `loops=49`), then on 3fb95cb
the crate tests and a 55-file subset run.

| file | fixed units |
|---|---|
| with | 235, 238, 241 (with matches fully, and equals `expected/with.out`) |

What changed:

- **Untyped literals in a SELECT's output are text in its target list**
  (resolveTargetListUnknowns), not cast in a projection above it: a constant is a text constant
  (`'lit'::text`, `NULL::text` in EXPLAIN VERBOSE and view definitions), an untyped parameter
  becomes a text one (`PREPARE p AS SELECT $1`: `{text}`). The result types (RowDescription,
  `pg_typeof`, CREATE TABLE AS, views, CTE and FROM-subquery columns) were already text; what
  moves is the plan text, and through it pulled-up subqueries' and CTEs' constants. As in
  PostgreSQL, a set operation's leaves and an INSERT's SELECT keep them unknown
  (p_resolve_unknowns off) for the set operation (select_common_type) or the target columns to
  resolve, but a GROUP BY / ORDER BY / DISTINCT key that is one is text in every case, and so is
  the select-list entry it is (addTargetToGroupList, addTargetToSortList): `INSERT INTO t(int)
  SELECT '5' ORDER BY 1` errors "column ... is of type integer but expression is of type text"
  as in PostgreSQL (it used to insert 5). (`binder/select.rs`, `bind_simple_select`.)
- INSERT ... SELECT copies an unknown-typed constant or parameter of the select list up as it is
  (transformInsertStmt's hack), so coercing it to the column's type makes a constant of that type
  (`Output: 1, '2020-01-01'::date`, where the engine showed `'1', '2020-01-01'`); an INSERT over a
  set operation reports a coercion error at the leftmost leaf's column (`dml.rs`).
- **DML row marks of a FROM subquery planned apart**: an UPDATE / DELETE / MERGE joined with one
  gives it a whole-row mark (ROW_MARK_COPY), its Subquery Scan returning `o.*` up the plan to the
  ModifyTable (`explain::dml_copy_marks`, now for subqueries too, not one holding the target, a
  view written through); its own relations have no mark (`rowmark_scans` stops at it: a DELETE
  ... USING (SELECT ... FROM m OFFSET 0) showed `m_1.ctid`, read inside the subquery, for `o.*`).
- MERGE's processed target list orders the source's columns as preprocess_targetlist adds them:
  each action's condition's Vars, then its target list's (in column order), in action order
  (`m.ctid, o.v, o.k, o.*` for `UPDATE SET v = o.v` before `INSERT VALUES (o.k, o.v)`).

Goldens added: `tests/sql/a20_unknown_literals.sql`, `a20_dml_subquery_marks.sql`.

Found, not fixed (none fails a unit today):

- A constant select-list entry under a Sort shows wrapped (`('a'::text)`, Postgres: `'a'::text`;
  set_dummy_tlist_references keeps Consts). The engine can't tell such a constant from a
  pulled-up subquery's output standing for a PlaceHolderVar (explain.rs's comment on
  `UNREAD_NULLS`), so it stays wrapped.
- `GROUP BY` a constant: Postgres shows a GroupAggregate with no group key, the engine a
  HashAggregate keyed by the constant.
- A pulled-up subquery's constant isn't folded into the expressions over it
  (`SELECT b || 'y' FROM (SELECT 'x' b) s`: Postgres `'xy'::text`), as HANDOFF lists.
- A DML statement's subquery whose output only its whole-row mark reads loses that output to
  unused-output pruning (`count(*)` shown `NULL::bigint` under `o.*`): agent 15's "whole-row
  mark's subquery keeping all its columns".


## Round 16, agent 17 (join relations parameterized by LATERAL references, partitionwise topmost relations)

Measured in subset runs of the planner-heavy files (test_setup, create_misc, create_index,
create_view, select, union, subselect, join, aggregates, inherit, partition_join,
partition_prune, partition_aggregate, equivclass, memoize, with, rowsecurity, updatable_views,
select_distinct, groupingsets, incremental_sort, merge, returning, rules, insert_conflict,
join_hash, select_parallel, rangefuncs, tsrf, limit, window, stats_ext, sqljson_jsontable),
unit by unit against a baseline run of the same subset on 9509532, then again after rebasing
onto f8c1317, 77bc60e, 58f4c06 and 5cfa72a (+5, none broken; crate tests and clippy on each;
select_parallel 112 differed once from the reference's side, a worker of its Gather starting
late under load: `loops=42`, the engine's output unchanged);
plus the other files with LATERAL references (create_table, collate.icu.utf8,
create_table_like, gin, gist, indexing, jsonpath, numeric, tablesample, plpgsql,
stats_import, tidrangescan, arrays, json, jsonb, sqljson, xml): every unit as before, all
matching.

| file | fixed units |
|---|---|
| sqljson_jsontable | 101 (**row order**) |
| join | 414, 744, 775 |
| partition_join | 21 |

What changed (`sql/paths/joinpath.rs`, `joinpath/outerjoins.rs`, `binder/optimize.rs`):

- **Join relations with LATERAL references left** (joinrels.c, relnode.c): a LATERAL item may
  join an item it doesn't read before the items it reads; the join relation's
  `lateral_relids` (min_join_parameterization over create_lateral_join_info's transitive
  closure) are the items it still reads, and it has only paths parameterized by them (a
  LATERAL item's own path is parameterized by its whole closure). join_is_legal's LATERAL
  checks replace the old rule (the item alone on a nested loop's inner side): no references
  both ways, one way only as a nested loop with the referencing side inside (not against an
  outer join's order, a unique-ified semi join or a FULL JOIN), only through a direct
  reference, and never with a remaining reference inside an outer join's right side with
  the join (join_plus_rhs). has_join_restriction is lateral_relids / lateral_referencers
  (base relations only); param_source_rels takes the join's lateral_relids. So
  `x, y, JSON_TABLE(... PASSING x, y)` joins `x ⋈ jt` parameterized by `y` and never `x ⋈ y`
  clauselessly (sqljson_jsontable 101's row order).
- **Parameterized hash and merge joins** (try_hashjoin_path, try_mergejoin_path): inputs
  parameterized by relations outside the join give a join path parameterized by all of them
  (calc_non_nestloop_required_outer) when param_source_rels allow it; hash_inner_and_outer
  tries each of the inputs' cheapest_parameterized_paths, and a relation with only
  parameterized paths has its least parameterized one as cheapest_total_path (set_cheapest).
  Such a path checks the join clauses movable into it (get_joinrel_parampathinfo, shared with
  the nested loop's as `param_clauses`); at an outer join one needing relations outside it is
  the join's Filter (RINFO_IS_PUSHED_DOWN). join 414: `Hash Right Join ... Filter:` under the
  nested loop.
- Building such plans: a parameterized scan, Append or LATERAL item that isn't the inner side
  of the nested loop passing its values (a hash or merge join's input, a nested loop's outer
  side) reads them from the nested loop above the join, through a one-row join passing them
  in (shown as nothing); a hash or merge join passes no parameters itself (the executor ran
  one with parameters as a nested loop, dropping its hash keys: wrong rows the comparison
  caught while developing join 744).
- An outer join's minimal left side no longer takes the LATERAL references of its right side
  (make_outerjoininfo has no such rule; join_is_legal's LATERAL checks keep the order), and a
  LATERAL right side takes into its scan only the join clauses over the relations it reads
  (its parameterized path's clauses, get_baserel_parampathinfo; it took every clause over
  both sides): join 744 (the hash join under `a`'s nested loop), 775 (the join filter's
  order).
- Parameterized join paths are kept by add_path among those of their parameterization (fuzzy
  costs and pathkeys; their rows are Postgres's ParamPathInfo's, so not compared), not only
  the cheapest.
- unnest of a non-constant array: 10 rows (estimate_array_length's default; it said 100).
- EXPLAIN costs a single-batch hash join under a nested loop as rescanned without rebuilding
  its table (cost_rescan), as the planner did.
- **The topmost scan/join relation** of a query level, when it is joined partitionwise, keeps
  only its partitionwise paths (apply_scanjoin_target_to_paths drops the others), where they
  competed with the plain join by cost: the Append (sorted) or a Merge Append of the child
  joins.
- A merge join input sorted on a class with two of its columns sorts by the one the target
  list needs (prepare_sort_from_pathkeys: the first member in the input's target list, which
  lists the columns needed above before those only the join clauses read): partition_join 21
  (`Sort Key: t2_1.b`).

Goldens: `tests/sql/a17_lateral_join_rels.sql`, `a17_sort_key_member.sql`.

Looked at and left (why):

- partition_prune 410 (the cross-partition UPDATE's moved row counted by a later Bitmap Index
  Scan, `Heap Blocks: exact=1`): not the pipelining, which UpdateNode already does (each
  source row is updated before the next is read). Postgres's index has an entry for every
  tuple version, the heap fetch deciding visibility, so the moved row's new entry is counted
  (and its heap page read) although the tuple is invisible to the command; the engine's index
  entries are MVCC-versioned like its rows and read at the command's snapshot. Counting them
  faithfully needs index entries kept apart from tuple visibility (dead ones counted until
  vacuumed too), a storage-model change, not done.
- partition_join 74: the expression `(t3.a + t3.b) / 2` is no equivalence class member here
  (classes are of columns), so no clause `t2.b = (t3.a + t3.b) / 2` is derived and the index
  scan is parameterized by `t1` only (5 loops, not 84: 2.56 a scan, not 0.30). Needs
  expression members in the join search's classes.
- partition_join 86 / 88 / 92: the IN subquery is planned (partitionwise) at bind time, before
  the WHERE's sublinks are pulled up, and `sublink::sub_join` takes neither an inheritance
  Append nor a partitionwise Append; even pulled up, Postgres's plan needs a semi join whose
  unique-ified right side is a join of two relations (create_unique_path over a join rel;
  `unique_rel` takes one item).
- join 502 / 503: the subquery's sorted path (Group over an index scan) kept beside its
  cheapest (HashAggregate) for a merge join: a subquery here is one plan, and EXPLAIN picks
  its grouping strategy itself.
- join 123: Postgres's `sum(i4b) = (sum(i4a) + 1)` is a derived class clause first made by
  get_joinrel_parampathinfo's dropped-class step for `t`'s index scan parameterized by both
  (b first), then reused by the join of the two; the engine orients class clauses by fixed
  rules.
- join 410: the parameterized join's rows (Postgres's ParamPathInfo rows come from the first
  input pair, `b` parameterized by `i2` joined to `i1`: 1 row; here 5); keeping the first
  pair's estimate (tried) changed other plans for the worse, the engine's input order not
  being Postgres's.
- join 442: the outer join clause dropped once `tt4.f1 = 'foo'` is derived (only in the
  order joining tt3 and tt4 first), the subquery's derived filter and a Memoize over it.
- partition_prune 265: an AlternativeSubPlan copied into each partition's index condition,
  chosen per copy by its executions (the plain SubPlan there); not attempted.
- unnest over an analyzed array column: Postgres takes its element-count histogram's average
  (STATISTIC_KIND_DECHIST), the engine the default 10.

## Round 16, agent 15 (full-run timeouts: catalog rescans; view whole rows; locking levels' marks)

A full run of the whole schedule in one database (`PGREGRESS_TIMEOUT=300`) on this round's
commit over f8c1317 measured **51053**, every file's count as the table below has it: the
first full run to reach the table's total. A full run of ede51fc before it measured 51035:
domain, oidjoins and alter_table matched there too after the first fix below, and select_views
lost 8 units to 6245681 (fixed here, below). No timeout, no engine restart: domain ran in 33 s,
oidjoins in 14 s, alter_table in 216 s.

- **domain 482 / oidjoins timeouts** (`exec/nodes.rs` `sys_rescan`): no hang and nothing that
  built up but the catalog itself. Sampled with gdb in a grown catalog (400 tables, 1600 types,
  14000 attributes): the stalled statements were nested loops whose inner side is a catalog
  scan parameterized by the outer row (`Index Scan using pg_type_oid_index ... Index Cond: (oid
  = a.atttypid)`, chosen since round 14's catalog index paths in joins), and correlated
  sub-selects over catalogs. The engine generates a catalog's rows whole, so every outer row
  generated pg_type (and pg_class, pg_attribute) again: O(outer rows x catalog), 48 ms per
  pg_type in a full run's catalog times 14000 attributes for column_domain_usage, minutes for
  oidjoins' 200 checks. Postgres reads such scans through the catalog's index (or the
  syscache), once per outer row and never the whole catalog. Now a catalog scan that reads an
  outer value generates the catalog once per statement (for its catalog and role, kept in the
  statement's `ExecCtx`: the rows as of the statement's start, as its snapshot gives Postgres),
  and a `column = outer value` equality (a built-in `=` on an integer, oid, "char" column, or
  a name / text one under a deterministic collation, the value of the column's type) finds its
  rows through a hash on that column; the whole filter is still checked on each. System views
  are generated again on each rescan, as Postgres runs their functions again. In the grown
  catalog: column_domain_usage > 500 s -> 1.8 s, oidjoins 127 s -> 6.6 s. Test:
  `tests/ddl_scaling.rs` `catalog_rescans_dont_grow_with_the_catalog` (the catalogs a query
  generates, `system::generated_rows_count`, with 3 and with 40 other tables; it fails with
  thousands before the fix).
- **alter_table 953** (`explain.rs` `name`): 1a2f31a shows a relation's whole row as `rel.*`;
  a view's whole row (`to_json(v1)` over a view of `bt`) is no relation's: the view's subquery
  is pulled up and its whole-row Var becomes a ROW() of the subquery's outputs typed as the
  view's row (pullup_replace_vars), `to_json(ROW(bt.id, bt.stuff))`. A ROW() of a view's or a
  composite type's row type is now shown as a ROW(); a table's, a materialized view's and a
  catalog's stay `rel.*` (1a2f31a's goldens unchanged).
- **select_views 25-39** (8 units, from 6245681, `explain.rs` `name_text_relabel`): a text column
  compared with a `name` pseudoconstant (`name = current_user`, texteqname) was shown relabelled
  as a join clause out of its equivalence class is (`(name)::text = CURRENT_USER`). A class of
  the column and a pseudoconstant with that one source clause uses the clause as written
  (generate_base_implied_equalities_const); only a clause whose `name` side reads another
  relation's columns comes out relabelled (6245681's subselect units unchanged).
- **Row marks** (`paths/rowmarks.rs`, `explain.rs`): a level with a locking clause of its own
  whose locks all went into FROM subqueries planned apart (`FOR UPDATE OF s`, `s` not pulled
  up) still gets its LockRows, locking nothing, marking `s.*` (grouping_planner's
  `if (parse->rowMarks)`, ROW_MARK_COPY), over a Subquery Scan (`rowmarks::OWN_LOCK`, set per
  level by the binder). A FROM subquery marked by its whole row keeps every output
  (remove_unused_subquery_outputs keeps them all for a whole-row Var: `s.other` was missing
  under a FOR SHARE subquery). The Limits over a level's LockRows return its marks' ctids
  (their input's target list).

Goldens: `a15_whole_row_views` (views' and composite types' rows, catalog rescans in joins and
sub-selects, text = name restrictions), `a15_lock_marks`.

Left, and why:
- returning 105: besides agent 9's notes, the target and the view's `foo` take each other's
  names (`foo_2` / `foo_1`) and the join order differs; rewriteRuleAction's range table order
  and the rule action's join planning, a larger change than a name.
- A unique-ified semi join under FOR UPDATE (`rowmarks::key` has no Distinct case): Postgres's
  HashAggregate carries the inner relation's ctid through, ungrouped; the engine's hashed
  Distinct compares every column it returns, so it needs a pass-through column (Plan::Distinct,
  its executor, EXPLAIN and costs), not a key case. Until then the outer scan locks its rows.
- A whole-row-marked subquery's width in the planner: Postgres costs `s.*` (24 bytes plus its
  columns' widths), which can swap a hash join's sides; the engine's paths don't see the mark.
  Also the join's row estimate over a locked subquery's key (500 for Postgres's 1000).
- A LIMIT subquery with a pushed-down lock is locked by its scan (no LockRows under the
  Limit), so the Limit's output lacks the ctid.
- MERGE per-row SubPlan costs: not attempted (no regress unit, and the source of Postgres's
  higher ModifyTable total isn't established).
- name = text joins aren't hashed (a Nested Loop for Postgres's Hash Join), seen while testing.

## Round 16, agent 16 (FULL JOIN without merge / hash plans, made-type sort support, semi join quals, SJE with LATERAL items)

Measured in subset runs of 37 files (test_setup, create_misc, create_table, create_index,
create_view, create_function_sql, select, inherit, select_distinct, select_distinct_on,
subselect, union, join, aggregates, groupingsets, join_hash, tsrf, window, equivclass, limit,
rangefuncs, with, partition_join, tuplesort, memoize, incremental_sort, select_parallel,
partition_prune, partition_aggregate, merge, explain, updatable_views, rules, returning,
stats_ext, insert_conflict, sqljson_jsontable), the base binary (f8c1317, again on 77bc60e) and this round's
side by side, unit by unit: 8 units fixed, none broken.

| file | fixed units |
|---|---|
| join | 576, 606, 617, 618, 619, 876, 878, 880 |

What changed:

- **Wrong results: a FULL JOIN no merge or hash join can run returned rows**
  (populate_joinrel_with_paths: Postgres errors "FULL JOIN is only supported with
  merge-joinable or hash-joinable join conditions", 0A000). The engine ran such a join (`ON
  a.x < b.y`, `ON a.x = 1`, `ON ... OR ...`, `IS NOT DISTINCT FROM`, a made `=` without HASHES
  beside another clause) as a nested loop. `optimize::full_join_supported` takes
  select_mergejoin_clauses' and hash_inner_and_outer's rules: either input as the outer one,
  every clause but constants a merge-joinable `=` across the sides (an operator without a
  commutator only with the outer side on its left), or one hash-joinable `=`; a constant
  false clause makes the join a dummy one. Built-in `=` of the geometric types, `xid = int4`
  (neither), `xid` / `cid` / `aclitem` (hash only), money, bit, tsvector, cross-type datetime
  (merge only) follow pg_operator; a clause it doesn't recognise counts as both, so it never
  errors where Postgres might not. Checked on the final plan (`check_merge_keys`), so EXPLAIN
  errors too.
- **Wrong results: a sort by a made type's ordering operator whose btree family has no
  comparison function for it sorted** (agent 10's equivclass made types: the merge join's
  `Sort Key: t1.b USING <` over `int8alias1`, whose family has `<` but no FUNCTION 1
  (alias, alias)). FinishSortSupportFunction errors "missing support function 1(T,T) in
  opfamily F" (XX000) as the sort starts, before any row is read: the Sort node checks
  `Catalog::sort_support_missing` for each key of a made type. A FULL / LEFT / RIGHT merge
  join over it, and `ORDER BY b USING <`, now error as Postgres does; with FUNCTION 1 added
  they run.
- cost_sort: a merge join's explicit sorts count as disabled nodes under enable_sort = off
  (`JoinGucs::sort`), so presorted index scans merging on the leading key win (join
  876 / 878 / 880).
- A semi join's clauses are distributed as an inner join's (deconstruct_jointree: ojscope
  NULL for JOIN_SEMI): one over its left side alone is that side's restriction, not a join
  filter, and an equality with a constant reaches the other side through its class
  (`ec_constants`), the join clause then implied (join 576: both sides of the partitioned
  semi join pruned to `p1`). An anti join's stay join clauses.
- Self-join elimination in a join group with LATERAL items (remove_self_join_rel replaces
  the removed relation in lateral_vars): the items' references to the removed scan read the
  kept one (join 617 / 618 / 619).
- paraminfo_get_equal_hashops: a Memoize over a LATERAL scan keys the values of the join
  clauses its parameterized path checks (its filter) first, then its own LATERAL references
  (a TABLESAMPLE's arguments) (join 606).

Goldens: `tests/sql/a16_sort_support.sql` (both errors, with the type oids masked),
`a16_join_parity.sql`.

Second batch (the same subset, against the first batch's binary, again on 5cfa72a and 9b0ac33: 3 units fixed, none
broken):

| file | fixed units |
|---|---|
| join | 772, 777 |
| aggregates | 116 |

- remove_useless_results_recurse: a pulled-up subquery's constant false (or NULL) WHERE on
  an outer join's nullable side goes to the front of the join's ON clause, the side a dummy
  rel (`Join Filter: false`, `(false AND (f1 = i8.q1))`): a LATERAL item's too, a RIGHT JOIN's
  left side, and a single scan made a dummy rel by it (`DummyInfo::qual_false`, the constant
  kept); a FROM list of several items keeps its quals (only single-child FromExprs are
  elided) (join 772, 777).
- A query level's grouping returns the GROUP BY keys its select list doesn't name (resjunk)
  under the sorts and limits above it too, and they carry them up to the level's top
  (`grouping_top`, `carry_group_junk`, make_sort_input_target); a sub-select's level as well
  as the statement's (aggregates 116: `Output: (sum((x.x + y.y))), y.y`).

Goldens: `a16_nullable_false.sql`, `a16_group_junk.sql`.

Looked at and left (why):

- join 612: a SubPlan's `a = t1.a AND a = t2.a` is one class with two Params,
  generate_base_implied_equalities_const giving `a = $2` and the gating `$1 = $2`. Doing this
  when the subquery is bound or planned changed EXISTS pull-ups (the semi join takes the
  rewritten clauses, a different join clause: `sj.a = t1.b` for `t1.a = sj.a`); it needs to run
  only once a sub-select stays a SubPlan.
- join 620 / 654 / 655: after self-join elimination Postgres keeps both relations' reltargets
  (the kept scan still returns a column only the removed join clause read, `t3.c`), and a
  physical target list for the kept scan; 632: Postgres removes the relation with the lower
  range table index, which for a pulled-up subquery's relations isn't the earlier leaf here.
- join 428–432, 424, 426, 548 (PlaceHolderVars: a pulled-up subquery's constant output wrapped
  only where an outer join nulls it, pullup_replace_vars_callback with varnullingrels; 429 /
  431 are their row order), 332, 440, 450, 771, 774 (nested PlaceHolderVars): agent 8's area.
- join 499, 543, 560, 715, 819, 888: cost-driven join orders and unique-ification choices.
- Seen while probing, not failing a unit (results match): an inner LATERAL join with a
  subquery proven empty shows its LATERAL output as `^1#0` in VERBOSE (`select * from i8 join
  lateral (select *, i8.q2 from i4 where false) ss on true`; Postgres `i8.q2`); a LEFT LATERAL
  join's ON clause over such a subquery is lost (`Join Filter: false` and `NULL` outputs for
  Postgres's `(false AND (f1 = i8.q1))` and `f1, (i8.q2)`); `WHERE null::boolean` shows as
  `false`; a pulled-up subquery's var-free WHERE that isn't constant (`now() > ...`) gates its
  scan (Postgres: the top of the join domain, or the ON clause of the outer join above);
  `IN (SELECT sum(b) ... GROUP BY a)` isn't shown over a Subquery Scan on `"ANY_subquery"`.
## Round 16, agent 19 (PlaceHolderVar class order, semi joins over FROM-less relations)

Measured in subset runs (test_setup, create_misc, create_index, create_view, select, union,
subselect, join, aggregates, inherit, partition_join, partition_prune, partition_aggregate,
equivclass, memoize, with, rowsecurity, updatable_views, select_distinct, groupingsets,
incremental_sort, merge, returning, rules, insert_conflict, join_hash, select_parallel,
rangefuncs, tsrf, limit, window, stats_ext, sqljson_jsontable), unit by unit against a run of
the base: 3 units fixed, none broken (on 357ae27, and again rebased onto f8c1317).

| file | fixed units |
|---|---|
| join | 366 |
| partition_join | 48, 207 |

What changed:

- generate_base_implied_equalities_no_const for a LATERAL subquery's output under a LEFT
  JOIN a WHERE reduces (a PlaceHolderVar the scan evaluates, read here as the join's value):
  its equality with a column an earlier clause already put in a class (the ON clause's
  `t1.a = s.b`) comes back `column = PlaceHolderVar`, the class's earlier member first
  (`Filter: (b = t1_1.b)`, `(a = t1_1.b)`), and is the scan's restriction, ahead of the join
  clauses; a column no clause had equated keeps the clause as written
  (`optimize::phv_class_order`, as `add_filter` places it). partition_join 48 / 207, and
  the same shape without partitions.
- remove_useless_results_recurse for a semi join (`reducejoins::remove_result_semijoins`,
  after reduce_outer_joins): an IN whose sub-select is a FROM-less relation, or a join on
  true of such relations reduce_outer_joins made inner, left or right (a full join stays a
  join), is its left side under the join's clauses with the right side's values put in
  (remove_result_refs); a FROM-less left side's Result gates on them, unfolded
  (`One-Time Filter: (1 = 1)`, create_resultscan_plan). join 366.

- Second batch (full runs before and after, unit by unit): `name = text` joins (nameeqtext /
  texteqname, in the hash text_ops family: hashname and hashtext hash equal strings alike)
  are hash keys (`optimize::name_text_pair`), Hash / Hash Semi / Hash Right Semi Joins as
  Postgres's, an inner or semi join's key showing the text column relabeled to the class's
  C collation (`Hash Cond: (nk.s = (tx.i)::text)`), an outer join's as written. Their class
  carries a constant the name side is equated with to the text side (`(i)::text =
  'A5'::name`, texteqname: select_equality_operator), dropping the join clause; a text
  side's `i = 'A7'::name` (under C) joins the class too, both sides compared with the
  constant, its own clause relabeled; its `i = 'A5'` (its column's collation) is a class of
  its own that stays on its side. A scan's own `text = name` restriction reading no other
  relation shows as written, against a column of its own too (`name = (passwd)::name`;
  agent 15 did it for pseudo-constants, select_views 25–39); one derived from the class
  comes relabeled. examine_variable looks through a COLLATE (a RelabelType once folded)
  for statistics.

- Third batch (the subset unit by unit on 3fb95cb and this batch, again rebased onto f334753: join 428, 429, 431, 432
  fixed, none broken; 429 / 431 are those plans' row order):
  - pullup_replace_vars_callback with varnullingrels: a pulled-up single-table subquery's
    constant output read in an outer join's own ON clause through joins inside the side that
    don't null it (as written) is the constant itself, not a PlaceHolderVar
    (`lift::unnulled_const`, deeper than the side right below the join that `direct` took):
    `t1.f1 = '***'::text` reads only the preserved side, the outer join's Join Filter over a
    Materialize, as Postgres's. When `direct` or this puts a constant in, the group is
    planned over the lifted tree (its inlining was dropped unless something else was
    lifted); a LATERAL value stays a PlaceHolderVar.
  - A Materialize over a join returns that join's target list (none, or `i8.q2`), not all
    its columns.
  - A join's target list below the node computing the level's: its first input's (as
    written: a Right join's inner side) columns and passed-up PlaceHolderVars, then its
    second's, the PlaceHolderVars a second input that is a join passes up after that side's
    Vars, those evaluated at the join last (build_joinrel_tlist, add_placeholders_to_joinrel):
    `Output: i8.q2, (NULL::integer)` over a Hash Right Join.

Goldens: `a19_phv_lateral_ec`, `a19_result_semijoin`, `a19_name_text_hash`, `a19_phv_on_const`.

Still open, and why:

- `name = text` as a merge key: Postgres sorts the text side under the class's C collation
  (`Sort Key: tx.i COLLATE "C"`); the merge paths here sort a side's columns by their own
  collations, so the clause stays a hash key only (a merge join shows only with hash joins
  off or presorted inputs).
- join 368: the dummy Result's target list is information_schema.column_udt_usage's
  expressions (`(c.relname)::information_schema.sql_identifier`). The engine reads that view
  as a virtual catalog computed by running its query apart, so the plan has no expressions
  of it to show; binding the view's definition in the query instead would change every
  query over information_schema (plans, row orders), and splicing its pg_get_viewdef text in
  for this case only isn't the planner's route. Left.
- join 345 (the PlaceHolderVar `64::cardinal_number` computed in the LATERAL subquery's scan
  `ss0`, the whole inner side parameterized by `t3`), 360 (a UNION ALL with a FROM-less arm
  as a parameterized appendrel: its arm a Result gated on the join's parameter, `((1) = 0)`,
  and the PlaceHolderVar `COALESCE(t.i, (1))` computed in a Result over it), 362 / 363
  (PlaceHolderVars with sub-selects: a copy of the InitPlan per place, the PlaceHolderVar
  recomputed above a join that doesn't carry it, the join order around the LATERAL `ss4`),
  partition_join 41 / 204 (the LATERAL subquery is a join: the partitionwise join of three
  relations, each child's inner join parameterized; here the inner join becomes a
  partitionwise join of its own first, the LATERAL clause a filter over it, and the
  non-partitioned plan also differs, a LEFT JOIN's ON clause acting as a class constant
  inside the LATERAL side (`t1.a = t3.b` derived) and the inner join costed differently).
  join 744 left to agent 17 (LATERAL parameterized join relations).
- From agent 16's list: join 430 (the same shape as 428 / 432 but the engine commutes the
  `i8 left join i4` below the others, a cost choice; the Hash under it also returns `i4.f1`),
  424 (Postgres's parameterized index scan on `tenk1_unique2` inside the Materialize; the
  engine hashes), 426 (a USING join alias over two subqueries' outputs of different types is
  Postgres's PlaceHolderVar `'123'::bigint`, folded; here `(123)` of the int side, computed
  at its scan), 440 (a Memoize over the LATERAL `ss2` Limit, keyed by `ss1`'s values), 548
  (a PlaceHolderVar a WHERE reduced away from its outer join is evaluated at its one
  relation's scan: the WHERE `stringu2 !~* CASE ...` is t1's filter there, a join filter here),
  332, 450 (join orders a PlaceHolderVar forces), 771 / 774 (nested PlaceHolderVars: the
  join order and its target list's order): not done.

## Round 16, agent 14 (parallel join inputs, partial DISTINCT orders, hash table sizing, inherited DML row marks, combine functions)

Measured in subset runs of 45 files (test_setup, create_misc, create_index, create_view,
create_aggregate, create_function_sql, create_table, inherit, select, aggregates,
select_distinct, select_distinct_on, with, join_hash, select_parallel, write_parallel, union,
subselect, join, groupingsets, incremental_sort, partition_aggregate, partition_join,
partition_prune, memoize, window, limit, update, delete, merge, returning, rules,
updatable_views, insert_conflict, equivclass, rowsecurity, stats_ext, tsrf, rangefuncs,
triggers, foreign_key, explain, generated_virtual, sqljson_jsontable, collate), against the
same subset run with 357ae27's engine (after rebasing onto agent 9's row-mark work): 17002 ->
17009 of 17106, 7 units fixed, none broken. No other agent's units changed.

| file | fixed units |
|---|---|
| join_hash | 72, 235, 237 |
| select_distinct | 90 |
| select_parallel | 132 |
| with | 283 |
| aggregates | 489 (`balk`, was "by design") |

What changed:

- ExecChooseHashTableSize's batch halving had its test inverted: Postgres halves the batches
  (doubling the buckets and space_allowed) while `nbatch >= space_allowed / BLCKSZ`, i.e. while
  that uses less memory with the batch files' buffers counted; the engine halved while it
  didn't, so a hash join estimated 8 batches costed and ran as 1 (`joins::hash_table_space`).
  A Parallel Hash is sized for all the participants' rows (the partial rows times the
  parallel divisor, initial_cost_hashjoin's inner_path_rows_total) in their combined hash_mem,
  falling back to one process's when the table doesn't fit (`parallel_hash_table_size`, used by
  the costs and by EXPLAIN ANALYZE): join_hash 72's 8 planned batches, and the parallel hash
  join costs (412.50 where the engine had 379.50).
- EXPLAIN ANALYZE's batch growth goes by the hashed values' real sizes: the executor counts
  each inner column's bytes as a tuple stores them (`NodeStats::hash_col_bytes`, also under the
  inner relation's scan below a merge join's Sort, which EXPLAIN may show hashed), and the
  Hash node knows the columns its table keeps (those returned above, the keys', the quals'),
  so a MinimalTuple of a 320 kB value is that big, not the planner's width; growth checks the
  table against space_allowed as ExecChooseHashTableSize left it (doubled with each halving);
  a Parallel Hash growing from one batch goes to two per participant with one process's
  hash_mem and a bucket array for the tuples expected per batch
  (ExecParallelHashIncreaseNumBatches), each participant may hold one chunk whatever its size
  (join_hash 237: `Batches: 8 (originally 1)`, `Buckets: 1024 (originally 2048)`).
- An outer join's partial paths are built either way round (populate_joinrel_with_paths: the
  LEFT join first, then the RIGHT one with the sides swapped; a RIGHT one by hashing only with a
  Parallel Hash, a nested loop only as a LEFT join), shown as the join type they run as; a merge
  join's sorted inputs keep their relations' unsorted paths and partial plans (`presort`), so
  the join relation's partial hash and nested loop paths are there however the serial plan
  joins (join_hash 235: Parallel Hash Left Join under a Finalize Aggregate, where the serial
  plan was a Merge Left Join).
- A LATERAL join's outer relation has its Gather path too (generate_useful_gather_paths at the
  base relation): a serial nested loop reads it when cheaper (select_parallel 132; the inner
  side, parameterized by the outer, has no partial paths).
- create_partial_distinct_paths in full: a sorted partial DISTINCT over every partial path of
  the input (an ordered parallel index scan included), in the DISTINCT's own key order and
  reordered to lead with the path's own keys (get_useful_pathkeys_for_distinct, under
  enable_distinct_reordering), each made ordered as make_ordered_path does (sorted fully only
  the cheapest; incrementally the others); add_partial_path / add_path with compare_pathkeys
  over the plans' orders; Gather Merges of each sorted partial DISTINCT; the final DISTINCT over
  each gathered path in its useful orders; the survivors beside the cheapest (`alt`), so a
  LIMIT above takes the one cheapest for its fraction (select_distinct 90: Unique over a
  Gather Merge of a Unique over a Parallel Index Only Scan, `Workers Planned: 1`).
- UPDATE / DELETE over joins, EXPLAIN VERBOSE: the other relations' whole-row row marks
  (ROW_MARK_COPY) of a CTE, VALUES or a function (`wcte.*`); an inheritance tree's target list
  is the new values, the other relations' row marks, then its row identity
  (add_row_identity_columns: `tableoid`, `ctid`), which each member's scan returns last, a
  Sort / Hash over its Append returning the members' columns under the tree's name; a target's
  scan that reads its ctid above a join builds no physical target list (use_physical_tlist
  refuses system columns), an ONLY target whose table has children included (with 283). Agent
  9's one-member case is kept as it is; this covers trees of several members, and the whole-row
  marks of everything but FROM subqueries (whose Subquery Scans agent 9 keeps).
- An aggregation EXPLAIN shows as a Finalize Aggregate over partial ones runs a user-defined
  aggregate's combine function as Postgres's executor does (advance_combine_function's
  strictness rules) over the one partial state the engine's single process makes: the
  aggregate's COMBINEFUNC is bound with it (`UserAgg::combine`), and the executor asks EXPLAIN's
  plan whether the aggregation is split (`explain::agg_split`, only for aggregates with a user
  combine function: a built-in's combining one partial state gives the serial result). So
  aggregates `balk` returns NULL as in Postgres; it was the last unit classed "by design".

Goldens added: `tests/sql/a14_parallel_shapes.sql`, `a14_dml_rowmarks.sql`,
`a14_combine_split.sql`.

Still open in these files, and why:

- with 235, 238, 241 (fixed in round 16, agent 20): since agent 9's MERGE sub-select copies, two things are left. An
  untyped literal in a SELECT's (a CTE's, a FROM subquery's) target list is resolved to text
  (resolveTargetListUnknowns) and shows `'cte_basic val'::text`; the engine keeps the constant
  `unknown` and shows `'cte_basic val'` (top-level `EXPLAIN VERBOSE SELECT 'lit'` too). And a
  MERGE's / UPDATE's joined FROM subquery returns its whole-row mark `o.*` up the plan
  (ROW_MARK_COPY); a DELETE ... USING a subquery marks the subquery's own table (`m_1.ctid`)
  instead. (241's `merge_source_cte.*` row mark is in now.)
- join_hash 269 / 271: SubPlan numbering (agent 11's analysis above), and a scan reading
  `tableoid` returns its reltarget (no physical target list) in reltarget order.
- Costs only, not failing a unit: a LATERAL nested loop's rows are the outer rows times the
  parameterized inner path's (Postgres: the join relation's size from the join clause's
  selectivity, 33333333 vs 33330000).

## Round 16, agent 9 (rule-rewritten statements' plans, MERGE sub-select copies, row marks, locks pushed into subqueries)

Two full runs of the schedule in one database (`PGREGRESS_TIMEOUT=300`) on this round's commits
over e038970: 51013 with the first three commits, 51017 with all of them. Against the counts
below before this round, +6: merge 348, returning 103, foreign_key 516, updatable_views 817,
819, 914 (merge, foreign_key and updatable_views now match fully). Three units lost in both
runs aren't this round's: alter_table 953 (`to_json(bt.*)` for Postgres's `to_json(ROW(bt.id,
bt.stuff))`, from 1a2f31a's whole-row Vars), and domain 482 / oidjoins 0, timeouts (the
statement ran past 300 s). Both time out the same way in a full run of the base commit
e038970, so they came with earlier commits. The table keeps their earlier counts.

- **MERGE 348** (`sql/paths/sublinkcopy.rs`): a pulled-up source subquery's output computed by
  a sub-select (`(SELECT count(*) ...) cnt`) is copied into each place reading it, as
  pullup_replace_vars does: the join's clauses, each action's condition, SET and VALUES, and
  RETURNING. Each copy is a SubPlan of its own, with its own relations (named in the order the
  planner meets them: the join's, then each action's target list, then its qual). A copy read
  by a multiple-column SET's sub-select (`SET (b, c) = (SELECT s.b, s.cnt)`) is nested into it
  as an InitPlan, once per sub-select. A join clause's correlated SubPlan is now costed
  (cost_qual_eval), which turns the join into Postgres's Hash Join. The source keeps the
  columns its actions, join and RETURNING read.
- **Rule-rewritten statements' EXPLAIN** (`explain.rs` `rule_statements`): a statement a rule
  rewrote into several (an INSTEAD action, run as a `*RULE*` CTE under a statement that
  returns nothing; a qualified or DO ALSO rule's Append of statements) is shown as Postgres's
  list of queries: each plan apart, a blank line between. The statement's relations are named
  before the action's target and FROM items (rewriteRuleAction's range table order); an
  inheritance parent's nominal name is reserved (returning 103, updatable_views 817,
  foreign_key 516). An INSERT's single VALUES row of constants is NEW in the rule's
  qualification (ReplaceVarsFromTargetList), and `pull_up_consts` puts a constant row's values
  into a filter's sub-selects and merges a constant join side gated by such a clause. The
  qualification is then an uncorrelated InitPlan with a One-Time Filter (updatable_views 819).
- **Row marks.** An UPDATE / DELETE / MERGE's joined subquery, a security barrier view's too,
  keeps its Subquery Scan (its whole-row mark), and a pseudoconstant qual over it shows inside
  it (updatable_views 914). A locking level's non-pulled-up FROM subquery shows its whole-row
  mark (`s.*`) with the locked relations' marks; an inheritance parent's tableoid comes after
  all of them, and its member scans and Append are in the parent's reltarget order. The
  non-target relations of a data-modifying statement and a locking level's unlocked ones
  (ROW_MARK_REFERENCE) return their ctid, so they get no index-only scan
  (`rowmarks::referenced`, before access paths are chosen).
- **Locks pushed into FROM subqueries and views** (a correctness fix): FOR UPDATE / SHARE with
  no relation list, or naming a FROM subquery or a view, now locks the relations inside it
  (transformLockingClause / markQueryForLocking). Before, a subquery's or a view's rows were
  never locked. A subquery planned apart locks every row it returns (its own LockRows); one
  pulled up joins the statement's level, which locks only the rows it returns.
  CheckSelectLocking's errors follow (aggregates, GROUP BY, a UNION in FROM), and a VALUES list
  and a view's set operation lock nothing. Test: `tests/wire.rs`
  `row_locks_pushed_into_subqueries_and_views`.

Goldens: `a9_dml_rowmarks`, `a9_rule_plans`.

Left:
- returning 105: the view's NEW values are computed below the join, where Postgres computes
  them in the ModifyTable's target list, and the top join's equivalence-class member differs
  (`foo_2.f2 = foo_1.f2` for Postgres's `foo_1.f2 = joinme.f2j`). These are plan internals,
  not names.
- `FOR UPDATE OF s` where the level has no locked relation of its own: Postgres still puts a
  LockRows carrying only `s.*` at the top; the engine shows none.
- A whole-row mark needs all the subquery's columns: Postgres's Subquery Scan returns them
  (`s.*` reads every one), while the engine's unused-output pruning drops them (`s.other`
  missing below a FOR SHARE subquery).
- VERBOSE target lists of an UPDATE joined with an inherited target (the target's tableoid,
  the joined subquery's `s.*` junk).
- A semi join unique-ified under FOR UPDATE: `rowmarks::key` has no case for the Distinct, so
  the level's rows are locked by its scans, not by one LockRows at the top.
- MERGE costs: the per-row SubPlans in the actions' target lists aren't charged (Postgres's
  ModifyTable total is far higher), and a nested copy's InitPlan is counted in its Result's
  total as well as its startup.
- Not this round's, found by it: domain 482 (`information_schema.column_domain_usage`) and
  oidjoins (its DO block over every catalog) run past 300 s in full runs, over the catalog the
  earlier files leave (also on e038970), but return at once on a fresh database.

## Round 16, agent 12 (UNION ALL appendrel names, LATERAL partitionwise joins)

Measured in subset runs (test_setup, create_misc, create_index, create_view, select, union,
subselect, join, aggregates, inherit, partition_join, partition_prune, partition_aggregate,
equivclass, memoize, with, rowsecurity, updatable_views, select_distinct, groupingsets,
incremental_sort, merge, returning, rules, insert_conflict, join_hash, select_parallel,
rangefuncs, tsrf, limit, window, stats_ext) on 27da13f, against the subset's baseline: 7 units
fixed, none broken (rowsecurity's three DROP USER units differed from a concurrent run's
database on the shared reference).

| file | fixed units |
|---|---|
| partition_prune | 392, 393, 399 |
| join | 374 |
| partition_join | 45, 46, 206 |

What changed:

- A UNION ALL flattened into an appendrel keeps its parent range table entry's name
  (`Plan::Append`'s `alias`): a FROM subquery's alias, or `*SELECT* 1` for a query level's own
  UNION ALL (flatten_simple_union_all; none when a leaf's columns are coerced, as
  is_simple_union_all). The Append references it (apprelids), so it is named before its
  members (`ab`, then `ab_1`…); arms kept as subqueries (a WHERE: is_safe_append_member) name
  their relations after the level's own, as setrefs reaches them, nested UNION ALLs flattened
  into the appendrel included (partition_prune 392 / 393 / 399, whose InitPlan now shows at the
  top Append too).
- restriction_is_always_true in a LATERAL UNION ALL's arms: the WHERE over the union goes into
  each FROM-less arm where the join's parameters are known, and an arm outputting a NOT NULL
  column no outer join below nulls drops its `IS NOT NULL` (an `IS NULL` drops the arm)
  (join 374). Only when that changes an arm; else the planner pushes the clauses as before.
- Partitionwise LATERAL joins of two items (`lateral_partitionwise`, the join search doesn't
  take those): when both sides are inheritance Appends partitioned the same way and every
  LATERAL member checks `param = its key` with the param the outer side's key, the join is the
  Append of the joins of matching partitions, level by level for sub-partitioned tables (a
  LEFT JOIN only when the inner side has every partition the outer has). Each child is a
  nested loop of its own; a member's projection and the subquery's are one, so a pulled-up
  scan lifts into the child's join tree.
- A LATERAL inner relation (a scan, or an appendrel of scans) reading the outer side's values
  is costed for the outer rows as its loops (get_loop_count), so its parameterized index scans
  compete as Postgres's do (partition_join 46: an index scan per partition).
- Partitionwise join Appends carry a stable id (`Plan::Append`'s `pw`, unique in the process)
  the planner keeps their partitioning under, not the first child's address, which a dropped
  plan's replacement could reuse (agent 13's audit).
- Harness: `PGREGRESS_HUNKS` (differing units per file in `hunks/`, default 12) and
  `PGREGRESS_ENGINE_BIN` (an engine binary copied aside, so the source can change while a run
  waits for a heavy slot).

- Second batch (subset run of the same files on 23e447c: partition_prune 746 fixed, none
  broken): an ordered Append under run-time pruning shows the members pruned as the executor
  starts as each level's own `Subplans Removed` (it fell back to one Merge Append when the
  shown members didn't count as many as the plan's), a nested Merge Append whose first member
  went names its keys unqualified, and a sub-partitioned level is named after its first member
  kept; an ordered Append wins over sorting all the members when their totals are fuzzily
  equal and it starts sooner (add_path's STD_FUZZ_FACTOR); a window over an inheritance tree
  reads it in the window's order when that costs less (it always sorted); a window's
  PARTITION BY / ORDER BY keys missing from a subquery's select list are resjunk, so its
  Subquery Scan shows, under its unique name (`*SELECT* 1_1` beside the appendrel's
  `*SELECT* 1`); a numbered inheritance Append is named after its first member only under a
  partitionwise join (a UNION ALL arm's took its member's name).

- Third batch (subset run on c3f5839: join 643 fixed, none broken): an inner join's WHERE
  equalities with constants as generate_base_implied_equalities_const makes them: a class
  with a constant and more than one other member (or source) gives `member = constant` for
  each member, however written (`3 = j2.c` shows `c = 3`), and a scan's constant clauses come
  in the order their classes were made (a merged class keeping its left operand's place),
  the ones derived across the join (`ec_constants`) among them (`ec_const_order`,
  `order_by_places`). A join clause is between each side's first member of its class
  (generate_join_implied_equalities_normal: `t1.a = t2.b` after `t1.a = t1.b`), only through
  equalities a side checks itself (join 609 / 610; a first version also went through other
  join clauses, `t1.a = t2.a AND t1.a = t2.b` becoming two `t1.a = t2.a`, a wrong result the
  subset run caught: partition_join 22).

Goldens added: `tests/sql/a12_union_appendrel.sql`, `a12_lateral_pw.sql`,
`a12_ordered_append_pruning.sql`, `a12_ec_const_order.sql`.

Still open from agent 5's list, and why:

- partition_prune 265 (an AlternativeSubPlan's EXISTS copied into each index condition: PG
  shows the plain SubPlan there and a hashed one elsewhere), 410 (a cross-partition UPDATE's
  moved row seen by a later bitmap index scan of the same command: an index entry of an
  invisible tuple counted, `Heap Blocks: exact=1`), 746 (window functions over ordered
  partitions: an ordered Append with a nested Merge Append isn't chosen when run-time pruning
  removes members, `count_members` against the shown members; and the arms' Subquery Scans
  on `"*SELECT* 1_1"` / `"*SELECT* 2"` aren't shown): fixed in the second batch.
- partition_join 48 / 207: partitionwise now, the clause equating the LATERAL output with a
  class member is generated `column = output` by Postgres (a new member equated to an old
  one), written order here; 204 needs the PlaceHolderVar `least(...)` (agent 8's area); 21
  (a merge join's sort key: Postgres takes the first class member in the scan's target list,
  the reltarget order for a partition), 74 (a parameterized index scan on an expression
  index costed differently), 86 / 88 / 92 (IN over a partitioned join subquery planned
  partitionwise before its pull-up).
- join 442 (the derived `tt4.f1 = 'foo'` is valid in every join order but dropping the join
  clause only when tt3 ⟕ tt4 is joined first; plus the subquery scan's derived filter and a
  Memoize), 502 / 503 (a grouped subquery's sorted path kept beside the cheapest for a merge
  join), 123 (the class clause's first derivation fixes its orientation), 410 / 414 (join
  orders: parameterized join relations for LATERAL items aren't built); 609 / 610 / 643 fixed
  in the third batch. sqljson_jsontable 101 (agent 11's): JSON_TABLE is a call joined to the row it reads
  (`from_call`), no item of the join search, so the search never makes `x ⋈ jt` parameterized
  by `y`; join 414 needs the same parameterized join relations for a LATERAL item, inside an
  outer join (and a hash join under the nested loop). partition_join 48 / 207: the clause
  equating the LATERAL output (a PlaceHolderVar evaluated at the scan) with a column is a
  restriction Postgres generates from the class (`b = t1_1.b`, first); here the output is the
  outer row's value and the clause stays as written.

## Fixed in round 15, agent 10 (equivalence classes: broken classes, made operators, nondeterministic collations, Merge Appends as join inputs, GROUP BY reordering)

Measured in subset runs (the 46 files of `test_setup`, insert_conflict, create_misc,
create_table, create_index, create_view, create_function_sql, select, inherit,
updatable_views, select_distinct, select_distinct_on, subselect, union, join, aggregates,
hash_index, gist, privileges, collate, rowsecurity, groupingsets, join_hash, merge, tsrf,
incremental_sort, generated_virtual, rules, stats_ext, select_parallel, foreign_key, window,
equivclass, sqljson_jsontable, limit, rangefuncs, rowtypes, returning, with, partition_join,
partition_prune, partition_aggregate, partition_info, tuplesort, explain, memoize, plus
collate.icu.utf8), compared unit by unit with the same subset run on the engine before:
15 units fixed, none broken; a second batch (rebased onto e038970, the same subset) fixed
1 more, none broken.

| file | fixed units |
|---|---|
| equivclass | 38, 48, 52, 59, 87, 94 (all) |
| inherit | 445, 453 |
| aggregates | 455, 458, 461 |
| collate.icu.utf8 | 328, 630, 647; second batch: 644; third batch: 611, 619, 622; fourth batch: 334 (all) |
| join | 817 |

What changed:

- Made operators (CREATE OPERATOR) between different types are merge / hash join clauses when
  they say MERGES / HASHES and are an equality of a btree / hash family (`user_eq_props`); one
  without a commutator is usable only with its outer side on the left
  (select_mergejoin_clauses / hash_inner_and_outer), so a FULL JOIN takes the other side as
  its outer one (87/94). An inner join's such equality goes into its class, which joins the
  first relation's member with the second's: the operator the other way round doesn't
  exist, the class is broken and its clause comes back unchecked for hashing (no hash join;
  Postgres only marks clauses distribute_restrictinfo_to_rels places). A made type without a
  default btree class sorts by its family's `<`, shown `USING <`. A disabled join method
  shows `Disabled: true` (initial_cost_* count the join itself).
- Broken classes (an `=` missing across the made types): the constant's clause stays twice
  (derived and as written, generate_base_implied_equalities_broken; `BROKEN_DUPS` keeps the
  scan's class from merging them), and a scan's own equalities bring a join column into the
  constant's class (`ec1.ff = ec1.f1` with `ss1.x = ec1.f1` and `ec1.ff = 42` gives
  `ss1.x = 42` in each UNION ALL arm, 48). A relation's `col = constant` restrictions count
  for Inner Unique (relation_has_unique_index_for), and a unique index's expression is
  unique to examine_variable (`expr = constant` estimates one row). A parameterized
  appendrel member costs its target list's expressions (cost_index's pathtarget cost).
- Merge Append paths of append relation leaves (UNION ALL subqueries, inheritance trees) in
  the join search (generate_orderedappend_paths): for the ORDER BY's keys and for columns
  equated with another leaf's, when some member reads the order from an index; built as the
  Sort over the Append EXPLAIN shows as a Merge Append (`merged_cost`, `order_members`). A
  Merge Append can't mark and restore (a Materialize as a merge join's inner side), a merge
  join's own Materialize is labelled as its input plus cpu_operator_cost a row, and a merge
  join's inner sort is never incremental (52, 59, inherit 453).
- EXPLAIN names: a pulled-up UNION ALL's arms (Append / UNION ALL SetOp) are of their level,
  numbered after its own relations, arm by arm, the UNION ALL placed by its first relation
  (`level_scans`).
- A projection over a UNION ALL merged in order: the members compute the sort expressions
  (prepare_sort_from_pathkeys), a Result above projects the query's columns unless they are
  the Merge Append's own (inherit 445, bug #18652).
- Equivalence classes under nondeterministic collations (`Clause::ecable`): a class of the
  clauses' collation, kept apart from other collations' (deterministic ones all compare
  bytes); a scan's members equated there (`c = b`), one join clause per class (630, 647).
  Partition keys matched through a class only under the key's collation (have_partkey_
  equi_join's check; this first gave wrong counts in 631, fixed before the batch); full
  partitionwise aggregation needs the grouping collation to be the partition key's (611's
  plan shape; its keys still show without `(c)::text`).
- GROUP BY orderings (get_useful_group_keys_orderings): the group pathkeys with the ORDER BY's
  keys first (preprocess_groupclause), and reordered to the input's order with the rest in
  that order, only with incremental sort or when the input covers them all; an ordering other
  than the ORDER BY's own is charged the sort above it (458). Presorted aggregates (DISTINCT
  / ORDER BY in the aggregate) take the reordering too, no sort when the input is in order
  (461), and an index-only scan the aggregation reads as it comes returns the index's
  columns (use_physical_tlist under CP_LABEL_TLIST). A merge join's outer incremental sort
  estimates its presorted keys' groups (455).
- A hashed grouping and a sorted one only fuzzily as cheap: both kept, the cheaper in total is
  the relation's cheapest path (add_path / set_cheapest), not the ordered one (328).

Goldens added: `tests/sql/a10_ec_alias.sql`, `a10_group_reorder.sql`, `a10_nondet_ec.sql`.

Second batch: a restriction comparing two columns of one relation (`c = b`, also one an
equivalence class derives) is estimated with DEFAULT_EQ_SEL, as eqsel does when
get_restriction_variable finds two Vars of the relation (it was 1/ndistinct, a join's
estimate): `costs::Est::same_rel` marks the estimates of one relation's restrictions, so the
per-partition nested loops of 644 cost what Postgres's do. Golden: `a10_varvar_sel.sql`.

Third batch (on 4aef367, the same subset: 3 more fixed, none broken): a written COLLATE over a
column of another collation is a RelabelType in Postgres's plan, shown `(c)::text` (keys show
top-level casts, show_sort_group_keys): a grouping's key so, references to it wrapped
(`((c)::text)`, its input computing it, the column bare at the top of the input's target
list), a Sort over it `((c)::text) COLLATE "C"`, and an inner join's clause from its class
with both sides relabelled to the class's collation (`((t1.c)::text = (t2.c)::text)`).
ANALYZE sorts and groups a column's values under its collation (attcollation, a made one's
oid resolved from the catalog): a nondeterministic collation's equal values one MCV, an ICU
collation's order in the histogram and correlation, as `pg_stats` shows them; the join
estimate under `COLLATE "C"` over those MCVs (eqjoinsel) then takes 619's partitionwise join.
Golden: `a10_collate_relabel.sql`.

Fourth batch (on 6245681, the same subset: 1 more fixed, none broken): an EXISTS over a FROM
subquery planned as a level of its own (`SELECT 1 FROM (SELECT DISTINCT x ...) t2 WHERE
t1.x = t2.x COLLATE ci`) is a semi join with the subquery (convert_EXISTS_sublink_to_join
keeps it an RTE): the correlated clauses the binder pushed into the level's scan come back
out over its outputs, through its DISTINCT, sorts and projections of columns
(`sublink::barrier_corr`), so the DISTINCT is hashed below a Hash Semi Join (334). Golden:
`a10_exists_distinct.sql`.

Still open in these files, and why:

- Seen while probing, not failing a unit: a constant equated with a column under a
  nondeterministic collation joins no class across a join (Postgres derives the other side's
  `c = 'a'::text COLLATE ci` and drops the join clause); estimates of a nondeterministic
  column against a constant compare the MCVs bytewise, not under the clause's collation.
  The same EXISTS over a DISTINCT subquery with a plain `t1.x = t2.x`: Postgres reduces the
  semi join to an inner join with the unique subquery and merge-joins it (here a Hash Join);
  as NOT EXISTS, an anti join's clause is no class's, so its outer column shows unrelabelled
  (`(t1.x = (t2.x)::text)`, here both sides relabelled). Results match.
- updatable_views 173 / 174 match since agent 9's `ec_constants` change (an input's own
  restrictions are class members: the rule action's join takes the subquery's `a = 2`;
  counted in agent 11's round 16).

## Round 16, agent 11 (search-path operator / function resolution; the long tail: RETURNING row types, whole-row Vars, group orders, dummy subqueries)

Measured in a 46-file subset run (`test_setup`, create_misc, create_table, create_index,
create_view, create_function_sql, select, inherit, insert_conflict, updatable_views,
select_distinct, select_distinct_on, subselect, union, join, aggregates, hash_index, gist,
collate, rowsecurity, groupingsets, join_hash, tsrf, rules, generated_virtual, window,
equivclass, sqljson_jsontable, limit, rangefuncs, rowtypes, with, partition_join, tuplesort,
partition_info, memoize, returning, foreign_key, stats_ext, incremental_sort, select_parallel,
partition_prune, partition_aggregate, merge, explain, privileges) on 27da13f plus this batch:
every file at or above this table's counts, 18 units above (16 below; updatable_views 173 and
174 also match in that run, after upstream's 7d597df, and are counted now). An earlier unit-by-unit
comparison with a baseline run of the same subset (on 1a7c139) showed no unit broken.

| file | fixed units |
|---|---|
| rowtypes | 217, 219, 225, 227 |
| rowsecurity | 592, 1122 |
| tuplesort | 102, 105 |
| groupingsets | 199, 201 |
| stats_ext | 112 (`check_estimated_rows` value) |
| updatable_views | 848 (**row order**: a semi join's Hash Right Semi Join) |
| insert_conflict | 12 |
| create_table | 112 |
| rangefuncs | 422 |
| generated_virtual | 456 |

What changed:

- **Wrong errors and results: operators and functions along the search path** (agent 13's
  find). OpernameGetCandidates / FuncnameGetCandidates: pg_catalog is searched where the path
  lists it (first when it doesn't), and of the same argument types the schema searched first
  wins, the later one hidden. `SET search_path = s, pg_catalog` with `s.=(int, int)`: `1 = 2`
  is `s.=` (it errored "operator is not unique"); with pg_catalog first (implicitly too) the
  built-in wins, where a user function of a built-in's name and argument types used to win
  (`abs(-1)` ran `s.abs`, and `abs(smallint)` too). The planner's clauses made from operator
  family members (rebuilt hash keys, LIKE-prefix / inet index conditions, boolean index keys)
  take the built-in operator, whatever the path finds; a hash or equivalence-class key comes only
  from the built-in `=` (a CREATE OPERATOR `=` over another built-in function isn't equality);
  a built-in operator the path wouldn't find by name shows as `OPERATOR(pg_catalog.=)` (EXPLAIN,
  `Hash Cond`, view definitions). Before, a user `=` of built-in types that won made a join wrong
  (its keys rebuilt with the path's `=`).
- **Result types**: RETURNING `old` / `new` (and the view's own whole row) of a write through an
  automatically updatable view have the view's row type, not `record` (RowDescription and
  `pg_typeof`; agent 2's "found, not fixed").
- **Row order**: EXISTS over an inheritance tree whose members' columns are in another order (a
  multiply inherited child) is pulled up into a semi join; a semi join's clause with a left
  column fixed to a constant fixes the right column (and goes), at the pull-up and when a filter
  is pushed down to it later (updatable_views 848: Postgres's `Hash Right Semi Join` rows).
- A CTE's whole row (a CTE Scan's, a multi-column function scan's) is a whole-row Var in EXPLAIN
  (`r.*`), its `IS [NOT] NULL` test not expanded (a non-inlined CTE is never pulled up); a CTE
  proven empty keeps its columns' names (`t.c`, not `NULL`).
- A FROM subquery proven empty before it was planned (`... ss WHERE false`): its record columns'
  fields are `fN` (get_name_for_var_field's fallback) and it counts in the range table as the
  relations its query reads (flatten_unplanned_rtes): `(a).f1` unqualified over a FROM-less one.
- A filter over a FROM-less subquery that reads an enclosing query's values is its gating qual
  (`One-Time Filter`), its sub-select an InitPlan (rowtypes 219).
- A GROUP BY key that ORDER BY names takes that item's direction (transformGroupClause copies its
  sort operator), wherever it is in the list: the grouping's sort, the joins planned for its
  order (a merge join sorted DESC) and the presorted aggregates' sort (now incremental over the
  input's order) follow it (tuplesort 102/105).
- A DISTINCT by sorting: the Sort refers to what its input computes (`((a + 1))`).
- Grouping sets compute their expression keys below the aggregation (make_group_input_target);
  over a pulled-up one-row VALUES the keys are over its values (groupingsets 199/201); a virtual
  generated column's expression is the scan's (generated_virtual 456). The statement's grouping
  shows the GROUP BY keys its select list doesn't name (resjunk) after it.
- Row-level security: a cheap leakproof qual of the statement goes ahead of a security barrier
  view's quals (security level 0), after the policies' cheaper ones (rowsecurity 592).
- An INSERT from a FROM subquery not pulled up (ORDER BY) whose row isn't the new row as it is:
  its Subquery Scan computes the new row (`ss.b, ss.c, NULL::integer`, rowsecurity 1122).
- An EXISTS sub-select is planned for its first row (make_subplan's tuple_fraction: an
  index-only scan, insert_conflict 12); an EXISTS InitPlan costs that row.
- A FROM subquery over one function scan is pulled up into the join group (rangefuncs 422).
- A partitioned table's whole row over a partition scan is the converted row (`(t.*)::parent`,
  create_table 112); a single partition left keeps the partitioned table's (inherited)
  statistics, extended ones included (stats_ext 112).
- ROWS FROM's calls show in one `Function Call:` line.

Goldens added: `tests/sql/a11_long_tail.sql`, `a11_search_path_ops.sql`.

Looked at and left (why):

- join_hash 269/271: SubPlan numbering. Postgres numbers sub-selects in the order it
  preprocesses the level's expressions (target list, then the join tree's quals in order, ...;
  a FROM subquery's after its parent's), the engine in the order EXPLAIN meets them (a scan's
  filter before the join's clauses). Renumbering needs that order kept from the binder for every
  sub-select (CTEs, hashed alternatives, MIN/MAX InitPlans included).
- sqljson_jsontable 101 (row order): Postgres joins `x` with `JSON_TABLE(... PASSING x, y)`
  first, parameterized by `y` (a join rel with an unsatisfied LATERAL reference), and never joins
  `x` and `y` clauselessly while they have a join-order restriction; the engine's join search has
  neither.
- foreign_key 516, updatable_views 817/819/914: EXPLAIN of a rule-rewritten statement shows each
  resulting query's plan apart (a blank line between), each planned on its own (an equivalence
  class through the rule's OLD join); the engine runs the actions as a CTE of the original (the
  rule-rewritten-statement work of returning 103/105 and updatable_views 173/174).
- groupingsets 55/57 (a LATERAL subquery's sub-select output under GROUPING), 189/191/195/197
  (copies of a sub-select in GROUPING arguments each a SubPlan of its own; the engine shares
  one), 215 (a ProjectSet over the grouping returning its keys).

## Round 16, agent 13 (session and thread state audit)

Every `thread_local!`, interior-mutable `static` and cache on the engine, session and catalog
structs was checked for a statement whose output depends on what ran before it (the inventory
and the rule for new caches are in HANDOFF.md, "Session and thread state"). No regress unit
changes: the suite runs each file on a connection thread of its own, which hides most of this.

Fixed (reproduced first by a golden recorded from PostgreSQL):

- **Wrong results: a SQL function's sub-selects reused their earlier call's value.** Subquery /
  CTE ids were per binder (2, 3, …), but a SQL function's statements, each bound by a binder of
  its own, run in their caller's `ExecCtx`, whose InitPlan results (and hashed EXISTS / scalar
  sub-selects, CTE tuplestores) are cached by id: `SELECT c(1), c(2)` with
  `c(int) ... AS $$ CALL p($1 * 2); SELECT (SELECT count(*)::int FROM log) $$` gave `1 | 1`
  (PostgreSQL `1 | 2`). Ids are now unique in the process (`Binder::fresh_id`). Golden:
  a13_sqlfn_subquery_ids.
- **A DO block reused another run's scan.** PL/pgSQL's `PARSED` (by body) and `PLANS` caches
  handed a DO block the parse / plans of the same body run before under the other
  standard_conforming_strings (`'a\\b'` printed `a\b` with the setting on). And `LEXED` (an
  expression's first scan) was shared by functions with the same body, so a function first run
  under another setting kept another function's scan. Keyed now by the setting (DO) and by the
  function (database, oid). Golden: a13_plpgsql_scs.

Fixed as hardening (not reproduced in a test; each could hand out a wrong entry):

- `EQ_OPS` (the `=` per type that hash keys are rebuilt into clauses with, round 15's lost join
  clause): kept across statements, catalogs and search paths; now started over for each catalog
  / search path. (A rebuilt key still takes the `=` the search path finds, not the one the
  clause named: `OPERATOR(pg_catalog.=)` under a search path whose first schema has an
  `int = int`. Not reachable today, see below.) Golden: a13_join_eq_search_path.
- Caches keyed by an address that a later allocation can take: `SYS_ROWS` (a system catalog's
  rows, by catalog address), PL/pgSQL `PLANS` (by database address), a prepared statement's
  generic plan cost (by binding address). Now a `Weak` or `Database::instance`.
- Process-wide caches keyed by oids, which repeat across engines and stores of one process:
  `sizing::METHODS` (compression methods; also by xid and row key), range constructors
  (`func::range::user_constructors`), PL/pgSQL `COMPILED` / `VALIDATED`. Keyed by
  `Database::instance` / the whole type now. Test: tests/session_state.rs.
- `Txn::replace_catalog` (a commit rebased past shared-catalog updates) gave the catalog
  `cur + 1`, a version another transaction's catalog could have: now `bump`ed.
- CTE estimates pushed by `define_ctes` stayed on the thread when a later CTE failed to bind
  (a leak, and entries a later statement's lookup by name could find): popped by a guard now.

Guard: `golden_shuffled` (60 files in a shuffled order on one thread, in `cargo test`;
`GOLDEN_SHUFFLE=all` for every file). Every file ran shuffled on the old engine (seed 0xa13) and
on the fixed one (seeds 0xa13 and 7): no file differed by its order (on the old engine only the
two new goldens differed, and they differ alone too). A regress subset (create_function_sql,
triggers, subselect, plpgsql, rangefuncs, polymorphism, with) matches as before (3736/3746).

Second batch (state an engine panic leaves, the last binder's catalog):

- **A panic left flags set for the session's later statements.** The server turns an engine
  panic into an ERROR and the session goes on, but flags set and restored by hand stayed set
  on its thread: after one, `pg_get_viewdef` printed views on one line (ruleutils'
  `NO_INDENT`), the next statements' notices carried the panicked call's CONTEXT line (the
  session's frames), scanner warnings went missing (`scs::QUIET`), regex matches checked the
  old statement's deadline (`regex::INTERRUPTS`), a CLUSTER's table owner stayed
  `current_user` (the session's definer roles), and the planner kept security-qual,
  PlaceHolderVar, EXISTS pull-up and EXPLAIN flags. All are guards now (`crate::scoped::set` /
  `set_ref`, `Begun`'s and the frames' `Drop`); scratch set in one place and taken in another
  is cleared at each statement's start (`scoped::reset_statement_scratch`), and
  `Session::recover_from_panic` (which the server calls) clears the session's running stacks.
  Test: `session_state::panic_leaves_no_state` panics just after each of 22 points is set
  (`scoped::panic_at_for_test`) and checks nothing is left set and the next statements run as in
  a session where the statement never ran (it failed at all 22 before).
- **Wrong error: a dropped (or rolled-back) cast still counted.** The made casts (and made types
  in built-in operator families) the binder reads through thread-locals were the last
  `Binder::new`'s on the thread: CREATE AGGREGATE after DROP CAST still found `sf(t2, int)` for
  an `int`-state `t1` aggregate ("requires run-time type coercion"; PostgreSQL: "function
  sf(t1, integer) does not exist"). Each statement and DDL command now notes its own catalog's.
  Golden: a13_user_casts_after_drop. (`partition::PRUNING`, the last binder's
  enable_partition_pruning, only decides which partitions run-time pruning skips, never rows:
  left as it is.)

- **Flaky golden: subscription statistics' reset time leaked between engines.**
  `pg_stat_reset_subscription_stats` kept its times in a process-wide map by subscription oid;
  `golden` and `golden_wire` (two engines in one process, handing out the same oids) ran
  subscription_ddl at the same time and one saw the other's reset (`stats_reset is null`
  false). Keyed by the cluster (its oid counter) now. Test:
  `session_state::subscription_stats_reset_per_cluster`.

Open, found on the way:

- `SET search_path = s, pg_catalog` with `CREATE OPERATOR s.= (int, int)`: `SELECT 1 = 2` errors
  "operator is not unique: integer = integer" (PostgreSQL takes the first schema's, `t`). Not
  state, for the long tail.
- The planner keys `lifted` / `pw_joins` by plan `Arc` addresses within a statement; a dropped
  plan's address can come back for another during the same planning (agents 8 / 12's areas).
  `lifted`: fixed by agent 8 (round 14 section); `pw_joins` still open.
- `catalog::SESSION_TEMP_SCHEMA` is set by `execute_with`: an embedder's Parse / Describe on a
  thread that ran another session, before this one executed anything, resolved `pg_temp` to
  that session's schema. Fixed (agent 11): `parse`, `parse_message` and `describe` set it too.
- Process-wide caches behind a `std::sync::Mutex` that `unwrap` its lock (`static_rows`,
  text search dictionaries) would panic in every session after a panic while one held it
  (poisoned); their critical sections only look up and insert. Fixed (agent 11): both take a
  poisoned lock's map as it is. Test: tests/poisoned_caches.rs (also the `pg_temp` case).

## Round 15, agent 9 (full-run ALTER TABLE "stalls", harness timeouts, row locks at the level's top)

A full run of the schedule (`PGREGRESS_TIMEOUT=300`, now per statement) against 308f2d1 +
8edc0c7 on 1983037 (plus the semi join / whole-row LockRows marks below) measured 50773: every
file's count as the table below had it then (50938, before agent 5's third batch) except
tablespace (101/261) and stats (477/482), whose units all failed on
the reference: another run's cleanup had dropped the shared cluster's `regress_tblspace` midway
(fixed in the harness, see below). No engine restart, no timeout: alter_table ran in 431 s (906 s
and a timeout before), fast_default in 67 s (425 s), foreign_key in 150 s (543 s).

The stalls (alter_table 830, fast_default 177–179) were no hang and no lock wait. Sampled with
gdb in the full-run state: every ALTER TABLE called `column_using` (does a column or an index
expression anywhere hold the table's row type?), which made a binder, a copy of the whole
catalog, for every table of the database, to bind its indexes: O(tables × catalog) per
statement. By schedule group 22 the database holds about 400 tables and 1300 relations, so
each ALTER TABLE took 2–14 s, and alter_table's few hundred ALTERs ran past the per-file psql
timeout, which happened to fall on `ALTER TABLE tt7 DROP q` (the same unit in both runs: the
work is deterministic). Subset runs have a small catalog (alter_table 84 s). Fixed: one binder,
only for tables with an expression index. Also: a stored text mentions a renamed or altered
relation only as a whole word (`ALTER TABLE t` rebound every view, rule, policy and function
body containing the letter t), and DROP's dependency graph looks user types up in a set
(`user_type_name` searched every type and relation for each column). Still linear per DDL
statement, not quadratic: `Ddl::save` serializes the whole catalog, DROP rebuilds the
dependency graph. Test: `tests/ddl_scaling.rs` counts the catalog copies DDL statements make
(`catalog::catalog_copies`, a thread-local tally in `Catalog`'s Clone) with 3 and with 60 other
tables; ALTER TABLE went from 12 copies to 126 before the fix.

Harness (`tests/pg_regress.rs`, HANDOFF.md "How to test"):

- `PGREGRESS_TIMEOUT` (default 300 s) is how long one statement may run with psql printing
  nothing; `PGREGRESS_FILE_TIMEOUT` (default 3600 s) caps a whole file. A loaded machine's 400 s
  `join` no longer loses its last units.
- test_setup 7: a reference `CREATE TABLESPACE regress_tblspace` failing because another run
  made it is compared as on a fresh cluster; and a run ending while another run's database is
  on the reference leaves the tablespaces and roles that appeared during it.

SELECT ... FOR UPDATE / SHARE (`sql/paths/rowmarks.rs`, `exec/nodes.rs` LockRowsNode): the
level's scans used to lock the rows they read, so a join or a later qual's rejects were locked
too, and `ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED` locked every row the sort read (a queue's
second worker got nothing). Now every scan of a locking level returns its row's key, one
LockRows under the LIMIT (over the sort) locks the level's rows (the named tables; the others
are reference marks), and a row a concurrent update changed is rechecked against the whole
level (EvalPlanQual: the level's plan run again, each scan returning its test row). IN / EXISTS
under FOR UPDATE are pulled up into semi / anti joins as Postgres does. EXPLAIN: one LockRows,
the marks' ctids after the target list (locked relations first, then by range table), the scans'
reltargets, a semi join's inner ctid, functions' and VALUES' whole rows (`g.*`), an inheritance
tree's tableoid. Not done: a FROM subquery's whole-row mark under a locking level (`s.*`, PG
keeps its Subquery Scan). No regress unit changed (the suite's FOR UPDATE EXPLAINs already
matched). Tests: `tests/wire.rs` `row_locks_at_the_query_level_top` (two sessions: SKIP LOCKED
queue, NOWAIT over a join, the recheck after a concurrent update), `tests/sql/a9_lockrows.sql`.

Rule-rewritten RETURNING (returning 103 / 105, still differing): EXPLAIN shows the rule action's
ModifyTable (no CTE Scan on `*RULE*`) with the statement's RETURNING over its old / new rows
(`old.f1`, `new.f1`). Left: which relation takes the plain name (Postgres names the statement's
own relation first: the action's target is `foo_1`), the target's restriction derived from the
equivalence class with the statement's rows (`f1 = 4` on both sides: a Nested Loop over a
Materialize), the non-target rows' ctids in the join's target list. The restriction is a
general equivalence-class gap, not the rules': a restriction already pushed into a scan (a
pulled-up subquery's `WHERE f1 = 4`) joins no class with the join clause above it, so
`SELECT * FROM t JOIN (SELECT * FROM t WHERE f1 = 4) s ON t.f1 = s.f1` gets no `t.f1 = 4`
(a Hash Join; Postgres a Nested Loop of two filtered scans): `binder/optimize.rs`
`ec_constants` only sees the quals being pushed down with the join clause.
(`ROLL_DUMP_PLAN` now prints a WITH's CTEs and a data-modifying node's source too.)

## Fixed in round 14, agent 8 (PlaceHolderVars at joins, catalog index paths, sublink pull-ups)

Measured in a 52-file subset (`test_setup`, create_misc, create_index, create_view, create_table,
create_function_sql, select, subselect, join, aggregates, union, select_distinct,
select_distinct_on, inherit, groupingsets, partition_join, partition_prune,
partition_aggregate, partition_info, rowsecurity, updatable_views, with, equivclass, memoize,
rowtypes, select_parallel, rules, merge, returning, insert_conflict, incremental_sort, collate,
window, limit, tsrf, explain, stats_ext, join_hash, rangefuncs, generated_virtual, tuplesort,
update, privileges, foreign_key, hash_index, sqljson_jsontable, psql, misc_functions, sysviews,
alter_table, indexing, stats) against the engine of 53dd5c6 plus this batch: 9 units fixed, none
broken (join compared unit by unit with another agent's run of that engine; alter_table 953,
`to_json(v1)` of a view's whole row shown as `bt.*`, differs in that run too: the whole-row Var
display, not this batch).

| file | fixed units |
|---|---|
| join | 328, 343, 388; second batch: 347 |
| subselect | 218, 221, 288, 350; third batch: 327; fourth: 328, 332 |
| memoize | 11 |
| merge | 462 |

What changed:

- PlaceHolderVars evaluated at joins. A subquery over a join pulled up under an outer join (as
  written: pull-up comes before reduce_outer_joins) keeps each output that wouldn't go NULL with
  its rows (a constant, a non-strict expression) as a PlaceHolderVar: the lifted join under a
  projection computing them (`plan::phv_projection`, `JoinMeta::phv_join`; a single-table
  subquery's under its scan, `ScanPlan::phv_proj`). The join group takes the projection's
  relations in (`outerjoins::walk`, `gather`), each PlaceHolderVar a column of the group's row
  with its ph_eval_at (the relations its expression reads, those an outer join inside nulls
  joined first: its varnullingrels; else the subquery's), computed by the lowest relation of
  them all (`Search::with_phvs`: the join's or the scan's target list), read as a column above
  and nulled with the rows. Clauses over it need those relations; at the join computing it, or
  inside a parameterized scan (passed as a parameter when the outer side computes it), its
  expression is in place, as one clause (not split nor simplified). make_outerjoininfo's
  min_righthand covers a right side's PlaceHolderVars, have_join_order_restriction /
  has_join_restriction their relations, have_dangerous_phv refuses a nested loop whose outer
  side has part of a parameter's relations. A subquery's own subqueries are pulled up into it
  first (their PlaceHolderVars for its outer joins), its outputs in the outer join's own ON
  clause are the expressions themselves (no varnullingrels there), and constants folded after
  (`1 = 1` gone, `5 + 0` is `5`). join 328 (`Filter: (nt2.b1 AND (id IS NOT NULL))` in the
  parameterized index scan), 343 (`unique2 = (11)`: the PlaceHolderVar computed at the
  `onerow` join, a parameter), 388.
- reduce_outer_joins: a PlaceHolderVar whose syntactic scope is one relation (a constant
  output of a single-table, VALUES or FROM-less subquery) behaves like its Var
  (find_nonnullable_rels); a scope with a join remove_useless_joins took out doesn't.
- A WHERE clause over a tree whose outer joins were all reduced (a pulled-up subquery's) joins
  the group as any clause (it was left a Join Filter above, `Index Only Scan` claiming columns
  no index has); one over one item restricts it. add_filter keeps a join's written kind when it
  rebuilds it. A hash key that can't be rebuilt as a clause leaves the tree out of the group
  (`=` operators remembered through projections too) instead of dropping the clause.
- Parameterized index scans: a comparison other than an equality parameterizes only over an
  index's column (match_join_clauses_to_index); a column's join clauses come before its
  restriction clauses in the Index Cond (get_join_index_paths).
- A LATERAL scan's clause over its own PlaceHolderVar is a restriction there: before the join
  clauses in its filter, the join clauses' values the first Memoize cache keys (memoize 11); a
  PlaceHolderVar over LATERAL values under the scan's projection is still a parameter of its
  own (memoize 9).
- System catalogs in a join group: their stand-in tables' parameterized index paths (subselect
  288: `Index Cond: ((attrelid = c.oid) AND (attnum = 1))`), the catalog's scan as before
  otherwise.
- `IN` over a sub-select whose select list returns a set: a semi join with it as a subquery
  (`"ANY_subquery"`, named by its columns: `Subquery::names`), its ProjectSet costed by its
  calls' rows (create_set_projection_path) and returning the calls' values first, then the
  columns (split_pathtarget_at_srfs) (218). A clause reading a LATERAL value stays out of a
  subquery level (qual_is_pushdown_safe) and is that Subquery Scan's own filter, not a One-Time
  Filter (221). `x IN (VALUES ..., (correlated sub-select))` is a LATERAL semi join (350).
- An empty relation that is a table (a partitioned table without partitions, or read ONLY; a
  scan whose quals are false) keeps its unique indexes for Inner Unique (merge 462).
- Round 16 audit item: the planner's `lifted` list (lifted groups' joins, not lifted again;
  projections computing lifted outputs, for `lifted_into_aggs`) was keyed by plan addresses a
  dropped plan's could be reused for within one planning. A lifted join is now marked in its
  `JoinMeta::lifted_by` with the planning's process-unique `Planner::id`; a lifted projection
  is noted by a `Weak` of its input (the allocation stays, so its address can't be another
  plan's) and asked about before `lifted_into_aggs` makes inputs mutable. Checked against the
  old keys on every lookup through the whole `cargo test` and the 30-file subset (join,
  subselect, aggregates, partition_*, memoize, window, groupingsets, ...): no lookup differed,
  no unit changed. No reuse case changing a result was found: a wrongly skipped lift plans the
  group unlifted (same rows), and a wrongly moved expression goes into the aggregates' arguments
  over the same row (only a volatile one read twice could differ).
- Second batch (measured in a 35-file subset: test_setup, create_*, select, inherit,
  updatable_views, select_distinct, subselect, union, join, aggregates, rowsecurity,
  groupingsets, join_hash, merge, sysviews, tsrf, incremental_sort, rules, psql,
  select_parallel, window, equivclass, limit, rangefuncs, returning, with, partition_join,
  partition_prune, partition_aggregate, memoize, insert_conflict; every file's count as this
  table had it or better, join, subselect, partition_join, memoize, merge and equivclass unit by
  unit): remove_useless_result_rtes over pulled-up one-row VALUES (join 347). A one-row VALUES
  under an outer join that nulls it is a FROM-less relation computing its values as
  PlaceHolderVars; projections over one compose; a left (or full, as written) join of a
  FROM-less relation on a clause folded to true (`1 = 1` of two such relations' values) is its
  left side; an outer join reduced to inner with a FROM-less side computing PlaceHolderVars is
  its other side with them computed over it (remove_result_refs), its clauses with the values
  plain clauses (hash keys over a constant turned back first), and that merged side's own
  outputs aren't inlined into the join above's clause as a subquery's would be.
- Third batch (the same 35-file subset, unit by unit against a run of the base: only subselect
  327 changed): convert_ANY_sublink_to_join for a correlated aggregate sub-select (subselect
  327). `x IN (SELECT agg(...) ... WHERE c = outer)` whose sub-select planning already made
  an aggregation, or its MIN / MAX InitPlan under a Result, is a LATERAL subquery
  (`"ANY_subquery"`, a Barrier) the join reads: an inner join when it returns one row at most
  (no GROUP BY: reduce_unique_semijoins), else a semi join. It broke ("outer reference without
  an outer row") because the lifting dropped the join's LATERAL values when the right side's
  only outer reference was inside a MIN / MAX InitPlan: a sub-select without parameters of its
  own reads its level's outer values as its own `Outer(1, …)`, which `reads_outer` didn't look
  into, so the join became a plain inner join and the search put the subquery on the outer
  side. A LATERAL subquery's Memoize keys are the values of the join clauses pushed into it
  first, then its own references (paraminfo_get_equal_hashops).
- Fourth batch (a 53-file subset: the 35 above plus collate, collate.icu.utf8, privileges,
  foreign_key, stats_ext, misc_functions, alter_table, indexing, tuplesort, generated_virtual,
  sqljson_jsontable, rowtypes, explain, update, hash_index, stats, select_distinct_on,
  partition_info; every file at its count here, alter_table 953 aside, the whole-row display
  that differs without this batch too): subselect 328 and 332.
  - 328: an inner or semi join's `name = text` clause comes out of its equivalence class with
    the comparison's collation, name's C (a non-default implicit collation beats the
    default). canonicalize_ec_expression relabels the text-family side (a column, an
    expression of columns) to it, and EXPLAIN shows that as a cast:
    `Index Cond: (stringu1 = ("*VALUES*".column1)::text)`. An outer or anti join's clause
    keeps its form, as do a constant and a sub-select's value (`name_text_relabel`, by the
    join being explained).
  - 332: a semi join's side is unique-ified on an expression of its columns
    (create_unique_path over semi_rhs_exprs that aren't Vars), sorted on it, a Unique
    keeping one row per value, the columns it reads coming out
    (`Unique -> Sort Key: (("*VALUES*".column1 * "*VALUES*".column1))`); no hashed form for
    those.
- Fifth batch (no unit; the 64-file subset unchanged): a join proven empty names the
  columns of the relations it removed, system catalogs' too, each by its relation, so its
  Result shows the target list over them (`Output: c.relname, t2.q1`,
  `((pg_class.relname)::text || 'x'::text)`) where it showed `NULL`.

Goldens added: `tests/sql/a8_phv_join.sql`, `a8_sublink_lateral.sql`, `a8_dummy_unique.sql`,
`a8_catalog_lateral.sql`, `a8_result_merge.sql`, `a8_lateral_any.sql`, `a8_ec_relabel_unique.sql`,
`a8_dummy_names.sql`.

Still open from this list, and why:

- join 366 (done in round 16, agent 19, after reduce_outer_joins on the plan before the
  walk): a semi join over a full join of RESULT relations, all removed by Postgres. Here
  the IN's sub-select (a join) stays a Barrier ("ANY_subquery") planned as a level of its own
  during the outer level's walk, after the outer join group was lifted, so the semi join's
  FROM-less right side can't be removed with it: it needs the sub-select pulled up into the
  group first. 345 (a PlaceHolderVar computed in a LATERAL subquery scan: the join order), 360
  (one computed over a parameterized appendrel), 362/363 (PlaceHolderVars with sub-selects,
  copied per place, InitPlan 2 a copy), 368 (the dummy Result's target list is
  information_schema.column_udt_usage's expressions; the engine reads that view as a virtual
  catalog, so it has no expressions to show: the fifth batch names other relations'
  columns), 744 (`x left join lateral (select a.q1 from y)` under a nested loop over `a`:
  Postgres hashes the left join with the LATERAL value as a key, a join rel parameterized by
  `a`; the search here has no parameterized join rels).
- partition_join 41: a partitionwise join with a LATERAL PlaceHolderVar.
- Found with 328: a hash join on `name = text` isn't costed here (Postgres hashes both as
  text), and an equivalence class doesn't carry a constant across `name = text`
  (`nk.s = tx.i and nk.s = 'A5'` gives tx `Filter: ((i)::text = 'A5'::name)` in Postgres).
  Both done in round 16, agent 19 (not as a merge key yet).
- A one-row VALUES under a left join that stays one (`i left join (select v.x, 5 as c from
  (values (0, 1)) v(x, y)) s on s.x = i.f1`): Postgres's join clause reads `0` itself
  (`Join Filter: (0 = i.f1)`, a Nested Loop over the Result); here `x` is a PlaceHolderVar of
  the subquery's projection, so a merge join on `(0)`. Rows match (a8_result_merge); no unit of
  the measured subset has this shape.
- groupingsets 189/191/195/197 (queued from agents 2 and 11): EXPLAIN numbering, not started
  past a probe. Postgres plans a copy of a sub-select for each GROUPING() argument, numbered
  before the group key's copy (in 18 the keys are an RTE_GROUP's expressions, preprocessed after
  the target list): 189 lists `InitPlan 1` (the argument's copy, unused) before `SubPlan 2`
  and the key's `InitPlan 3`. The engine keeps GROUPING as key positions
  (`Expr::Grouping(pos)`), so an argument has no copy of its own, and EXPLAIN numbers sub-selects
  as it meets them, child first, so the key's comes first. 191 also needs a GroupAggregate
  without keys for a pseudoconstant key and `(select grouping(v1))` as a SubPlan (the engine
  folds GROUPING to 0 without grouping sets, making it an InitPlan). 195/197 also need the
  key's SubPlan attached to the scan computing it (make_group_input_target) and the GROUP BY
  column `t1.v` in the Agg's target list. 215: three EXPLAIN target-list gaps, each general:
  the grouping-set mask column shows as `GROUPING` in a MixedAggregate's Output under a
  ProjectSet; a scan computing the group keys returns them in the target list's order (the
  `a + b` entry before the resjunk `a`); and the GROUP BY keys the select list doesn't name
  stay resjunk entries of the nodes above the aggregation (Sort, ProjectSet:
  `Output: (count(*)), q1`), which here only the level's top projection carries. 55/57: not
  started (a LATERAL subquery's sub-select output under GROUPING).

## Fixed in round 14, agent 3 (strict-NULL folding, semi joins unique-ified early, EXISTS with sublinks)

A full run of the schedule against 3b11900 plus this batch's constant folding measured 50831:
every file's count as below except three run artifacts, test_setup 7 (the shared reference
cluster's `regress_tblspace` from a concurrent run), and two `ALTER TABLE` stalls past the psql
timeout (alter_table 830, and fast_default 177–179 after `ALTER TABLE t ADD COLUMN y` stalled:
fast_default matches 275/275 in a subset run). The units below were then fixed and checked in
subset runs (the first batches' files): none broken.

| file | fixed units |
|---|---|
| subselect | 310, 322, 326, 342 |

What changed:

- eval_const_expressions' simplify_function: a strict function over a NULL constant is NULL,
  whatever its other arguments (a custom plan's NULL parameter too, so `... / $3::real = ANY
  (...)` is a constant-false qual, `One-Time Filter: false`); COALESCE drops NULL constants and
  ends at a constant, one first being the result. Not in views (deparse) or unplanned binds.
- A semi join's clauses need only their own relations, as an inner join's (deconstruct_jointree
  gives a semi join no ojscope), and join_is_legal lets the semi join's right side, unique-ified
  on all its semi join clauses' keys, join a part of its left side first (326: `a ⋈ unique(c)`,
  then `⋈ b`).
- An EXISTS whose WHERE clause has a sub-select reading the EXISTS's rows and the outer query's
  (a correlated IN) is still pulled up into a semi join: the sub-select a SubPlan of the semi
  join's clause, its parameters moved to the join's row (322); a correlated clause's volatility
  is its sub-select's plan's.
- VERBOSE row marks of locked rows: a locked scan and its LockRows return the row's ctid, a FROM
  subquery locking rows shows its Subquery Scan (its plan carries the mark as resjunk), a
  LockRows at a level's top returns the marks after the target list (310).
- MERGE into an empty partitioned table: the dummy relation is the target's, its columns
  qualified by the target's name, its row identity a ctid, last (merge 462 still lacks Inner
  Unique: the empty relation knows no unique index).

Goldens added: `tests/sql/a3_const_fold.sql`; `a3_sublink_pullup.sql` extended.

Still open in subselect: 218, 221, 288, 327, 328, 332, 350.

Follow-up (checked in a subset run of 20 files after rebasing onto agent 5's round): a LATERAL
subquery planned apart (ORDER BY, ...) takes no clause reading the left side's values: it stays
the Subquery Scan's filter (qual_is_pushdown_safe refuses lateral references), so select_parallel
132's plan shape matches except the parallel outer side (`Gather` over `Parallel Seq Scan`, a
parallel-costing difference). subselect 329 kept: a semi join's clause in a parameterized scan is
derived afresh, outer member first, not reused as written.

## Fixed in round 14, agent 5 (run-time pruning, expression merge keys, EC orientation)

Measured in subset runs against the engine of 7be2e83 rebased onto 4186dde (test_setup,
create_misc, create_index,
create_view, create_table, select, subselect, join, aggregates, union, select_distinct,
select_distinct_on, inherit, groupingsets, partition_join, partition_prune, partition_aggregate,
partition_info, rowsecurity, updatable_views, with, equivclass, memoize, rowtypes,
select_parallel, rules, merge, returning, insert_conflict, incremental_sort, collate, window,
limit, tsrf, explain, stats_ext, join_hash, rangefuncs, generated_virtual, tuplesort, update,
privileges, foreign_key, create_function_sql, hash_index, sqljson_jsontable), and a full run of
the schedule up to `truncate`: 50 units fixed, none broken (every other file of the subset
matches this table's counts).

| file | fixed units |
|---|---|
| partition_prune | 24, 203, 414, 435, 440, 446, 485, 489, 490, 524, 526, 528, 532, 534, 544, 555, 623, 626, 633, 664, 722, 725, 729, 735, 754, 756 |
| join | 77, 85, 349, 364, 379, 458, 459, 462, 513, 514, 515, 566, 591, 647, 844, 867, 921, 923 (90 → 74); third batch: 248, 420; fourth: 416 |
| partition_join | 94, 134, 158, 235, 603, 604, 605; second batch: 43, 173, 256, 258, 599, 600, 601 |
| create_index | 671 |

What changed:

- Run-time partition pruning: a timestamp key compared with timestamptz constants (stable,
  reads TimeZone) prunes as the executor starts, the constants converted to the key's type
  when that's exact an hour either side; an Append of one member is no Append (no pruning of
  it); a Merge Append shows `Subplans Removed`; every member pruned, the columns still the
  parent's (qualified, the pruned members range table entries); a UNION ALL's members are
  pruned too (an appendrel of several partitioned tables), the counts of flattened arms
  summed; a parameterized index scan's conditions prune per outer row; the plan is costed for
  every member (pruning only removes them as the executor starts).
- WHERE clauses with an uncorrelated sub-select (an InitPlan) or a parameter go into a UNION
  ALL's arms; a one-row VALUES arm is its constants (pull_up_simple_values: a gating Result);
  an arm with a WHERE or several FROM items stays a subquery (is_safe_append_member): its
  relations named after the level's own, its Append kept; a flattened arm's parent and
  sub-partitions take no name.
- `x COLLATE c` with `c` the column's own collation is the column; IN over array values is
  ORed comparisons (no array of arrays); a varchar IN with a non-constant value is the array
  coerced to text[] (an ArrayCoerceExpr, which pruning can't use, so no pruning).
- int4 and oid relabels through a subquery's output (`(x::oid)::integer`) are the key to
  pruning; a clause over a projection of an inheritance Append goes into its members.
- Grouping sets whose first rollup's keys are fixed by constants: the sorted / mixed plans only
  (consider_groupingsets_paths given sorted input): a MixedAggregate.
- GROUP BY () (or HAVING) without aggregates is degenerate grouping: a Result (gated by the
  HAVING), one per empty grouping set, the input not read.
- join strength reduction: an ANY SubPlan at the top level (and a row comparison anywhere)
  proves its left side non-null.
- Merge joins on an expression of one side and a column of the other (a FULL JOIN USING's
  merged column, `a.q2 = COALESCE(b.q1, 1)`): the expression side sorted by it (`Sort Key:
  (expr)`, the input's target list computing it under VERBOSE), not for semi / anti joins
  (their unique-ified side groups by columns here). FULL JOIN row order now Postgres's (join
  77, 85).
- Equivalence-class clauses get the orientation of their first derivation: check_index_predicates
  for each relation with a partial index (the other relations' member first), then each
  relation's index paths (its member first), in range-table order; create_join_clause reuses
  either way round.
- Nested loop join filters: an inner join's equivalence clauses first, then cheapest first
  (order_qual_clauses); `Rows Removed by Join Filter` under ANALYZE; an inner Materialize shows
  the outer rows as its loops; a nested Append / Merge Append the planner flattened shows its
  members' counters.
- A RIGHT JOIN tries its LEFT JOIN form first (equal costs keep it).
- OR join clauses parameterize an index scan as `= ANY (ARRAY[outer values])` even with no other
  candidate clause, the other movable join clauses its filter; inequality join clauses
  parameterize an appendrel's index scans.
- A semi join's clause with a column the subquery fixes to a constant fixes the outer column
  too (the clause goes); a COALESCE equated with a constant above an outer join fixes the next
  join's inner side (reconsider_outer_join_clause).
- Equivalence classes survive a grouping and a partitionwise join's Append (redundant GROUP BY
  and ORDER BY keys go, the group count estimated over the keys left).
- MIN / MAX over a partition named directly; the choice compares disabled nodes first.
- A FROM-less subquery's non-volatile function call is pulled up (its equality with a column of
  the other side a restriction of it, an equivalence class member across the join above,
  generated member first); MERGE through a view names the view's relation and lists only the
  result relations left after pruning.
- Partitionwise joins under a LIMIT: each child planned for it, the Append of those or of the
  cheapest total paths, whichever reads the LIMIT's rows cheaper.

- Second batch (on 96d4656, subset runs of the same files: 7 more fixed, none broken):
  ORDER BY … LIMIT over a partitionwise join: each child join planned in the order too, a
  Merge Append of them when it reads the rows wanted cheaper (generate_orderedappend_paths;
  `JoinMeta::presorted` marks the members); an anti join's clauses keep their written
  orientation; a hashed grouping with a Sort above is taken when that costs less than sorting
  for a GroupAggregate; a partitionwise inner join's columns carry their partitioned tables'
  statistics (group counts above it).

- Third batch (on 767acf3, subset runs of 55 files: 2 more fixed, none broken), a wrong
  result first: a join's hash keys are rebuilt as clauses with the `=` remembered for their
  type, and the types were collected through joins only, so a lower join under a filter
  (`COALESCE(b.x, a.x) = 44` above two left joins, its upper join's key gone into constants)
  lost its clause when no other join had the type, and its rows were a cross product (it
  depended on the session's earlier queries: the operators are per thread). The types are
  collected through every node, and a key without one resolves `=` itself rather than going.
  Then: a join's sorted output is useful for merging when a clause between it and the rest
  is on that class, an outer join's included (pathkeys_useful_for_merging: join 248's Merge
  Right Join over a presorted inner join); an expression's borrowed class isn't a left join's
  output order (its null-extended rows); an EXISTS pulled up over a subquery planned with
  merge-input sorts sees through them; a parameterized bitmap heap scan is costed for its
  repeated scans (compute_bitmap_pages' loop_count, the index part amortized too, and EXPLAIN
  costs it the same way: join 420).

- Fourth batch (subset runs of 26 files: join 416 fixed, none broken): a WHERE's equalities
  left above a left join, read in its nullable side, make one equivalence class with the
  WHERE's other equalities (process_equivalence, in their order), and its clauses are the ones
  Postgres derives at the join (generate_join_implied_equalities_normal): the members only the
  join computes equated in turn, the last with the first outer side member, a written clause
  over the same two members reused as written; after the other quals. Only classes of three
  or more members of one btree family without constants, and not when a qual is strict for
  the nullable side (the join is reduced to an inner one first in Postgres).

Goldens added: `tests/sql/a5_runtime_pruning.sql`, `a5_join_misc.sql`, `a5_lifted_merge.sql`,
`a5_pw_order.sql`, `a5_merge_useful.sql`, `a5_oj_classes.sql`.

Still open from this list, and why:

- partition_prune 392 / 393 / 399: a UNION ALL FROM subquery's alias is a range table entry
  named first (`ab`, so its leaves are `ab_1`…); the plan doesn't keep the alias of a pulled-up
  UNION ALL. 392 / 393 also show the InitPlan at the top Append (the arm subquery's quals read
  the outer level's). 265 (a SubPlan copied into each index condition), 410 (an index entry
  for a dead tuple: `Heap Blocks: exact=1` with no row), 746 (window functions over
  partitions: ordered partition scans under each arm's WindowAgg), 377–383 / 652 (parallel).
- join 609 / 610: the equivalence class has the scan-level `t1.a = t1.b` too, so Postgres
  joins on `t1.a = t2.b`; the engine's classes come from join clauses only.
- join 442: `(t1 LEFT JOIN t3 ON t3.f1 = 'foo') LEFT JOIN t4 ON t3.f1 = t4.f1` gives t4
  `f1 = 'foo'` and no join clause when t3 ⋈ t4 is joined first (the clause's version without
  the first join's nulling is in t3's class with the constant); the binder only sees constants
  pushed with the clause, and dropping the clause is right for that join order only (also a
  Memoize and a LATERAL filter there). join 502 / 503: the subquery's sorted path (a Group
  over an index scan) kept beside its cheapest one for a merge join above; subqueries are
  planned once here. An inner join's class clause in a bitmap scan's Recheck Cond is derived
  afresh, outer member first (`a.unique1 = thousand`); the engine shows it as written.
- join 374: `t1.a IS NOT NULL` (t1.a NOT NULL, on the preserved side) pushed into a LATERAL
  UNION ALL arm is always true there (restriction_is_always_true over the outer Var); the
  arms take the WHERE in the planner (push_into_union_all), which doesn't know which of the
  join's params come from such columns. (Dropping the test where the binder pushes it, a
  LATERAL subquery without UNION ALL, was tried and works; the arms need the params' nullability
  carried to the planner's leaf planning.)

## Fixed in round 14, agent 6 (dummy-rel classes, parallel plans, incremental sort stats)

Measured in subset runs against the engine of 7be2e83 (rebased onto 3b11900) (test_setup, create_table, create_misc,
create_index, create_view, inherit, select, union, join, aggregates, groupingsets, equivclass,
subselect, partition_join, partition_prune, partition_aggregate, incremental_sort,
select_parallel, select_distinct, limit, memoize, with, rowsecurity, updatable_views, rules,
window, sqljson, tsrf, join_hash, write_parallel, stats_ext): 15 units fixed, none broken; a second
batch (on 4186dde) fixed 2 more in the same subset, none broken.

| file | fixed units |
|---|---|
| incremental_sort | 26, 27, 44, 45 |
| select_parallel | 35, 75, 116, 138, 170, 203; second batch: 167, 217 |
| join_hash | third batch: 123 |
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
- Second batch: a partial plan already in a leading part of the ORDER BY's order is sorted
  incrementally under the Gather Merge (select_parallel 167); a Subquery Scan whose filter
  workers can't run gathers the subquery's own plan below it (217).
- Third batch: a Gather (Merge) of partial groupings returns their rows times the parallel
  divisor (compute_gather_rows), not times the workers: join_hash 123's Finalize Aggregate.

Goldens added: `tests/sql/a6_dummy_rel_keys.sql`, `a6_parallel_plans.sql`,
`a6_incremental_sort_analyze.sql`.

Still open in these files, and why:

- incremental_sort 87, 89, 99–107 and select_parallel 191, 205, 207, 208: fixed, see "Fixed
  in round 14, agent 7".
- select_parallel 132 (a LATERAL subquery with ORDER BY pulled up: it stays a Subquery Scan in
  Postgres). Also seen while probing: a security-barrier view read without quals loses its
  Subquery Scan here, and its unread outputs aren't NULLs in the scan's target list.
- Cosmetic, not failing a unit: a sort's input target doesn't count computed resjunk sort keys
  in its width; a partitionwise grouping's serial members show the width before their
  projection. (A Gather over a relation's partial plan returns the relation's rows now, agent
  7, except over an Append.)

## Fixed in round 14, agent 7 (several partial paths per relation)

Measured in subset runs (test_setup, create_table, create_misc, create_index, create_view,
inherit, select, union, join, aggregates, groupingsets, equivclass, subselect, partition_join,
partition_prune, partition_aggregate, incremental_sort, select_parallel, select_distinct, limit,
memoize, with, rowsecurity, updatable_views, rules, window, sqljson, tsrf, join_hash,
write_parallel, stats_ext) against this batch's engine, rebased onto ce4f7ee: 15 units fixed,
none broken (test_setup 7 differs in some runs: the shared reference cluster's tablespace).

| file | fixed units |
|---|---|
| incremental_sort | 87, 89, 103, 104, 105 |
| select_parallel | 191, 205, 207, 208 |
| memoize | 67 |
| partition_prune | 377, 378, 380, 381, 383 |
| aggregates | second batch: 342, 505, 507 |
| incremental_sort | third batch: 99, 100, 101, 102, 106, 107 |
| partition_prune | fourth batch: 652 |

What changed:

- A scan keeps its relation's other partial paths beside the cheapest one (`ExNode::partials`,
  pruned as add_partial_path does: compatible pathkeys, fewer disabled nodes, then total cost
  with the 1% fuzz): its parallel index scans, built for the orderings the planner found useful
  (`ScanPlan::useful`, now also set for a grouping's keys, hinted_scan, and for a join leaf's
  ORDER BY and merge columns, `leaf_want`). They go through projections of the scan only.
- generate_useful_gather_paths (`useful_gathers`): for an order, a Gather Merge over each partial
  path already in it, over the cheapest one sorted, and over each other one in a leading part
  of it sorted incrementally, returning the relation's rows. Used by an ORDER BY's sort, by a
  complete grouping in order (incremental_sort 87: a GroupAggregate over a Gather Merge of an
  Incremental Sort of a Parallel Index Scan) and by a UNION's arms merged in the columns' order
  (89: a Merge Append of Gather Merges, the ORDER BY then sorting incrementally). A partial
  grouping also groups each partial path already in its keys' order (a Partial GroupAggregate
  straight over a Parallel Index Scan).
- add_path keeps the path cheapest to start beside the cheapest in total: a node's `alt` is a
  chain now (a sorted grouping's alternatives, the serial grouping in order beside a parallel
  one), the Sort of an ORDER BY sorts a partly ordered alternative incrementally, and a LIMIT
  weighs every one of them, plus create_ordered_paths' own Gather Merge whose workers' sort is
  costed for the rows the LIMIT reads (kept only when that is fuzzily cheaper).
- A scan reading an index in the order the ORDER BY wants (the planner dropped the Sort) is
  gathered only by a Gather Merge in that order (`wanted_order`); a plain Gather would lose it.
- A join's partial paths of every method and orientation (`join_force`): hashing (a Parallel
  Hash too) and nested loops with either side outside for an inner join, beside the serial
  join's own, compared as add_partial_path does (total cost only; a tie keeps the one built
  first: the side earlier in the FROM list outside, nested loops before hashing).
  incremental_sort 103: a Parallel Hash Join where the serial join merges. A partial nested
  loop takes the inner side materialized when cheaper and an outer partial path in order when
  fuzzily as cheap; a Function Scan of parallel-safe calls is parallel safe (104/105: a partial
  nested loop of a Parallel Index Only Scan and a Function Scan under a Gather); a
  parameterized inner side a worker can run (its quals read only the nested loop's outer row)
  makes a partial nested loop too (memoize 67: a Memoize over a parameterized index scan;
  partition_prune 377–383: a parameterized Append of index scans under a Gather).
- A base relation whose Gather beats its serial path leaves the joins above only that path
  (unless the serial join is clearly cheaper, or merges, or reads an input in the wanted
  order); a partial nested loop never takes a gathered inner side (select_parallel 208).
- A Gather over a relation's (non-Append) partial plan returns the relation's rows
  (create_gather_path); its target list is its child's exact one (so is a Materialize's or a
  Hash's over it); a scan or Function Scan of no column read is 0 wide.
- UNION ALL arms with quals stay query levels of their own: an arm's InitPlans (those no other
  arm reads: a qual pushed into every arm stays the parent's) go on its top node, which pays for
  them, and the arm isn't parallel safe (select_parallel 205: a Gather per arm).
- make_sort_input_target under a ProjectSet: the sort's input is the non-postponed columns,
  then those the postponed ones read, a Result over the sort (under a Gather Merge) restoring the
  ProjectSet's input order; the top projection dropping the resjunk sort columns isn't a node
  (207).
- Incremental sorts and groupings estimate groups with the relation's rows (estimate_num_groups'
  rel->rows), not the input's; a semi join's side unique-ified on columns without statistics
  counts get_variable_numdistinct's default for each (the Hash Right Semi Join wins, 191); a
  WindowAgg returns only what's read above it.

- Second batch: an aggregate of an internal state and an anonymous record argument isn't
  split into partial aggregation (agg_args_support_sendreceive: record_recv needs the typmod;
  aggregates 342: `array_agg(s)` of a FROM subquery's whole row); a UNION ALL arm of one
  table without quals of its own is an appendrel member (is_safe_append_member): its scan
  returns only the columns read above, no physical target list (505/507); an arm's
  parallel-safe InitPlans (no Gather in them) leave it parallel safe, its partial plan
  carrying them too (SS_charge_for_initplans), so a UNION ALL of such arms is one Parallel
  Append of the arms' Parallel Appends (partition_prune 652's shape).

- Third batch (incremental_sort 99–102, 106, 107): a FROM subquery whose
  select list over a function scan only reads the other side (`LATERAL (SELECT
  tenk1.unique1 FROM generate_series(...))`) is pulled up (`lift.rs`): the function scan is
  the leaf, its outputs plain Vars of the other relation, and the join isn't LATERAL any
  more. A nested loop's outer side then can be a Gather Merge of a partial path sorted in the
  order the node above wants (generate_useful_gather_paths with relation_can_be_sorted_early:
  the Sort's or a DISTINCT's keys, `order_hint`, expression keys computed at the relation, none
  volatile), kept beside the cheapest join as its ordered alternative; an ORDER BY sorts it no
  further and a DISTINCT (now also planned through the grouped join like a grouping) takes
  it with a bare Unique when cheaper. A text key computed from a `name` column sorts under
  `COLLATE "C"` (the name's collation wins over the default) and shows it. Measured in the
  subset above plus returning, merge, insert_conflict and rangefuncs, rebased onto 27da13f:
  6 units fixed, none broken (rangefuncs 422 too, already fixed upstream by then).
- Fourth batch (partition_prune 652): a UNION ALL arm kept a subquery charges its InitPlans
  to its partial plan too (SS_charge_for_initplans: every path of the level), so it starts
  later. A Parallel Append's members costing the same keep their relids' order
  (append_startup_cost_compare / append_total_cost_compare's bms_compare, `ExNode::rank`): a
  query level's own UNION ALL takes its leftmost arm's entry as the appendrel's parent, the
  arm a copy after the others (flatten_simple_union_all), and arms pulled up bring their
  relations after all the arms' entries. setrefs names the subquery arms' relations as it
  reaches them, in the final plan's order: when a Parallel Append reorders the arms, EXPLAIN
  names them again in its order (`arm_orders`, the build run again as for partitionwise
  groupings). Measured in the same subset on 23e447c: 1 unit fixed, none broken.
  Seen while probing, not in the suite: a UNION ALL of three such arms stays one Append of
  them all in Postgres (the engine nests the first two); a one-table arm without quals
  pulled up beside them names its parent and partitions first.

Goldens added: `tests/sql/a7_partial_paths.sql`, `a7_partial_joins.sql`; second batch:
`a7_parallel_aggs.sql`; third batch: `a7_lateral_gather.sql`; fourth batch:
`a7_union_arms.sql`.

Still open in these files, and why:

- select_parallel 132 (a LATERAL subquery with ORDER BY).
- Cosmetic: a partial nested loop's Memoize keeps the serial path's rescan costs (Postgres
  costs it for the partial outer rows: memoize 67's costs); a Gather over an Append keeps the
  partial rows times the divisor.

## Fixed in round 14, agent 4, third batch (joins under windows, Memoize over appendrels, filters over outer joins)

Measured against the engine of 1a2f31a in a 36-file subset (`test_setup`, create_misc,
create_table, create_index, create_view, create_function_sql, select, inherit, insert_conflict,
updatable_views, select_distinct, subselect, union, join, aggregates, hash_index, gist, collate,
rowsecurity, groupingsets, join_hash, tsrf, rules, collate.icu.utf8, generated_virtual, window,
equivclass, sqljson_jsontable, limit, rangefuncs, rowtypes, with, partition_join, tuplesort,
partition_info, memoize): 6 units fixed, none broken.

| file | fixed units |
|---|---|
| window | 392, 394, 395 |
| generated_virtual | 450, 452 |
| memoize | 52 |

What changed:

- A join under a window is planned for the window's order (its PARTITION BY and ORDER BY
  keys are the query's pathkeys then), its presorted paths (an index-ordered nested loop, a
  merge join over an index) competing with sorting the cheapest; under a LIMIT, for the rows
  read before the window's first output when its frame doesn't need them all (a frame ending
  `UNBOUNDED FOLLOWING` or `n FOLLOWING`: the cheapest in total).
- A Memoize may cache an append relation's parameterized members (a UNION ALL's index scans
  by the outer value), as it does a parameterized scan.
- A filter left over an outer join (a WHERE clause over the nullable side, `COALESCE(t2.b, 1)
  = 2`) is part of the join's restriction list in Postgres: its cost per row now counts in the
  join's nested loop, hash and merge costs (the Hash Left Join wins over the Nested Loop then).

Goldens added: `tests/sql/ga4_plan_tail4.sql`.

## Fixed in round 14, agent 4, second batch (whole-row Vars, catalog join stats, resjunk keys)

Measured in a full run of the whole schedule (one database, `PGREGRESS_TIMEOUT=600`) against
the engine of 96d4656 plus this batch: 50877 units, every file as this table's counts except
collate +2, limit +1, rowtypes +1, partition_info +2 (this batch's), join +1 (its row-order
unit 433, which matches in full runs), and that run's artifacts: alter_table 1744 (the known
`ALTER TABLE tt7 DROP q` stall, two 600 s timeouts), test_setup 78 (the shared reference
cluster's `regress_tblspace`) and subselect 344 (its unit 329, fixed in 767acf3, wasn't in
that engine).

| file | fixed units |
|---|---|
| collate | 146, 147 |
| limit | 66 |
| rowtypes | 78 |
| partition_info | 40, 45 (row order: plan choice) |

What changed:

- EXPLAIN shows a relation's whole row (a `ROW()` of all its columns of its named row type) as
  the whole-row Var `rel.*`, which ruleutils always qualifies, null-extended above an outer
  join too (`Filter: (t2.* IS NOT NULL)`); a VALUES list's or a subquery's whole row stays a
  `ROW()` (expanded when pulled up).
- An IN list over a whole-row reference of constants of its row type is one `= ANY (array)`
  (`<> ALL` for NOT IN), as transformAExprIn builds it; results unchanged.
- A byte-ordered `COLLATE` ("C", "POSIX") on a system catalog's LIKE / regex pattern still
  uses the catalog's index (match_pattern_prefix).
- A grouping read in its ORDER BY's order under a LIMIT keeps the resjunk sort key in its
  target list (`GroupAggregate Output: ..., thousand`).
- A system catalog's columns in a join group take its stand-in table's statistics: a unique
  index's column is unique (get_variable_numdistinct), so `pg_partition_tree(...) JOIN
  pg_class` is estimated and joined (Hash Join, rows in the function's order) as Postgres does.

Goldens added: `tests/sql/ga4_plan_tail3.sql`.

## Fixed in round 14, agent 4 (memoized SubPlan params, parameterized-path clauses, dummy rels, small EXPLAIN gaps)

Measured against the engine of commit 7be2e83 in a 45-file subset (`test_setup`, create_misc,
create_table, create_index, create_view, create_function_sql, select, inherit, insert_conflict,
updatable_views, select_distinct, select_distinct_on, subselect, union, join, aggregates,
hash_index, gist, privileges, collate, rowsecurity, groupingsets, join_hash, tsrf, rules,
collate.icu.utf8, generated_virtual, incremental_sort, stats_ext, select_parallel, foreign_key,
window, equivclass, sqljson_jsontable, limit, rangefuncs, rowtypes, returning, with,
partition_join, partition_prune, tuplesort, partition_aggregate, partition_info, explain,
memoize): 25 units fixed, none broken (test_setup 7 differed in one run only: the shared
reference cluster had `regress_tblspace` from a concurrent run); re-checked after rebasing onto
3b11900.

| file | fixed units |
|---|---|
| memoize | 19, 23, 30, 55, **56 (wrong results)** |
| rowsecurity | 500, 503, 548, 556 |
| collate | 114, 115 |
| equivclass | 69, 82 |
| generated_virtual | 454, 462, 463 |
| gist | 47, 51 |
| rowtypes | 224 |
| rules | 544 |
| create_function_sql | 130 |
| create_view | 271 |
| hash_index | 30 |
| select | 64 |
| tsrf | 12 |

What changed:

- **Wrong results**: a correlated SubPlan whose join had a parameterized inner scan (a
  Memoize's, say) read the SubPlan's own outer value (`t0.ten` in memoize 56) as the nested
  loop's parameter: the inner scan runs with the outer row as a frame of its own, so its quals'
  `Outer(d, …)` are `Outer(d + 1, …)` there now (`Rel::shallow_param`, `deepen_outer`; no
  parameterized path when a sub-select in the quals reads an enclosing level itself).
- get_baserel_parampathinfo: a parameterized scan enforces every join clause movable into it,
  equivalence-class ones too (one per class), not only its index's: `Index Cond: (x =
  (t1.t)::numeric)` with `Filter: (t1.x = (t)::numeric)`; the parameters (a Memoize's cache
  keys) follow ppi_clauses' order (movable join clauses, then the classes').
- Memoize: Cache Mode binary keys compare bit for bit (-0.0 and 0.0 two entries); the cache
  is bounded by hash_mem and evicts its least recently used entries (Evictions, peak Memory
  Usage).
- check_index_predicates: quals a partial index's predicate implies stay for a target relation
  (UPDATE / DELETE / FOR UPDATE: EvalPlanQual rechecks them) and as a hash index's conditions
  (no amoptionalkey).
- EXPLAIN: an index-only scan's expression key columns are INDEX_VARs, parenthesized in its
  Index Cond and Order By; an ordered scan at a level's top returns its resjunk ORDER BY keys
  (`Output: a, b` for `ORDER BY b`, the KNN distance for a GiST ORDER BY); a column of a
  collation other than its type's default shows it in a Sort Key (`x COLLATE "C"`); a column a
  stored view reads that has been dropped since shows by the view's name for it (`t.f3`, a
  `raise_error` with a fourth, shown argument); information_schema's helper functions are
  qualified when it isn't on the search path; a rule's INSERT ... ON CONFLICT DO UPDATE counts
  its EXCLUDED range table entry too (the Conflict Filter's `excluded.hat_color`,
  `hat_data.* <> excluded.*`).
- Dummy relations: a subquery whose plan is proven empty (a set-returning select list and an
  OFFSET 0 over an empty relation too) is a dummy rel and empties an inner join above it; a
  contradiction (`id = 1 AND id = 2`) or refuted CHECK leaves a scan's dummy Result with its
  columns named; a dummy Result's columns show bare unless the statement scans nothing
  (set_rtable_names), and its Vars before the PlaceHolderVars it evaluates.
- remove_unused_subquery_outputs on a subquery level's select list: unread plain columns are
  NULLs too (not sort or group keys), and such a NULL is a constant above, not a reference
  (`Output: a, NULL::integer` through a Limit and Sort); a Hash over a Subquery Scan returns
  what the scan does; a grouped or DISTINCT subquery is unique on its keys for a join (Inner
  Unique), not under a nondeterministic collation.
- Equivalence classes: a sort key over a domain without constraints (a CoerceToDomain the
  planner simplifies to a RelabelType) is its column's (equivclass 82); a made type's `=` in no
  btree family forms no class; under row-level security a non-leakproof `col = const` is no
  class member (a made type's operator over a built-in's implementation counts as its own,
  non-leakproof function).
- Costs: an uncorrelated hashable `= ANY` sub-select costs its comparisons per row (cost_subplan
  with useHashTable), so a hashed NOT IN goes ahead of costlier quals (rowsecurity);
  constraint exclusion compares equal expressions (a virtual generated column's, generated_virtual
  462/463); JSON_TABLE's Table Function Scan costs its paths once plus a tuple per row, its
  width its columns'; a unique index of a system catalog makes its column unique for
  get_variable_numdistinct.
- A void SQL function whose body computes another type isn't inlined (inline_function's
  exprType check): `Output: voidtest1(33)`.
- `ROLL_DUMP_PLAN=1` in the engine's environment prints the plan tree EXPLAIN receives on stderr
  (a debugging aid).

Goldens added: `tests/sql/ga4_plan_tail.sql`, `ga4_plan_tail2.sql`, `ga4_conflict_rule.sql`.

Looked at and left (why):

- memoize 11: a PlaceHolderVar (`s.c1 = t1.twenty` under a LEFT JOIN LATERAL) is a restriction
  clause of the inner side in Postgres (lift.rs territory); 52: a parameterized Append path over
  a UNION ALL of partitions; 67: a parallel plan.
- equivclass 38/48: a broken class (`=` missing across the made types) keeps both the derived
  and the source `ff = 42` (Index Cond shows it twice); the engine dedupes them. 52/59: a Merge
  Append of UNION ALL arms ordered by expressions as a merge join input (the same capability as
  inherit 445). 87/94: a merge / hash join over an operator without a commutator needs the
  FULL JOIN's sides swapped so its left argument is the outer side, and a made type's `USING <`
  sort.
- inherit 445: the plan merges the UNION ALL arms in `1 - id` order already (merge_append),
  but EXPLAIN shows it as Sort / Result / Append: the projection above the Append has to go into
  each arm (Merge Append members computing the key) for `merged_appended` to see it. 453 (and
  equivclass 52/59): the join search has no ordered (Merge Append) paths for an append
  relation's members, so a Merge Join over Merge Appends under the LIMIT isn't considered.
- updatable_views 173/174: the rule-rewritten UPDATE joins the view's subquery to the table by a
  parameterized (lateral) join (`a = OLD.a`), so the subquery's `a = 2` doesn't reach the table
  as an equivalence-class constant.
- partition_info 40/45: the join cost of `pg_partition_tree(...) JOIN pg_class` differs (a
  catalog scan's width counts all its columns, not those needed).
- sqljson_jsontable 101: a join rel with a LATERAL reference to only some of its inputs
  (Postgres joins `x` with `JSON_TABLE(... PASSING x, y)` first, parameterized by `y`); the
  engine's join search has no partially-satisfied LATERAL join rels.

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
| insert_conflict | 261 | 261 | 100% | yes | no |  |
| create_function_c | 10 | 10 | 100% | no | no |  |
| create_misc | 88 | 88 | 100% | yes | yes |  |
| create_operator | 90 | 90 | 100% | yes | yes |  |
| create_procedure | 115 | 115 | 100% | yes | yes |  |
| create_table | 348 | 348 | 100% | yes | no |  |
| create_type | 88 | 88 | 100% | no | no |  |
| create_schema | 30 | 30 | 100% | yes | yes |  |
| create_index | 708 | 708 | 100% | yes | yes |  |
| create_index_spgist | 202 | 202 | 100% | yes | yes |  |
| create_view | 343 | 343 | 100% | no | no |  |
| index_including | 137 | 137 | 100% | yes | yes |  |
| index_including_gist | 52 | 52 | 100% | yes | yes |  |
| create_aggregate | 60 | 60 | 100% | yes | yes |  |
| create_function_sql | 168 | 168 | 100% | yes | yes |  |
| create_cast | 24 | 24 | 100% | yes | yes |  |
| constraints | 607 | 607 | 100% | yes | yes |  |
| triggers | 1041 | 1041 | 100% | no | no |  |
| select | 87 | 87 | 100% | yes | yes |  |
| inherit | 934 | 934 | 100% | yes | yes |  |
| typed_table | 36 | 36 | 100% | yes | yes |  |
| vacuum | 328 | 328 | 100% | yes | yes |  |
| drop_if_exists | 161 | 161 | 100% | yes | yes |  |
| updatable_views | 1109 | 1109 | 100% | no | no |  |
| roleattributes | 80 | 80 | 100% | yes | yes |  |
| create_am | 142 | 142 | 100% | yes | yes |  |
| hash_func | 43 | 43 | 100% | yes | yes |  |
| errors | 87 | 87 | 100% | yes | yes |  |
| infinite_recurse | 8 | 8 | 100% | yes | yes |  |
| sanity_check | 3 | 3 | 100% | yes | yes |  |
| select_into | 67 | 67 | 100% | yes | yes |  |
| select_distinct | 101 | 101 | 100% | yes | yes |  |
| select_distinct_on | 23 | 23 | 100% | yes | yes |  |
| select_implicit | 44 | 44 | 100% | yes | yes |  |
| select_having | 23 | 23 | 100% | yes | yes |  |
| subselect | 356 | 356 | 100% | yes | yes |  |
| union | 198 | 198 | 100% | yes | yes |  |
| case | 64 | 64 | 100% | yes | yes |  |
| join | 925 | 906 | 98% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (19) |
| aggregates | 556 | 556 | 100% | yes | no |  |
| transactions | 433 | 433 | 100% | yes | yes |  |
| random | 48 | 48 | 100% | yes | yes |  |
| portals | 354 | 354 | 100% | yes | yes |  |
| arrays | 519 | 519 | 100% | yes | yes |  |
| btree_index | 135 | 135 | 100% | yes | yes |  |
| hash_index | 102 | 102 | 100% | yes | yes |  |
| update | 289 | 289 | 100% | yes | yes |  |
| delete | 10 | 10 | 100% | yes | yes |  |
| namespace | 42 | 42 | 100% | yes | yes |  |
| prepared_xacts | 2 | 2 | 100% | yes | yes |  |
| brin | 77 | 77 | 100% | yes | yes |  |
| gin | 59 | 59 | 100% | yes | yes |  |
| gist | 69 | 69 | 100% | yes | yes |  |
| spgist | 31 | 31 | 100% | yes | yes |  |
| privileges | 1398 | 1398 | 100% | no | no |  |
| init_privs | 4 | 4 | 100% | yes | yes |  |
| security_label | 28 | 28 | 100% | yes | yes |  |
| collate | 149 | 149 | 100% | yes | yes |  |
| matview | 189 | 189 | 100% | yes | yes |  |
| lock | 134 | 134 | 100% | no | no |  |
| replica_identity | 75 | 75 | 100% | yes | yes |  |
| rowsecurity | 1226 | 1226 | 100% | yes | no |  |
| object_address | 85 | 85 | 100% | yes | yes |  |
| tablesample | 58 | 58 | 100% | yes | yes |  |
| groupingsets | 217 | 217 | 100% | yes | yes |  |
| drop_operator | 12 | 12 | 100% | yes | yes |  |
| password | 53 | 53 | 100% | yes | yes |  |
| identity | 288 | 288 | 100% | yes | yes |  |
| generated_stored | 468 | 468 | 100% | yes | yes |  |
| join_hash | 279 | 279 | 100% | yes | yes |  |
| brin_bloom | 40 | 40 | 100% | yes | yes |  |
| brin_multi | 172 | 172 | 100% | yes | yes |  |
| create_table_like | 191 | 191 | 100% | yes | yes |  |
| alter_generic | 336 | 336 | 100% | no | no |  |
| alter_operator | 65 | 65 | 100% | yes | yes |  |
| misc | 68 | 68 | 100% | no | no |  |
| async | 11 | 11 | 100% | yes | yes |  |
| dbsize | 25 | 25 | 100% | yes | yes |  |
| merge | 572 | 572 | 100% | yes | yes |  |
| misc_functions | 148 | 148 | 100% | no | no |  |
| sysviews | 29 | 29 | 100% | yes | yes |  |
| tsrf | 74 | 74 | 100% | yes | yes |  |
| tid | 41 | 41 | 100% | yes | yes |  |
| tidscan | 49 | 49 | 100% | yes | yes |  |
| tidrangescan | 45 | 45 | 100% | yes | yes |  |
| collate.utf8 | 61 | 61 | 100% | yes | yes |  |
| collate.icu.utf8 | 681 | 681 | 100% | yes | yes |  |
| incremental_sort | 120 | 120 | 100% | yes | no |  |
| create_role | 144 | 144 | 100% | yes | yes |  |
| without_overlaps | 674 | 674 | 100% | yes | yes |  |
| generated_virtual | 489 | 489 | 100% | yes | no |  |
| rules | 632 | 632 | 100% | yes | yes |  |
| psql | 1264 | 1264 | 100% | no | no |  |
| psql_crosstab | 32 | 32 | 100% | yes | yes |  |
| psql_pipeline | 362 | 362 | 100% | yes | yes |  |
| amutils | 10 | 10 | 100% | yes | yes |  |
| stats_ext | 878 | 878 | 100% | yes | no |  |
| collate.linux.utf8 | 2 | 2 | 100% | yes | yes |  |
| select_parallel | 250 | 250 | 100% | yes | yes |  |
| write_parallel | 22 | 22 | 100% | yes | yes |  |
| vacuum_parallel | 14 | 14 | 100% | yes | yes |  |
| maintain_every | 16 | 16 | 100% | yes | yes |  |
| publication | 781 | 781 | 100% | yes | yes |  |
| subscription | 179 | 179 | 100% | no | no |  |
| select_views | 52 | 52 | 100% | no | no |  |
| portals_p2 | 41 | 41 | 100% | yes | yes |  |
| foreign_key | 1267 | 1267 | 100% | yes | yes |  |
| cluster | 205 | 205 | 100% | yes | yes |  |
| dependency | 68 | 68 | 100% | yes | yes |  |
| guc | 222 | 222 | 100% | yes | yes |  |
| bitmapops | 12 | 12 | 100% | yes | yes |  |
| combocid | 62 | 62 | 100% | yes | yes |  |
| tsearch | 451 | 451 | 100% | yes | yes |  |
| tsdicts | 131 | 131 | 100% | yes | yes |  |
| foreign_data | 624 | 624 | 100% | no | no |  |
| window | 400 | 400 | 100% | yes | yes |  |
| xmlmap | 40 | 40 | 100% | no | no |  |
| functional_deps | 40 | 40 | 100% | yes | yes |  |
| advisory_lock | 38 | 38 | 100% | yes | yes |  |
| indirect_toast | 31 | 31 | 100% | no | no |  |
| equivclass | 96 | 96 | 100% | yes | yes |  |
| json | 469 | 469 | 100% | yes | yes |  |
| jsonb | 1090 | 1090 | 100% | yes | yes |  |
| json_encoding | 47 | 47 | 100% | yes | yes |  |
| jsonpath | 224 | 224 | 100% | yes | yes |  |
| jsonpath_encoding | 35 | 35 | 100% | yes | yes |  |
| jsonb_jsonpath | 836 | 836 | 100% | yes | yes |  |
| sqljson | 242 | 242 | 100% | yes | yes |  |
| sqljson_queryfuncs | 318 | 318 | 100% | yes | yes |  |
| sqljson_jsontable | 131 | 131 | 100% | yes | yes |  |
| plancache | 97 | 97 | 100% | yes | yes |  |
| limit | 85 | 85 | 100% | yes | yes |  |
| plpgsql | 941 | 941 | 100% | yes | yes |  |
| copy2 | 290 | 290 | 100% | no | no |  |
| temp | 211 | 211 | 100% | yes | yes |  |
| domain | 486 | 486 | 100% | no | no |  |
| rangefuncs | 428 | 428 | 100% | yes | no |  |
| prepare | 33 | 33 | 100% | yes | yes |  |
| conversion | 68 | 68 | 100% | no | no |  |
| truncate | 193 | 193 | 100% | yes | yes |  |
| alter_table | 1749 | 1749 | 100% | yes | yes |  |
| sequence | 265 | 265 | 100% | no | no |  |
| polymorphism | 426 | 426 | 100% | yes | yes |  |
| rowtypes | 237 | 237 | 100% | yes | no |  |
| returning | 145 | 145 | 100% | yes | yes |  |
| largeobject | 151 | 151 | 100% | yes | yes |  |
| with | 307 | 307 | 100% | yes | yes |  |
| xml | 281 | 281 | 100% | yes | yes |  |
| partition_join | 614 | 605 | 99% | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (9) |
| partition_prune | 771 | 770 | 100% | yes | no | wrong result `SELECT: row count differs` (1) |
| reloptions | 66 | 66 | 100% | yes | yes |  |
| hash_part | 29 | 29 | 100% | yes | yes |  |
| indexing | 645 | 645 | 100% | yes | yes |  |
| partition_aggregate | 137 | 137 | 100% | yes | yes |  |
| partition_info | 73 | 73 | 100% | yes | yes |  |
| tuplesort | 108 | 108 | 100% | yes | no |  |
| explain | 60 | 60 | 100% | yes | yes |  |
| memoize | 73 | 73 | 100% | yes | yes |  |
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
