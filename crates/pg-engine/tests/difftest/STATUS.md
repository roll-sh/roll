# Differential testing against PostgreSQL 18.6

`tests/difftest.rs` (+ `difftest/generate.rs`) generates random schemas, data and statements
from a seed, runs each statement on the in-process engine and on the reference server
(`roll-engine-ref`), and compares what a client sees. All entry points are `#[ignore]`d.

## Running it

```sh
B=crates/pg-engine/tools/heavy.sh
# build once (through the lock), then run the binary directly (a light job, one process)
$B cargo test -p pg-engine --test difftest --no-run
BIN=$(ls -t target/debug/deps/difftest-* | grep -v '\.d$' | head -1)
# smoke: the regression check (fixed seeds 1-25, 80 statements each, a few minutes);
# fails on any mismatch of a class above `error_message`
DIFFTEST_DB=<yourprefix>_dt DIFFTEST_OUT=~/.cache/<you>/dt $BIN --ignored --exact smoke --nocapture
# a run over many seeds
DIFFTEST_SEEDS=1-200 DIFFTEST_N=150 DIFFTEST_DB=... DIFFTEST_OUT=... $BIN --ignored --exact run --nocapture
# reduce a recorded case to a minimal reproducer (writes <case>.min.sql)
DIFFTEST_CASE=<case.sql> $BIN --ignored --exact reduce --nocapture
# run a case or any .sql file: `replay_case` (its last statement), `probe` (every statement,
# printing both sides where they differ)
DIFFTEST_CASE=<file.sql> $BIN --ignored --exact probe --nocapture
```

Set `TMPDIR` to a directory on disk (the engine's stores go there; /tmp is RAM on ai-server)
and `DIFFTEST_DB` to your own prefix: the runner drops and creates `<prefix>_<seed>` on the
reference. Other knobs: `DIFFTEST_REF` (127.0.0.1:55499), `DIFFTEST_PASSWORD` (pw),
`DIFFTEST_PLAN` (share of queries re-run under random planner settings, 0.25),
`DIFFTEST_ORACLES` (share of statements followed by a TLP / NoREC check, 0.2),
`DIFFTEST_HANG_SECS` (60), `DIFFTEST_SMOKE_SEEDS` (25), `DIFFTEST_PANIC_TRACE` (print panics),
`DIFFTEST_ECHO` (write the statement about to run, with its setup, to
`DIFFTEST_OUT/current-s<seed>.sql`: the case of a crash that aborts the process).

## What it generates

- **Schemas**: 2–4 tables of int2/int4/int8, numeric(10,2), float8 (dyadic values, so sums
  are exact in any order), text (default, "C", "POSIX", und-x-icu, en-x-icu collations),
  bool, date, timestamp, timestamptz, interval, int4[], text[], jsonb, json; NULLs; primary
  keys, defaults; partitioned tables (range / list / hash, with and without default
  partitions); inheritance children; btree (multi-column, DESC, partial, expression, unique
  NULLS NOT DISTINCT), hash and gin indexes; views; SQL and PL/pgSQL functions (IF / CASE /
  FOR loops / exception handlers, STRICT, every volatility) and set-returning functions
  (`RETURNS TABLE`, SQL and `RETURN QUERY`); ANALYZE or not.
- **Queries**: joins of every kind (inner, LEFT, RIGHT, FULL, comma, CROSS, USING, LATERAL
  sub-selects, VALUES, generate_series, unnest WITH ORDINALITY, set-returning functions);
  sub-selects (scalar, IN / NOT IN, EXISTS / NOT EXISTS, ANY / ALL / SOME, correlated at two
  levels); aggregates (count, sum, avg, min / max, bool_and / or, bit_and / or, string_agg,
  array_agg, jsonb_agg with ORDER BY, DISTINCT, FILTER); GROUP BY with ROLLUP / CUBE /
  GROUPING SETS / GROUPING(); HAVING; window functions (rank-like, sum / max over RANGE /
  GROUPS / ROWS frames, lag / lead / ntile / first_value under a total order); CTEs
  (MATERIALIZED, NOT MATERIALIZED, recursive UNION / UNION ALL); UNION / INTERSECT / EXCEPT
  [ALL]; DISTINCT, DISTINCT ON; ORDER BY with NULLS FIRST / LAST, LIMIT, OFFSET; FOR UPDATE /
  SHARE / NO KEY UPDATE / KEY SHARE; casts (including through text), CASE, COALESCE, NULLIF,
  GREATEST / LEAST, IS [NOT] DISTINCT FROM, IS TRUE / UNKNOWN, BETWEEN SYMMETRIC, LIKE /
  ILIKE / regex / SIMILAR TO, COLLATE, row comparisons, array and jsonb operators, many
  text / numeric / date functions.
- **DML**: INSERT ... VALUES / SELECT with ON CONFLICT DO NOTHING / DO UPDATE ... WHERE,
  UPDATE (with FROM a keyed sub-select), DELETE, MERGE (WHEN MATCHED [AND] UPDATE / DELETE,
  NOT MATCHED INSERT, NOT MATCHED BY SOURCE DELETE, RETURNING merge_action()), all with
  RETURNING; DO blocks raising notices; ANALYZE. After each DML statement the table's whole
  contents are compared too (`wrong_state`; the seed stops there, its data having diverged).

Results are compared as a multiset unless the query's ORDER BY covers every output column
(then in order); column names and types, command tags, errors (SQLSTATE and message; DETAIL /
HINT / position as `error_detail`) and notices are compared too.

**Oracles on the engine alone** (a difference the reference shows too is dropped as the
generator's): TLP (a query against the union of its WHERE p / NOT p / p IS NULL partitions,
plain, DISTINCT and aggregated) and NoREC (`count(*) WHERE p` against
`sum(CASE WHEN p THEN 1 ELSE 0 END)`). **Planner settings**: a share of the queries runs again
on both sides under 1–4 random `enable_*` settings (hashjoin, mergejoin, nestloop, seqscan,
indexscan, indexonlyscan, bitmapscan, sort, hashagg, memoize, material, incremental_sort,
presorted_aggregate, partitionwise join / aggregate, partition_pruning, group_by_reordering,
self_join_elimination, distinct_reordering), `join_collapse_limit = 1`, a small `work_mem`;
the engine's rows must not change with the plan (`plan_variance`).

## Classes

Most serious first: `crash` (a panic), `hang`, `wrong_rows`, `wrong_order`, `wrong_state`,
`tlp`, `norec`, `plan_variance`, `wrong_columns`, `engine_error` (the engine errs, the
reference doesn't), `missing_error`, `wrong_error` (another SQLSTATE), `eval_error` (a data
error, 22xxx / 21000, on one side only or a different one: which rows and expressions get
evaluated, or folded while planning, differs), `error_message`, `notice`, `tag`,
`error_detail`.

## Score

| run | seeds × statements | statements | queries | both sides error | mismatches |
|---|---|---|---|---|---|
| first (before fixes) | 60 × 120 | 7,063 | 6,067 | 1,669 | wrong_rows 1, wrong_state 2, engine_error 136, missing_error 142, wrong_error 73, error_message 80, error_detail 148 |
| after batch 1 | 60 × 120 | 7,158 | 6,145 | 1,693 | wrong_rows 2, wrong_state 1, engine_error 16, missing_error 138, wrong_error 57, eval_error 148, error_message 80, error_detail 152 |
| smoke after batch 2 | 25 × 80 | 2,000 | 1,701 | 468 | wrong_rows 1, engine_error 7, missing_error 11, wrong_error 12, eval_error 34, error_message 15, error_detail 45 |
| wider grammar (round 2), seeds 100–299, batch 3 engine | 200 × 150 | 29,422 | 25,047 | 7,080 | wrong_rows 11, wrong_state 10, engine_error 83, missing_error 153, wrong_error 237, eval_error 744, error_message 122, error_detail 548 |
| smoke, round 2 grammar | 25 × 80 | 1,937 | 1,654 | 486 | wrong_rows 1, wrong_state 1, engine_error 9, missing_error 6, wrong_error 12, eval_error 34, error_message 10, error_detail 29 |

Round 2 of the grammar added DELETE ... USING, data-modifying CTEs, window frames with
EXCLUDE, ordered-set aggregates (percentile_disc, mode), more jsonb and integer functions.
About a quarter of the statements error on both sides (the generator's waste; the run prints
the most common such errors): array_length / jsonb functions over jsonb scalars, unknown
literals to polymorphic functions, aggregates in WHERE / JOIN ON, LATERAL-less references to
FROM items.

`smoke_known.txt` lists the smoke run's remaining mismatches above `error_message` (14 since
round 4, batch 1): the smoke test fails only on one not in it.

## Fixed

Batch 1 (each with a golden in `tests/sql/a35_*.sql`):

- **Wrong rows**: a grouping set nulled a constant (or any key without the level's columns)
  wherever it appeared, not just in the select-list item the key names:
  `SELECT 1 AS c0, count(*) FROM t GROUP BY GROUPING SETS ((b), 1) HAVING 1 IS DISTINCT FROM NULL`
  lost the `(b)` set's rows; `SELECT b, (1+1)*3 ... GROUPING SETS ((b), (1+1))`,
  `now() IS NULL` likewise. Keys with columns or volatile functions, and outer references in
  the select list, are still nulled everywhere (`a35_groupingsets_const`).
- **Wrong columns / wrong error**: a function in FROM with one OUT column
  (`RETURNS TABLE (x int)`, `OUT y`) or a one-field composite named its column after the
  alias (`SELECT s2.x FROM srf(9) s2`: "column s2.x does not exist"); its whole-row reference
  was a record, not the scalar; a composite function's whole row was an anonymous record
  (`a35_srf_out_column`).
- **Wrong error**: a boolean test of a constant (`NULL IS FALSE`) wasn't folded, so
  `FULL JOIN ... ON NULL IS FALSE` failed with "FULL JOIN is only supported with merge-joinable
  or hash-joinable join conditions" (`a35_full_join_const`).

Batch 2 (`a35_parse_errors`):

- **Missing errors**: `FOR UPDATE / SHARE / NO KEY UPDATE / KEY SHARE` on the nullable side
  of an outer join (after reduce_outer_joins) was accepted
  (`SELECT 1 FROM a LEFT JOIN b ON a.x = b.y FOR UPDATE`); FOR UPDATE with window functions
  or set-returning functions in the select list too.
- **Missing error**: polymorphic parameters with only untyped literals
  (`SELECT to_json('Ab')`: "could not determine polymorphic type because input has type
  unknown").
- **Missing errors / message**: a sub-select's ungrouped outer references: the message is
  "subquery uses ungrouped column ... from outer query", and they are checked as written, also
  when folding drops the sub-select (`HAVING NULL BETWEEN (SELECT t.b ...) AND NULL`,
  `CASE WHEN 5 IS NULL THEN (SELECT t.a ...) END`).
- **Engine error**: DISTINCT ON and ORDER BY items match by target-list entry, and the sort
  list skips an item naming an entry it has (`SELECT DISTINCT ON (1) b, 1, b ... ORDER BY 1,
  2, 3` is valid).

Batch 3 (`a35_equal_values`):

- **Wrong rows**: `max` / `min` kept the first of two values that compare equal but differ;
  PostgreSQL's `X_larger` / `X_smaller` keep the newer (`1.0` vs `1.00`, `'1 mon'` vs
  `'30 days'`, `0` vs `-0`, arrays and rows of those, strings a nondeterministic collation
  finds equal), bpchar's and tid's the older:
  `SELECT max(x) FROM (VALUES (1.0), (1.00)) v(x)` is `1.00`.
- **Wrong rows**: bpchar compared with its trailing spaces in merge joins, grouping,
  DISTINCT, DISTINCT ON, count(DISTINCT), UNION / INTERSECT / EXCEPT and max / min:
  `char(2)` joined with `char(4)` on equal values returned no rows under a merge join
  (`SELECT * FROM ca JOIN cb ON x = y`), `SELECT x FROM ca UNION SELECT y FROM cb` kept both
  `'a '` and `'a   '`.

Batch 4 (`a35_sje_outer_join`):

- **Wrong rows**: self-join elimination took an outer join's ON clauses as equalities between
  the scans it compares: `SELECT t7.id, t8.id FROM (t2 t7 CROSS JOIN t2 t8) LEFT JOIN t0 t9 ON
  t8.id = t9.a AND t7.id = t9.a` (t2's key `id`) merged t7 into t8 and returned 2 of 4 rows
  (`paths/analyzejoins.rs` `self_join_pair`).

Batch 5 (`a35_full_join_dummy`):

- **Wrong error / wrong state**: a FULL JOIN under a constant-false restriction of its own
  join relation (`WHERE false`, `WHERE NOT NULL`) raised "FULL JOIN is only supported with
  merge-joinable or hash-joinable join conditions"; PostgreSQL marks the join relation dummy
  first (restriction_is_constant_false), so `INSERT ... SELECT count(*) FROM (a FULL JOIN b ON
  <not mergeable>) WHERE NOT NULL` inserts its one row.

Round 18, agent 45, batch 1 (`a45_oj_pseudoconst`, `a45_groupingsets_empty`):

- **Wrong rows**: a clause of an inner join reading no column of the level (an uncorrelated
  sub-select, an InitPlan's output, a volatile call) on the nullable side of an outer join
  gated the whole outer join: `SELECT * FROM c LEFT JOIN (a a1 JOIN a a2 ON (SELECT false))
  ON a1.x = c.z` returned no rows (also `ON random() > 2`, `NOT EXISTS (...)`, RIGHT JOINs,
  nested and multi-way joins). It was the join search, not the binder: `outerjoins.rs`
  `gather_group` took every sub-select for volatile and gave the clause no items (the top of
  the group). Now as distribute_qual_to_rels: such a clause holds at its join's items
  (qualscope) unless it is pseudoconstant and in the top join domain (not under a LEFT /
  anti join's nullable side), where it gates the whole plan. EXPLAIN gates an inner join on
  an InitPlan's output with a Result (`create_gating_plan`) instead of a Join Filter.
- **Wrong rows**: with grouping sets, an expression equal to a grouping key written as a
  constant (`1 AS c2` with `ROLLUP (t.b, 1)` naming `1 AS c0`), other than the item the key
  names, is no grouping column: PostgreSQL computes it below the Agg and reads it from the
  group's representative row, so it is never nulled, but it is NULL in the empty grouping
  set's row over no input rows. The binder makes each such occurrence a pass-through key
  (a key in no grouping set, `select.rs` `PassThrough`), which the executor takes from the
  group's first row (`agg.rs` `set_key`) and GROUPING() skips; EXPLAIN shows them as `(1)`
  and as extra input columns. A constant `ORDER BY` item names its select-list entry
  (`ORDER BY 2` sorts by c2's value, not the key's).

Round 18, agent 45, batch 2 (`a45_constant_folding`):

- **Errors PostgreSQL never raises (constant folding)**: the binder still folds constants as
  it builds expressions, but a constant that fails to fold stays unfolded and its error is
  recorded (`Binder::fold_failed`); once the statement is planned, the first such
  expression still in the plan fails it, in eval_const_expressions' order
  (`binder/fold.rs`: a level's CTEs first, then its target list, then its quals inputs
  first, each expression's sub-selects after it; AND / OR stop at the deciding constant,
  CASE skips arms with a constant false / NULL condition and everything after a constant
  true one, COALESCE stops at its first non-NULL constant). So the unused outputs of pulled-up
  subqueries and views, of subqueries planned apart (unless sort / group keys or
  DISTINCT), CTEs nothing references (no longer planned at all, nor shown by EXPLAIN), the
  select list of an EXISTS (simplify_EXISTS_query, unless it aggregates), unused column
  defaults and `false AND 1/0 = 1` no longer fail; `1/0 = 1 AND false`, `int4pl(NULL,
  1/0)`, `ORDER BY 1/0`, a degenerate HAVING over a FROM that fails, and relations found
  empty (their unfolded constants kept with the dummy rel, `DummyInfo::kept`) still do.
  Casts of typed constants (`'NaN'::numeric::int`, `2147483648::int`) and NULLIF fold the
  same way; an unknown literal's input conversion still fails at once, as in the parser.
  Smoke: eval_error 34 -> 11, engine_error 9 -> 6.

Round 18, agent 45, batch 3 (`a45_saop_all_empty`, `a45_oj_preserved_clause`, `a45_misc_r18`),
found by the wider grammar (seeds from 1000 on, below):

- **Wrong rows**: `x op ALL (array)` is true over an empty array whatever x is, but a WHERE
  clause of it made a LEFT JOIN an inner one (`reducejoins.rs` / `outerjoins.rs`, as
  is_strict_saop: ALL only over an array known to have elements): `SELECT * FROM a LEFT JOIN
  b ON a.x = b.x WHERE b.z < ALL ('{}')` lost a's unmatched rows.
- **Wrong rows**: an outer join's ON clause reading only its preserved side, over a subquery
  planned apart (`(SELECT count(*) > -1 AS c FROM ...) v ... ON v.c`): the join search took the
  clause for a restriction below the join, the subquery returned NULL for the column and
  the clause never held (`joinpath.rs` `needed_above`).
- **Wrong error order**: pseudoconstant quals cost nothing per row (tested once), so they
  keep the order they were distributed in (a pulled-up subquery's WHERE before the WHERE
  above it), which decides which of two failing quals fails the statement.
- **Error message**: an ON clause's reference to a FROM item outside its join is "invalid
  reference to FROM-clause entry" with PostgreSQL's DETAIL, not "missing FROM-clause entry".
- Constant folding, follow-ups: CTEs nothing references are dropped by their bound references
  (not the statement's name counts, which missed references inside VALUES and ON CONFLICT),
  constants kept with empty relations are only quals and outputs something reads, an
  immutable function call over constants that fails when the binder runs it waits for the
  plan too, and a cursor's query folds as any other.

Round 18, agent 45, batch 4 (`a45_full_join_quals`):

- **Wrong rows**: `array_remove` of a one-dimensional array with a lower bound other than 1
  rebased it to 1 (`array_remove('[0:1]={-2,-1}', 5)` is `[0:1]={-2,-1}`).
- **Missing errors**: a FULL JOIN whose ON clause is a NULLIF, CASE or COALESCE over the
  sides, or a sub-select (even one of no column: an outer join's clause is never a gating
  qual), is neither merge- nor hash-joinable: "FULL JOIN is only supported with
  merge-joinable or hash-joinable join conditions".
- Constant folding: a sub-select is planned after the expression around it is simplified, so
  what fails inside it doesn't keep an `OR true` / `AND false` from deciding the clause,
  and the sub-selects of a decided clause are never planned (`NOT IN (SELECT ... failing)
  OR 11 > 9` deletes rows instead of failing). A UNION ALL appendrel's members keep the
  outputs whose constants failed to fold (PostgreSQL simplifies all of an appendrel's
  translated vars): `SELECT 1 FROM (SELECT 1/0 FROM t UNION ALL SELECT 1) s` fails.

Batch 5: a grouping key written as an expression (`abs(9)`, `extract(...)::int`) is a
pass-through value in the items that contain it as written (`CASE WHEN a > 0 THEN abs(9)
ELSE abs(9) END`: NULL in the empty set's row over no input), not only literal keys; one
the binder folds away with its surroundings (`abs(9) + 1`) still isn't.

Batch 6 (`a45_setop_empty_first`): INTERSECT / EXCEPT never read their second input when
the first one (EXCEPT's left, INTERSECT's with fewer groups) is empty, as nodeSetOp, so an
error evaluating it doesn't happen (`SELECT 1 FROM t WHERE false EXCEPT SELECT 2147483647 * a
FROM t`) (`exec/nodes.rs` `SetOpNode`).

Batch 7 (`a45_inline_strict`):

- **Crash**: planning a query level inside a lifted join group (a FROM subquery with a FULL
  JOIN under a CROSS JOIN with a view over EXCEPT) panicked with "RefCell already borrowed":
  `joinpath.rs` held `ExecCtx::bound_trees` borrowed across planning the group, which records
  trees of its own (round 16's bound join trees).
- **Errors PostgreSQL never raises**: a strict SQL function whose body has non-strict
  constructs (CASE, COALESCE, IS [NOT] NULL, boolean tests, AND / OR, IS DISTINCT, NULLIF,
  GREATEST / LEAST, ARRAY[], ROW()) isn't inlined (inline_function's
  contain_nonstrict_functions), so what its body would fold to doesn't fail the statement.

Batch 8 (`a45_cbrt_musl`): `cbrt()` follows musl's algorithm (the reference's C library)
instead of the host libm, whose last digit differs for some inputs (`cbrt(3.75)`).

## Wider grammar (round 18)

`generate.rs` `wide_expr` and the statement kinds after it, for seeds from `WIDE_SEEDS`
(1000) on (the smoke seeds keep their statements): date / time arithmetic, time zones and
DST, `date_bin`, `make_*`, infinite dates and timestamps, intervals (ISO 8601, justify,
`age`, multiplication and division), numeric NaN / Infinity / huge scales and `^`, float
edge values and formatting, `to_char` patterns, regular expressions (`regexp_*`,
`substring ... from`, `~*`), `SIMILAR TO`, `LIKE ... ESCAPE`, `ILIKE`, collations, SQL/JSON
(`JSON_VALUE`, `JSON_QUERY` with wrappers, `JSON_EXISTS`, `JSON_OBJECT`, `JSON_ARRAY`, `IS
JSON`), jsonpath (`@?`, `@@`, `jsonb_path_query_array`, `like_regex`, `.datetime()`),
multidimensional arrays and slices, ranges and multiranges (operators, union /
intersection, `range_merge`); prepared statements with parameters, cursors, savepoints with
errors in transactions, recursive CTEs with SEARCH / CYCLE, window frames with EXCLUDE.

| run | seeds × statements | statements | serious mismatches |
|---|---|---|---|
| wide, before batch 3 | 1000–1039 × 100 | 4,000 | wrong_rows 2, tlp 1 |
| wide, before batch 3 | 1040–1139 × 100 | 9,893 | wrong_rows 2, wrong_state 3, tag 3 |
| wide, batch 3 engine | 1140–1339 × 100 | 19,995 | wrong_rows 1, wrong_order 1, wrong_state 1, tag 6 |
| wide, batch 5 engine | 1340–1539 × 100 | 19,868 | wrong_rows 6, wrong_state 3, tag 9 |
| wide, batch 6 engine | 1540–1739 × 100 | 19,943 | crash 2, wrong_rows 6, wrong_state 3, tag 3 |
| round 2 (agent 68), before and after batch 1 | 1740–1889 × 100 | 14,895 | wrong_rows 3, wrong_state 3, tag 6 (the same both) |
| round 2, base (a4823f7c) | 1890–2039 × 100 | 14,991 | wrong_rows 1, wrong_state 1, engine_error 44, eval_error 96, tag 6 |
| round 2, batch 2 | 1890–2039 × 100 | 14,991 | wrong_state 1, engine_error 30, eval_error 84, tag 5 |
| round 3 (agent 75), base (f4406f71) | 2040–2189 × 100 | 15,051 | wrong_rows 1, wrong_columns 1, engine_error 40, missing_error 33, eval_error 101, tag 9 |
| round 3, batch 1 | 2040–2189 × 100 | 15,051 | engine_error 39, missing_error 24, eval_error 98, tag 9 |
| round 3 grammar, batch 1 engine before its last fixes | 3030–3329 × 100 | 29,911 | wrong_rows 2, wrong_state 6, wrong_columns 1, tag 7 |
| round 4 (agent 84), base (2b703e96) | 3330–3479 × 100 | 14,991 | wrong_rows 4, wrong_state 2, engine_error 30, missing_error 64, wrong_error 46, eval_error 77, tag 11 |
| round 4, batch 1 | 3330–3479 × 100 | 14,991 | wrong_rows 4, wrong_state 2, engine_error 30, missing_error 53, wrong_error 45, eval_error 63, tag 9 |
| round 4, base (ffceeaa8) | 3480–3533 × 100, then a crash | ~5,400 | aborted at seed 3534 by a stack overflow (fixed in batch 2); 83 serious-or-eval mismatches until then |
| round 4, batch 2 | 3480–3579 × 100 | 9,941 | 74 over 3480–3533 (none new); in all wrong_rows 1, wrong_state 2, wrong_columns 2, engine_error 20, missing_error 38, wrong_error 11, eval_error 61, tag 6 (the wrong_rows / wrong_state of 3563 and 3570 are the base's too) |
| round 5 grammar (agent 104), engine before batch 1's fixes | 5006–5105 × 100 | 9,807 | crash 3, wrong_rows 59, wrong_state 7, wrong_columns 7, engine_error 14, missing_error 20, wrong_error 22, eval_error 53, tag 5 |
| round 5, base (0f9f0609, round 5 generator) | 5106–5255 × 100 | 14,256 | crash 6, wrong_rows 64, wrong_state 15, engine_error 25, missing_error 10, wrong_error 42, eval_error 65, notice 5, tag 1 |
| round 5, batch 1 (before its last fix) | 5106–5255 × 100 | 15,161 | wrong_rows 9, engine_error 11, missing_error 10, wrong_error 37, eval_error 28, tag 1 (213 mismatches above error_message in the base, 93 here; 134 gone; 6 new, the trigger WHEN of seeds 5130 / 5207, fixed in the batch; 8 more only past a seed's divergence in the base) |
| round 5, base (0f9f0609, round 5 generator) | 5256–5355 × 100 | 9,697 | crash 2, wrong_rows 39, wrong_state 7, engine_error 14, missing_error 8, wrong_error 28, eval_error 26, notice 7, tag 1 |
| round 5, batch 2 | 5256–5355 × 100 | 10,064 | wrong_rows 1, wrong_state 2, engine_error 8, missing_error 12, wrong_error 21, eval_error 13, notice 3, tag 1 (110 above error_message in the base, 52 here; 64 gone; none new but 6 past a base divergence; the wrong_rows / wrong_state are the old grammar's, the base's too: DISTINCT ON's physical order, a failing UPDATE condition, ANALYZE's sample) |

## Round 6 grammar (agent 118)

Seeds from `R6_SEEDS` (6000) on add the `r6*` objects (`round6.rs`, its own generator; the
round 5 objects exist too) and make about a third of the statements `round6_stmt`'s, picked by
what applications send most:

- Cursors over every kind of query (in index order or not, joins, partitions, groupings,
  DISTINCT, UNION ALL, LIMIT), declared with or without SCROLL / NO SCROLL / WITH HOLD, with
  fetches in every direction (PRIOR, BACKWARD, ABSOLUTE, LAST, MOVE BACKWARD ALL) and WHERE
  CURRENT OF on any of them, each step in a savepoint rolled back after it; `pg_cursors`'
  `is_scrollable`; EXPLAIN (COSTS OFF) DECLARE under cursor_tuple_fraction settings. The round 5
  restriction (backward fetches and CURRENT OF only where the cursor says) is gone.
- Binary COPY: out of tables and queries over many types (typsend of numerics with their
  specials, intervals, ranges, multiranges, arrays, records, json, inet, bit), and into `r6u`
  from streams well formed or not (signature, flags, field counts and lengths, the trailer
  missing, data after it, invalid UTF-8, NULL keys). Case files write the stream as `\x` and hex
  digits after `\copy-data` (`common::copy_bytes`); a binary COPY TO's messages compare as hex.
- Upserts as ORMs write them (ON CONFLICT on the key, a unique column, a constraint name, a
  partial unique index with its WHERE; DO NOTHING / DO UPDATE with EXCLUDED, a WHERE, an
  optimistic version, a sub-select; several rows, one twice; RETURNING with old / new);
  data-modifying CTEs feeding each other; MATERIALIZED / NOT MATERIALIZED CTEs; recursive CTEs
  with SEARCH / CYCLE; window functions as reports use them; JSON building and paths
  (jsonb_build_object / jsonb_agg with FILTER and ORDER BY, row_to_json, jsonb_set, `#-`,
  jsonb_to_recordset, jsonb_populate_record, jsonb_path_query_first); migrations between DML
  (columns added with defaults and NOT NULL, renamed, retyped with USING, dropped; constraints
  NOT VALID; identity restarts; indexes); ORM query shapes (keyset pages over row comparisons,
  OFFSET pages, EXISTS, LATERAL top-N, DISTINCT ON, a wrapped count, `= ANY` lookups, ILIKE);
  built-in functions with OUT parameters read as `(f()).x`, `(f()).*` and in FROM.

## Round 6 (agent 118)

Batch 1 (`a118_cursor_plan`, `a118_round6_fixes`, `a118_error_order`, `a118_round6_b`):

- **Cursor plans (round 5's open item)**: a DECLAREd query is planned for cursor_tuple_fraction
  (CURSOR_OPT_FAST_PLAN) and without parallel workers (no CURSOR_OPT_PARALLEL_OK): the binder
  plans its level with the fraction as a `lim` (`paths::cursor_limit`: a LIMIT count that isn't
  constant, minus the fraction, which no Limit node scales; `lim_fraction`), which reaches the
  scan choice, the join search (`limit_order`, through a WHERE over the joins) and the merge
  append choice; EXPLAIN's Sort over a presorted alternative compares the fractional costs at
  the query's final node (`explain::final_cheaper`). Whether a cursor declared without SCROLL /
  NO SCROLL can fetch backward, and which tables WHERE CURRENT OF finds a row of in a cursor
  without FOR UPDATE, are read off the plan EXPLAIN shows (`explain::cursor_plan`:
  ExecSupportsBackwardScan with the index's access method, search_plan_tree through Result,
  Limit, Subquery Scan and Append) when the logical plan can't say (a sort, an Append:
  `cursor::plan_decides`), else off the planned scans (a bitmap scan, a GiST index scan can't
  run backward). A cursor over a UNION ALL is keyed (`keyed_plan`). EXPLAIN DECLARE shows the
  cursor's plan. An ordered Append's startup is every member's (cost_append in PostgreSQL 18;
  the engine took the first member's), in the planner and EXPLAIN alike.
- **Wrong rows (GROUP BY, seeds 5276, 5299, 5375, 5377)**: a GroupAggregate shows each group's
  key as its first row in the sort's order (pg_qsort over all the rows): keys whose equal
  values look different (`1 mon` / `30 days`, 1.0 / 1.00, -0 / 0, a nondeterministic collation)
  are presorted as order-sensitive aggregates are (`agg::equal_apart`).
- **Wrong state (virtual generated columns, seed 5309)**: RETURNING and a trigger's WHEN read a
  virtual column as its expression over the row (`bind_returning`, `bind_trigger_when_as`), and
  a CHECK or NOT NULL reading one makes its error the statement's (`dml::generated_value`): an
  `INSERT ... RETURNING *` whose virtual column overflows fails instead of storing the row.
- **Wrong rows (NOT NULL NOT VALID, seed 5319)**: only a validated NOT NULL proves a column
  non-null to the planner (`Table::known_not_null`, PostgreSQL 18's attnullability): `WHERE doc
  IS NULL` was dropped as always false.
- **Wrong state (record comparisons, seeds 5290, 5325)**: record_eq / record_cmp check a
  column's types, operator and collations only as they reach it (`ops::check_record_columns`):
  `OLD.* IS DISTINCT FROM NEW.*` over a row with a json column after a differing one no longer
  fails "could not identify an equality operator for type json".
- **Engine errors**: a DISTINCT aggregate's ORDER BY sub-select matches its argument as equal()
  does (`same_query_as`, seeds 5265, 5268); AND / OR with constant arguments simplify as
  simplify_and_arguments does, BETWEEN's too, and IN lists / `= ANY` / `<> ALL` of constants
  fold (`fold_in_list`), so a FULL JOIN ON `1 IN (2, 3)` or a CASE over `1 BETWEEN 0 AND 2` is a
  constant (seeds 5298, 5359); a FOR UPDATE pushed into a subquery or view isn't applied to its
  CTEs (markQueryForLocking skips RTE_CTE), and a level it marks nothing in (only CTE
  references) isn't checked for aggregates (the planner's CheckSelectLocking needs row marks).
- **Error order (round 5's open items)**: COPY FROM a row at a time inserts the rows before an
  unterminated CSV quoted field (`split_csv` returns them with the error) and before any later
  record's parse error read in the same batch (`next_batch`: rows still pending, not only the
  parse's own); buffered COPY's NOT NULL row fails before the rows buffered ahead of it reach
  their unique checks; INSERT into a view with fewer values than columns named gives parse
  analysis's 42601 before the rewriter's 0A000; ALTER COLUMN TYPE fails on an index whose
  access method has no default operator class for the new type before converting any row
  (42704 before 22P02); COPY resolves its relation and column list (DoCopy) and COPY TO refuses
  a view, a partitioned or a foreign table (BeginCopyTo) before the options are processed
  (`copy_relation_error`).
- **Binary COPY**: data after the trailer fails "received copy data after EOF marker"; numeric
  ±Infinity are sent with PostgreSQL's display scale 32 (numeric_send).
- **Statistics after a rewrite**: an ALTER TABLE that rewrites an indexed table measures it as
  reindex_relation's index_update_stats does (`reindexed`), so pg_class has its tuples and pages
  where it had -1 / 0, and plans after it match (seed 6008: an index scan PostgreSQL wouldn't
  pick).
- From the round 6 grammar's first run: a cursor with a LIMIT of its own is planned for its
  tuple fraction of the Limit's paths (standard_planner's get_cheapest_fractional_path takes
  the caller's fraction, whatever preprocess_limit made of the LIMIT below:
  `paths::FINAL_FRACTION`), so `ORDER BY id LIMIT 4` reads the key's index as PostgreSQL's
  does and WHERE CURRENT OF works over it; a cursor whose FETCH failed (rolled back to a
  savepoint) refuses WHERE CURRENT OF as "held from a previous transaction" (execCurrentOf finds
  no QueryDesc); factorial of a negative number is 22003; a DISTINCT aggregate's ORDER BY
  matches a call over a sub-select (`abs((SELECT 1))`) to its argument.
- Harness: binary COPY in and out (above); the embedded API returns a binary COPY TO's messages
  as bytea, not lossy text. `pk_clustered`'s layout parity runs its UPDATE ... RETURNING after
  the SET LOGGED rewrite in key order: with the table measured after the rewrite (as
  PostgreSQL's reindex leaves it) the UPDATE scans the heap, whose order is the layout's.

Batch 2 (`a118_round6_d`), agent 84's extract() item at its source: a call written in SQL
syntax (EXTRACT, SUBSTRING, POSITION, TRIM, OVERLAY, ...) is bound wrapped as `Expr::SqlSyntax`
(for its display), which the strictness analyses didn't look through, so a WHERE over one never
reduced an outer join (seeds 4005, 4014, 4045, 4047: FULL JOIN errors; LEFT JOINs kept where
PostgreSQL has inner joins). `reducejoins::value_cols` / `nonnull_cols`, `outerjoins::
strict_items` and `nulls_with_row` see through it: in PostgreSQL it is the same FuncExpr.

Memory (agent 104's 362 / 317 MB): not reproduced. Same seeds 5106–5255, the engine of
534eef2b^ with cdb8d269's harness against cdb8d269, two runs each: peak 577 / 612 MB (base)
and 598 / 606 MB (head), final RSS 194–222 MB on both, the same binary varying by up to 35 MB
between runs (the test binary uses the system allocator; what is left at the end is mostly
freed memory it kept). Neither commit adds retained state (no new caches, per-session or
per-plan structures).

Pair run, fresh seeds 5406–5555 (round 5 grammar, 150 statements each) against the base
(d0c965fb): wrong_rows 51 → 9, wrong_error 41 → 17, eval_error 41 → 29, missing_error 19 →
18, tag 2 → 1; 79 mismatches fewer, none new. Peak RSS of the run 200.6 / 201.1 MB (base /
head, debug). Smoke: no new mismatch, the known list unchanged. The round 6 grammar's first
100 seeds (6000–6099), 15,445 statements: before the fixes from its own runs wrong_rows 5,
wrong_state 1, engine_error 37, missing_error 10, wrong_error 6, eval_error 7; after them
wrong_rows 2, engine_error 2, missing_error 4, wrong_error 6, eval_error 4 (Open below).
Isolation 121 / 122 (1,647 / 1,648). Regression suite in full on a private reference (debug):
51,148 / 51,150, no unit changed against results.tsv. Crate tests debug and release.

Cost: a DECLARE whose plan has a sort or an Append on top builds EXPLAIN's tree once
(`cursor_plan`); everything else is planning-time only (one more fraction test at the
fractional-cost sites, a NOT VALID scan of the constraints per not-null test). pgbench, release
builds alternated three times (4 clients, 20 s each, server CPUs pinned, a busy machine: load
6–13): select-only and TPC-B within the run-to-run noise (select-only spread 5,300–10,700 tps on
the base alone); a cursor script (two DECLAREs, one ORDER BY over an unindexed column, FETCH,
CLOSE) 1,256 / 1,160 tps median (base / head), inside the spread of either (1,012–1,322);
engine peak RSS 399–409 MB on both. If the DECLARE cost shows on a quiet machine, the plan
can be built only when a backward fetch, `pg_cursors` or WHERE CURRENT OF needs it (at the
risk of a plan made from statistics newer than the DECLARE's).

## Round 5 grammar (agent 104)

Seeds from `R5_SEEDS` (5000) on add the `r5*` objects (`round5.rs`, its own generator) and make
about a third of the statements `round5_stmt`'s; every seed also ends with its **final state**
compared: the list of tables, views and sequences, and every table's contents
(`Runner::final_state`, class `wrong_state`). The objects: `r5a` (int, text under one of 13
collations: C, POSIX, ucs_basic, pg_c_utf8, unicode, ICU locales and three created ones, two
of them nondeterministic; huge / tiny / NaN / infinite numerics; intervals at their limits;
int4range and int4multirange; documents and a stored tsvector with a GIN index; an array of
the composite `r5ct`; stored and virtual generated columns), with BEFORE / AFTER row triggers
(WHEN on columns and on the whole row) and statement triggers over transition tables logging
into `r5log`; updatable views over it with LOCAL / CASCADED check options, one over the other,
a view with a computed column and an ALSO rule, a join view with INSTEAD OF triggers; a table
`r5r` with conditional INSTEAD / ALSO / NOTHING rules; `r5p` range-partitioned (a list
sub-partition, a default partition or not) and `r5px` to attach. The statements:

- DDL between DML, each followed by the table's contents and its catalog description (columns
  with types, NOT NULL, generated kind, default expression, collation; constraints with
  `convalidated` and their definitions): ALTER COLUMN TYPE (with and without USING, over
  generated columns, indexes and views), SET / DROP DEFAULT, ADD COLUMN (defaults, NOT NULL,
  stored and virtual generated), DROP COLUMN, SET EXPRESSION / DROP EXPRESSION, CHECK and
  FOREIGN KEY constraints NOT VALID then VALIDATE, NOT NULL (PostgreSQL 18's `NOT NULL ...
  NOT VALID` too), UNIQUE [NULLS NOT DISTINCT] under the column's collation; partitions
  attached (rows inside or outside the bound, a default partition holding them), detached,
  created over existing rows.
- DML through the triggers, views, rules and partitions: INSERT (ON CONFLICT DO UPDATE ...
  WHERE), UPDATE (FROM, of composite array elements' fields, of generated columns), DELETE,
  MERGE with RETURNING old / new, row movement; the log's contents after each.
- Transactions with cursors (SCROLL / NO SCROLL / WITH HOLD, FOR UPDATE with WHERE CURRENT OF)
  across savepoints rolled back or released, errors inside, COMMIT / ROLLBACK / COMMIT AND
  CHAIN, `pg_cursors`.
- PREPARE / EXECUTE with parameters of 44 types (with typmods, arrays, ranges, composites,
  network, bit, money, text search, reg* types) and values valid, NULL, out of range or
  malformed; inferred parameter types; a statement kept across later DDL; DML.
- COPY TO (text / csv, HEADER, delimiters, NULL markers, quoting, FORCE_QUOTE, FORCE_NOT_NULL,
  a query, generated columns, a partitioned table, a view) and COPY FROM (good and bad lines,
  NULL markers, DEFAULT, ON_ERROR ignore with LOG_VERBOSITY and REJECT_LIMIT, through the
  triggers). Case files write COPY data as psql scripts do, so `probe` / `reduce` read them.
- Collations in ORDER BY, GROUP BY, DISTINCT, joins, set operations, LIKE / ILIKE / regular
  expressions, case mapping and window functions (written so that which of several equal
  values a nondeterministic collation keeps doesn't show); full text search (to_tsvector in
  four configurations, every tsquery parser, `@@`, ranking with weights and normalizations,
  headlines with options, ts_stat, ts_debug, tsquery operators); range and multirange
  operators, functions and aggregates; arrays of composites (field access, unnest, containment,
  JSON); numerics up to 10^1000 and intervals at their limits (arithmetic, aggregates, extract,
  justify, to_char, date_bin).

Backward fetches and WHERE CURRENT OF go only to cursors that say whether they may (SCROLL /
NO SCROLL, FOR UPDATE): otherwise PostgreSQL decides from the cursor's plan, made for
cursor_tuple_fraction (Open below).

## Round 5 (agent 104)

Batch 1 (`a104_*` goldens), agent 84's wrong results first, then the round 5 grammar's:

- **Wrong rows (seed 3563 #88, in the base too)**: array searches and containment ignored the
  call's input collation: under a nondeterministic one `'{z}' @> ARRAY['Z']`, `<@`, `&&`,
  array_position(s), array_remove and array_replace now compare as the element type's `=`
  under it (`collation::sensitive`, `array.rs` `same`), so a RIGHT JOIN ON such a containment
  kept its matches (`a104_array_collation`).
- **Missing error / wrong state (seed 3570 #67, in the base too)**: a constant call under a
  COLLATE (`starts_with('é' COLLATE ci, 'a')`) wasn't seen as foldable by the planning-time
  error pass (`fold::foldable`), and a CASE folded to one of its arms lost the collation the
  whole CASE derives (`bind_case`: `note_coll_of`), so "nondeterministic collations are not
  supported for substring searches" never came over an empty table, and the MERGE went on
  (`a104_fold_collate`).
- **Wrong rows (IN lists)**: `b IN ('E', 'A' COLLATE "de-x-icu")` compared each value under its
  own collation; transformAExprIn makes the values without the level's columns one array, whose
  comparison derives one collation from the left side and all of them (`a104_in_collation`).
- **Missing error (agent 84's open item)**: a one-value IN is `x = v` (NOT IN `x <> v`) as
  make_op resolves it, so `ARRAY[d] IN ('{1}'::int4[])` over a domain finds no operator as
  `=` doesn't (`a104_in_single`).
- **Wrong state (INSTEAD OF triggers)**: a trigger's WHEN was bound in the scope of the
  statement firing it, so from a PL/pgSQL trigger function (an INSTEAD OF trigger's INSERT ...
  ON CONFLICT DO UPDATE) `OLD.a` was "ambiguous" with the function's OLD; it binds behind a
  barrier now (`a104_trigger_when_plpgsql`).
- **Engine error / wrong state (DROP COLUMN)**: a trigger whose WHEN reads the whole row
  (`OLD.* IS DISTINCT FROM NEW.*`) depended on every column, so DROP COLUMN wanted CASCADE;
  a whole-row reference depends on no column (`a104_trigger_whole_row_dep`).
- **Wrong rows (prepared statements)**: PREPARE's parameter types keep only the type
  (PrepareQuery): `numeric(5,2)`, `varchar(3)`, `char(2)`, `bit(4)`, `interval(0)` parameters
  took their values through the typmod (`a104_prepare_typmod`).
- **Wrong rows (COPY TO csv)**: an empty string was quoted whenever the NULL marker wasn't
  empty; CopyAttributeOutCSV quotes it only when it is the marker (`a104_copy_csv`). Rows
  skipped by ON_ERROR ignore before a record that fails give their LOG_VERBOSITY notices
  (`a104_copy_notice`).
- **Wrong rows (JSON of composite arrays)**: `to_jsonb(cs)` / `to_json(cs)` of a stored array
  of a composite named the fields f1, f2: the element type is the declared composite
  (`json.rs` `elem_type`, `a104_composite_array_json`).
- **Eval errors (virtual generated columns)**: ADD COLUMN ... VIRTUAL and SET EXPRESSION on a
  virtual column evaluated the expression over every row (a division by zero failed the ALTER,
  and the column was never added); it is computed when read (`a104_virtual_add`).
- **Eval error (interval aggregates)**: sum / avg of intervals add the finite values apart
  from the infinities, as IntervalAggState does: an overflow there fails next to an infinity,
  infinities of both signs fail, avg divides (interval_div) (`a104_interval_agg`).
- **Crash (debug builds)**: to_char of an interval of 178,000,000 years overflowed the day of
  year (`(year * 12 + mon) * 30 + mday`); Postgres's int arithmetic wraps (-fwrapv)
  (`a104_interval_to_char`).
- **Wrong errors (error order)**: a view's WITH CHECK OPTION is checked after the row's unique
  keys (ExecInsert / ExecUpdate's WCO_VIEW_CHECK comes after the index entries:
  `store_inserted`, `update_row_rls`), so a duplicate key wins (`a104_wco_order`); ALTER COLUMN
  TYPE checks the conversion before what uses the column (a generated column, a view), and
  USING on a generated column is 42611 (`a104_alter_type_order`).
- Harness: COPY TO STDOUT of no rows is still a COPY out on the engine side (column `copy`).

Verified (0f9f0609 + the batch, debug): crate tests, clippy (no new warnings), the smoke
(no new mismatch; the known list unchanged), the pair run above, isolation 121 / 122
(1,647 / 1,648), the regression suite in full on a private reference: 51,148 / 51,150, no unit
changed against results.tsv. Cost: the per-row changes are an `Option` test per array element
compared, the infinities' counters of interval sums, and the views' check options moved after
the write (no work without views). Release builds side by side over 2M rows (medians of 7
alternated runs, load about 22): `ta @> ARRAY['a7']` 1,412 / 1,371 ms (base / head), `&&` 1,403
/ 1,458, `sum(array_position(..))` 1,167 / 1,035, `array_remove` 1,503 / 1,594, `sum(iv),
avg(iv)` 729 / 680, COPY TO csv 1,597 / 1,528, an UPDATE of 500k rows 6,112 / 5,898: within the
run-to-run noise.

Batch 2 (`a104_copy_single`): COPY FROM into a table with BEFORE ROW triggers or volatile
defaults (CopyFrom's CIM_SINGLE, `!copy.multi`) inserts each row as it is read, so an earlier
row's unique violation, or its trigger's notice, comes before a later record's format error
(seeds 5021, 5058): a record failing to parse keeps its error (`CopyReader::failed`) until the
records before it in the chunk have gone in. Buffered COPYs (no such trigger) still fail on the
record first, as PostgreSQL's multi-insert buffer does. Cost: none per row (one `Option` per
batch). Verified: crate tests (wire_waits' transaction_timeout transcript failed once on the
client's wording for a closed connection and passed alone), clippy (no new warnings), the
pair run above, regress files copy, copy2, copydml, copyselect, copyencoding, triggers and
updatable_views (2,757 units, none changed against results.tsv).

## Round 4 grammar (agent 84)

Seeds from `R4_SEEDS` (4000) on add two tables (`round4.rs`, its own generator, so earlier
seeds keep their schemas): `tj4 (id, j jsonb, js json, ts timestamptz, t timestamp, d date, iv
interval, n numeric, f float8, k int)` with JSON documents of every shape (arrays, nested
objects, scalars, JSON null, numeric strings, date strings, duplicate keys in `js`), DST
boundaries, infinities and BC values, infinite and mixed-sign intervals, numeric NaN /
Infinity / huge and tiny values, float -0 / NaN / Infinity; and `tp4 (k, d, v, n)` range-
partitioned on k with a sub-partition list-partitioned on v, a default partition at both
levels and a unique index on (k, v). A quarter of the statements are then `round4_stmt`'s:
jsonpath functions and operators (`@?`, `@@`, `_tz` forms, silent, vars, filters, item
methods `.double()` / `.boolean()` / `.keyvalue()` / `.datetime()` / `.decimal()` ...,
`jsonb_path_query` in LATERAL WITH ORDINALITY); JSON_VALUE / JSON_QUERY / JSON_EXISTS with
RETURNING, wrappers, QUOTES and ON EMPTY / ON ERROR; JSON_TABLE with NESTED PATH and
ordinality; JSON_ARRAYAGG / JSON_OBJECTAGG / JSON_OBJECT / JSON_ARRAY / JSON_SERIALIZE /
JSON_SCALAR / JSON(), IS JSON, jsonb_set / insert / strip_nulls / `#-`; window frames (ROWS,
GROUPS, RANGE with numeric / interval / float offsets, every EXCLUDE); LATERAL set-returning
functions (jsonb_array_elements / each / to_recordset, generate_series over timestamps and
dates, multi-argument unnest, ROWS FROM, regexp_matches, string_to_table) and SRFs in the
select list; MERGE (NOT MATCHED BY SOURCE, DO NOTHING) and UPDATE / INSERT ... ON CONFLICT /
DELETE with RETURNING old / new and tableoid across partitions (row movement into default
partitions); partition-pruned queries; numeric edge functions (round / trunc with negative
scales, scale / min_scale / trim_scale, power, ln, to_char, width_bucket, factorial, gcd);
and timestamp / interval arithmetic under a `SET TIME ZONE` (DST, AT TIME ZONE, date_trunc
with a zone, date_bin, age, justify_*, make_timestamptz). JSON_VALUE never runs RETURNING json
/ jsonb or over NULL inputs (PostgreSQL 18.6's carried-over defaults, Open below).

## Round 4 (agent 84)

Batch 1 (`a84_fresh_r4`), round 3's leftovers:

- **Eval errors (seeds 2133 #66, 2184 #1)**: a sub-select is planned while its expression is
  preprocessed (make_subplan), whatever becomes of the relation reading it later, so its
  failing constants fail the statement even on the nullable side of `LEFT JOIN ... ON false`
  (`fold::push_candidates`: an empty relation keeps its sub-selects' failing constants); a
  constant-false WHERE over an outer join gates the whole join (the top join domain's
  pseudoconstant), where the engine pushed it into the outer side and so never planned a
  subquery there (`optimize::add_filter`).
- **Missing errors (seed 3045 #47 and kin)**: check_generic_type_consistency over domains:
  an `anyelement` argument of a domain stays the domain while `anyarray` / `anyrange` ones
  are flattened, so `int4range @> d`, `d <@ int4range`, `int[] @> ARRAY[d]`, `int[] = d[]`
  (NULLIF, IN) and `text[] <@ d3text[]` find no operator (`Binder::poly_domain_conflict`).
- **Wrong columns (seed 3202 #58)**: a scalar sub-select of a domain column, or of
  `array_agg` of one, and ARRAY() of one, have the domain's (array) type
  (`Binder::sub_domains`).
- **Missing errors (record_cmp)**: two row types whose field declares different collations
  (`ROW(x COLLATE "C", 1)` against `ROW(x, 1)`, through subqueries, IN, IS DISTINCT FROM)
  have no collation for it, and comparing two values of it fails with 42P22; NULLs and
  fields after a difference never compare it (`ops::check_record_collations`).
- **Missing errors (coerce_record_to_complex)**: `max(c)::ct`, `r::ct` of a subquery's
  record column, `(SELECT ROW(...))::ct`, `INSERT ... SELECT ROW(...)` and `UPDATE ... SET c =
  (SELECT ROW(...))` fail "cannot cast type record to ct" at PostgreSQL's positions; only a
  ROW() or a whole-row reference converts (routine results and PL/pgSQL values convert as
  before).
- **Eval errors (folding after pull-up)**: an immutable expression a pulled-up subquery's
  constant output was put into after binding is folded whole (`fold::unfolded`), also inside
  a sub-select reading it (`Cx::fold_outer`: seed 1926 #43's ON EXISTS over a UNION); an
  empty join's dummy outputs keep the failing constants of the columns read above it,
  through the projections kept over it (`analyzejoins::dummy_with`, seed 1926 #96); a
  sub-select in an empty relation's output read above it (smoke s10 #33).

Pair run, fresh seeds 3330–3479 against the base: 47 mismatches fewer (table above), none new;
smoke: 4 known eval_errors gone (s10 #33, s20 #42, s22 #62, s25 #79; `smoke_known.txt` re-recorded,
14 entries after the rebase). Regression suite: full runs of 79067aa2 and, rebased, 45b30b44
(debug, private references), 51,148 / 51,150, no unit changed against results.tsv.

Batch 2 (`a84_fresh_r4b`), from the round 4 grammar's seeds 4000–4099 (the first run: wrong_rows
24 in 30 seeds, nearly all jsonpath):

- **Wrong rows (jsonpath, depth first)**: a path is evaluated item by item to its end, as
  executeItemUnwrapTargetArray does, so what an unwrapped array gave before an error goes
  through the rest of the path, and a silenced error keeps it (`$.a.boolean()` over `["1",
  "a"]`: `[true]`); lax `jsonb_path_exists` / `@?` / JSON_EXISTS stop at the first item found
  (an error past it never happens: `t`, where the engine said NULL / false); silent
  `jsonb_path_match` / `@@` judge the items found before an error.
- **Wrong rows (`.double()`)**: of a numeric it keeps the numeric (only checked to be a
  double): `-0.0` stays `0.0`, 30 digits stay 30 digits.
- **Engine error (SQL/JSON and time zones)**: JSON_VALUE / JSON_QUERY / JSON_EXISTS /
  JSON_TABLE run their paths with time zones (JsonPathQuery's `useTz`), so `.timestamp()` of a
  string with an offset converts instead of failing "cannot convert value from timestamptz to
  timestamp without time zone usage".
- **Wrong rows (sum of -0)**: sum(float8) / sum(float4) start from the first value (float8pl),
  so only -0s sum to -0; Youngs-Cramer's Sx starts from 0.
- **Wrong rows (outer join reduction)**: `NOT (x < ANY ('{}'))` is true for a NULL x, so it
  doesn't make x's relation non-nullable: ANY counts only at the top level, where false is as
  good as NULL, else an array known to have elements (is_strict_saop's falseOK); the engine
  turned the LEFT JOIN under it into an inner join and lost its rows.
- **Missing errors / wrong columns**: NULLIF's `=` over an array of a domain against the base
  type's array finds no operator (seed 4012 #0), and NULLIF has its first argument's domain.
- **Wrong error (error order)**: the errors of planning a level (a FULL JOIN no merge or hash
  join can run) wait for the whole statement's parse analysis, whose errors (an ungrouped
  column) come first (seeds 4000 #33, 4009 #37, 4040 #39 and kin: 0A000 for 42803).
- **Crash (found by the crate's deep_input test after the rebase)**: `optimize::
  strip_free_domains` unwrapped a transform that fails past the statement's stack budget; it
  now leaves a qual without domain coercions alone (no rebuilt tree at all) and stops at the
  budget, so the next guard raises "stack depth limit exceeded".
- **Crash (stack overflow, the pair run's seed 3534 #11, in the base too)**: a join group over
  an inheritance parent (a view joining it to `unnest ... WITH ORDINALITY`, FULL JOIN, a
  grouped CTE over the same view) was retried partitionwise (`joinpath.rs`, the forcing
  `pw_agg_join`), gave the same plain plan, and was retried again on a clone the `lifting`
  guard didn't recognise (a nested level's grouping had reset the forcing), until the stack
  overflowed and aborted the process. Retries now nest at most four deep
  (`Planner::pw_retry`). `DIFFTEST_ECHO=1` writes each statement about to run as a case file
  (`current-s<seed>.sql`), which is how a crash that takes the process down is caught.

Cost: none of this runs per row except the record comparisons' collation check (only for two
anonymous row types whose field collations differ) and IN lists / IS DISTINCT FROM over
anonymous records; the binder's new checks run only for calls with two polymorphic
parameters, and the constant-tree scan is one linear pass per statement. Measured side by
side against the base (release builds, load 20–45, medians of 10 alternated runs over 2M
rows): sum / avg of float8 454 / 463 ms, IS DISTINCT FROM 830 / 827, IN over named records
3,570 / 3,681, `=` over named records 2,014 / 2,045; point selects with IN and IS DISTINCT
FROM over the simple protocol within the run-to-run noise.


## Round 3 grammar (agent 75)

Seeds from `R3_SEEDS` (3000) on add to the schema: a nondeterministic collation `ci3` among
the text columns' collations; domains (`d3int` with a CHECK, `d3text` with a collation) as
column types; a generated column (`g`, STORED or VIRTUAL) over an integer one, which DML
doesn't write; NULL-heavy data (a NULL share of 15%, 40% or 70% per table); triggers on one
keyed table (a BEFORE row trigger skipping some rows and filling NULLs, AFTER row triggers
and an AFTER statement trigger over its transition table, all logging into `audit3`, whose
contents are compared after each DML statement on that table); and a composite type `ct3 (a
text COLLATE <C | en-x-icu | und-x-icu | ci3 | default>, b int)` with its table `tc3 (id, c
ct3, ca ct3[], n)` (NULL values and NULL fields), btree indexes on `c`, `(c DESC NULLS LAST,
n)`, `ca`, a unique one on `c`, and the range type `ctr3` over it. 12% of the statements are
then `composite_stmt`'s: comparisons, ORDER BY in every direction, GROUP BY, DISTINCT, DISTINCT
ON, min / max, count(DISTINCT), window ORDER BY and PARTITION BY, self-joins, UNION, IN / ANY,
GREATEST, row comparisons of fields, anonymous ROW() sort keys, arrays and ranges of `ct3`, a
correlated min, and UPDATE / INSERT ... ON CONFLICT / DELETE ... RETURNING over `tc3`. (DML
with RETURNING, MERGE, ON CONFLICT, data-modifying and recursive CTEs, window frames,
partitioned and inherited tables were there already.)

## Round 2 (agent 68)

Batch 1 (`a68_lateral_postpone`):

- **Wrong rows**: a clause of a LATERAL subquery pulled up whose FROM is a join (its WHERE, or
  an inner join's ON inside it) that reads only the left side's columns was distributed as a
  restriction of the left side: `SELECT * FROM a LEFT JOIN LATERAL (SELECT 1 FROM b, d WHERE
  a.x > 7) s ON true` lost a's rows failing it, where they should be null-extended (seed 1604
  #73: 6 rows, PostgreSQL 30; the clause read the nullable side of a lower outer join, so it
  became that join's Filter). PostgreSQL postpones a clause with Vars outside its syntactic
  scope to the nearest join level above whose scope has them all (distribute_qual_to_rels'
  lateral_clauses), which under a LEFT JOIN LATERAL is that outer join: its Join Filter.
  `outerjoins.rs` `postpone_lateral_clauses` does that for the clauses of inner joins and
  filters inside a pulled-up LATERAL subquery (`RawJoin::framed`), before the group
  distributes them. EXPLAIN still differs in one detail: under `ON false` PostgreSQL also
  gates the nullable side with `One-Time Filter: false`.
- Harness: the regress cleanup drops only the databases its own run's statements created
  (HANDOFF, "How to test").

Batch 2, from seeds 1740–1889 (`a68_groupingsets_folded`, `a68_fold_empty_subquery`,
`a68_strict_args`, `a68_distinct_equal_values`, `a68_full_join_distinct`):

- **Wrong rows (grouping sets, folding)**: a GROUP BY item without columns (`abs(9)`,
  `position('bar' in 'foo')`, `1 + 1`, a typed literal `('2025-06-19'::date)`) that occurs inside a select-list
  item or HAVING is a value of the group's rows there, NULL in the empty set's row over no
  input; the binder folded it into its surroundings first (`abs(9) * 2` became `18`, never
  NULL). While a grouping-set query's select list and HAVING are bound, such an occurrence
  is now kept apart (`SelectState::pass_marks`, marked `SqlSyntax(SqlSyntax(_))` so nothing
  folds through it) and `post_agg` makes it the pass-through value (seed 1830 #9). Agent 45's
  open item ("grouping-key matching of folded expressions") is this.
- **Wrong rows (DISTINCT, equal values)**: a plain DISTINCT under an ORDER BY that EXPLAIN shows
  as a Unique over a Sort kept the first row of each group it read, not the first in the
  sort's order, which differs for values that compare equal but differ (`'1 mon'` /
  `'30 days'`): seed 1777 #34. Such a DISTINCT now sorts all its rows first, on the ORDER BY's
  keys and then its other columns (`exec/nodes.rs` `distinct_presort`), when its column
  types allow such ties; `sort_rows_by` is Postgres's quicksort instead of a stable sort.
- **Wrong state (errors PostgreSQL raises)**: a strict function's arguments are all evaluated
  before a NULL one makes the call NULL (EEOP_FUNCEXPR_STRICT), so `(b #> '{a,b}') || (b -
  'a')` over a JSON `null` fails with "cannot delete from scalar" as in PostgreSQL instead of
  updating the row to NULL (seed 1832 #4; `sql/expr.rs`, `exec/kernel.rs`).
- **Wrong state (errors PostgreSQL raises)**: a FROM subquery found empty under a level found
  empty too dropped the constants of its outputs that failed to fold, so `INSERT ... SELECT
  ... FROM v WHERE NULL GROUP BY CUBE (v.c3, 1)` (c3 `jsonb_array_length('{...}')`) inserted
  a row where PostgreSQL fails (seed 1827 #39). An empty relation now keeps, per output column,
  the failing constants a reader of it would have simplified (`DummyInfo::kept_cols`), kept
  outright once the statement's whole plan shows the column read (`settle_kept_cols`, before
  `fold::first_error`: a level planned apart doesn't know which of its outputs its parent
  reads, and an unread view output must not fail, as `SELECT c2 FROM v WHERE false`).
- **Wrong state (engine error)**: `x IS DISTINCT FROM y` over constants wasn't folded (a
  `FULL JOIN ... ON 1 IS DISTINCT FROM 1` failed "FULL JOIN is only supported with
  merge-joinable or hash-joinable join conditions"), and a WITH query nothing reads failed
  the statement with that planning error, where PostgreSQL never plans it (seed 1829 #97: a
  DELETE ... RETURNING whose sub-select has such a CTE). A plan-time error of a read-only WITH
  query's body is kept with it (`Binder::defer_plan`, `CteDef::deferred`) and raised where
  the query is read (or kept with the WITH query around that reference).

## Round 3 (agent 75)

Batch 1 (`a75_composite_collation`, `tests/locale/composite.sql`, `tests/index_key_format.rs`
`composite_index_keys_are_upgraded`), agent 67's leftover:

- **Wrong rows (composites with collated fields)**: records compared by their bytes
  everywhere: `ROW('a', 1)::ct < ROW('B', 1)::ct` with `ct (a text COLLATE "en-x-icu", b
  int)` was false (PostgreSQL's record_cmp compares each field under its attcollation:
  true), `WHERE c >= ROW('Z', 0)::ct` counted 2 rows of 7 where PostgreSQL counts 1, and min /
  max, ORDER BY, window ORDER BY, GREATEST / LEAST, IN / ANY, arrays of composites, nested
  composites, domains and range types over them disagreed; in an ICU-default database every
  record with a text field did (its fields are under the default). Under a nondeterministic
  field collation, GROUP BY, DISTINCT, count(DISTINCT), UNION, window PARTITION BY, `IN
  (SELECT ...)` and unique indexes kept values PostgreSQL finds equal apart. A record's
  comparison is now a record collation (`Collation::record`: per field, the collation it
  compares under, nested for composite fields), derived from the type
  (`collation::for_type`: a CREATE TYPE's COLLATE, a table row type's column collations, an
  anonymous ROW()'s field collations, which `RowType::colls` now carries as PostgreSQL's
  blessed row type does; the database default otherwise) and carried as any collation is:
  with the comparison call (`record_compare_def`, also for sub-select comparisons), a sort
  key's `<`, min / max, window and grouping keys, set operations, range types over
  composites (`types/range.rs` `vcmp`) and index keys.
- **Wrong rows (field access)**: `(c).a` derived the default collation, not the field's
  (FieldSelect's resultcollid), so `(c).a < 'b'` and `((c).a, (c).b) < ('b', 0)` compared
  bytes (`binder::collation_at`, `collation::field_collation`).
- **Index key format 3**: a btree or hash index on a composite column (or an array or range of
  them) whose fields are under such collations keys each field by its collation's sort key
  (`keys::encode_collated`), so index scans, ORDER BY and Merge Append use it in record_cmp's
  order; indexes written before are rebuilt when a database is first opened
  (`Engine::reencoded_indexes`).
- **Wrong rows (min / max)**: `max(c)` of a composite was planned as an index scan under
  `c IS NOT NULL`, which tests the fields, so a value with a NULL field (`(,1)`, the largest)
  was skipped: PostgreSQL has no sort operator for record's min / max and aggregates them.
  Their result type is `record`, as PostgreSQL's (`(min(c)).a` fails).
- Build hazard: `crates/pg-icu`'s build fails when ICU4C can't be built (it used to warn and
  build without ICU, leaving every ICU collation wrong or unavailable); `PG_ICU_DISABLE=1`
  builds without it on purpose.

Found by the round 3 grammar and seeds 2040-2189 (`a75_fresh_r3`):

- **Wrong rows (self-join elimination over composites)**: a self-join on a unique composite
  column became one scan filtered by `c IS NOT NULL`, which tests the fields, so rows with a
  NULL field were lost (`x JOIN y ON x.c = y.c`); PostgreSQL's NullTest there is of the value
  (argisrow false, shown as `c IS DISTINCT FROM NULL`) (`analyzejoins::replace_selfjoin_qual`).
- **Wrong rows (presorted aggregate, nondeterministic collations)**: with a DISTINCT aggregate
  over a nondeterministic key the Agg's input was presorted by bytes, so `max(s)` / `min(s)` of
  `SELECT max(s), count(DISTINCT s)` kept another of two equal values than PostgreSQL, whose
  Sort below orders under the key's collation (ties stay in input order): text and
  composites alike.
- **Wrong rows (aggregate level)**: an aggregate whose arguments name only an outer query's
  columns belongs to that query, also when folding drops them (`count(1) FILTER (WHERE
  starts_with(NULL, o.c))` in a sub-select): the binder now notes the levels of the columns the
  arguments name as written (`Binder::agg_refs`), as parse analysis does (seed 2183 #73: an
  outer query over one row returned 0 for the sub-select instead of NULL; smoke's
  `missing_error s1 #34` went with it).
- **Wrong columns**: a scalar sub-select's name is its first column's resname, `?column?` when
  that figures none, so a CASE whose ELSE is one is named after it (seed 2187 #13).
- Seed 1472 #24 (an INSERT failing in the engine only): a SQL function PostgreSQL neither
  inlines nor runs while planning has its body planned only when it runs, so a failing
  constant in it fails only then (`fold.rs`: such a call's sub-plan isn't walked).
- Seed 1926 #33 (an error PostgreSQL raises): the constants an empty relation keeps for the
  readers of its columns are settled in the plans of sub-selects too (an EXISTS turned into an
  InitPlan read a pulled-up view's failing output in a join clause; `settle_kept_cols`).
- Seed 1926 #43 stays open: an EXISTS over a UNION whose arm reads a column of a relation found
  empty (`ON EXISTS (SELECT 1 UNION SELECT jsonb_array_length(s7.c2 -> 'a') FROM w WHERE
  false)`, `s7 ... WHERE false`) fails in PostgreSQL only when that relation is empty; which
  outputs PostgreSQL substitutes there before planning the sub-select needs its pull-up order.
  (Round 4: the variants over a relation that isn't empty are fixed; this one stays open.)

Left: two anonymous rows whose fields have different collations (`ROW('a'::text COLLATE "C",
1)` against `ROW('B'::text, 1)` in an ICU database) compare under the first's, where
PostgreSQL's record_cmp has no collation for the field and fails ("could not determine which
collation to use for string comparison") once it compares two values of it; `max(c)::ct`
(a record cast to a composite) isn't refused. (Both fixed in
round 4.)

Batch 2 (`a75_fresh_r3b`), from the round 3 seeds 3030-3329:

- **Wrong state (errors PostgreSQL never raises)**: `x [NOT] IN (subquery)` and `x op ANY /
  ALL (subquery)` over a subquery returning no rows evaluated `x` first, so a call failing in
  it failed a DELETE PostgreSQL completes (seed 3193 #75): ExecHashSubPlan returns before it
  projects the left side over an empty table, ExecScanSubPlan has no row to test it against
  (`exec/mod.rs`).
- **Wrong state (errors PostgreSQL never raises)**: a subquery planned apart (grouped,
  aggregated) restricted by a constant-false qual of its own level is excluded before it is
  planned (relation_excluded_by_constraints, in set_rel_size), so nothing in it is simplified;
  the engine pushed the qual into the subquery and kept its failing constants with the dummy
  (seeds 3233 #5, 3204 #45: an INSERT ... ON CONFLICT and an UPDATE whose SET sub-selects read
  such a view failed). The qual now stays over the subquery (`optimize::add_filter`) and the
  dummy keeps only its level's constants (`analyzejoins::without_subqueries`). Over a join a
  constant-false qual is the join's (distribute_qual_to_rels' pseudoconstant, gating the
  join): its relations are planned, and their subqueries' failing constants do fail.
- **Engine error**: an immutable call over constants under a COLLATE wasn't folded (`'a' = 'a'
  COLLATE "und-x-icu"`; eval_const_expressions makes such a CollateExpr a Const), so a FULL JOIN
  on it failed "FULL JOIN is only supported with merge-joinable or hash-joinable join
  conditions" where PostgreSQL's ON is a constant true (seed 3142 #91). Only for a result
  without a collation (a folded text value couldn't carry its explicit one on), and after the
  call's collations are derived, so explicit ones that conflict still fail first.
- **Wrong rows (grouping sets; agent 68's leftover)**: an untyped literal an operator makes a
  constant equal to a grouping key without columns is an occurrence of that key, as parse
  analysis matches the transformed expressions: `HAVING 1 > (10.00 - ('2025-06-19' -
  '1999-11-11'::date))` for a key `('2025-06-19'::date)` is NULL in the empty set's row over no
  input, where the engine folded the subtraction and kept the row
  (`a75_groupingsets_literal`; `Binder::pass_marked_literal`).

Left from the round 3 seeds: `int4range @> x` with `x` of a domain over int4 (seed 3045 #47:
PostgreSQL finds no operator, the engine resolves one); `array_agg` of a domain column (seed
3202 #58: PostgreSQL's column type is the domain's array type, the engine's int4[]); seed 3285
#94 is the physical row order after an UPDATE ... FROM a DISTINCT ON sub-select (below).
Seeds 2040-2189 against batch 1: 16 mismatches fewer (engine_error 39 -> 36, missing_error
24 -> 22, eval_error 98 -> 92, wrong_error 40 -> 39, tag 9 -> 7), two new eval_errors where the
engine now skips an evaluation PostgreSQL makes (seed 2133 #66 "cannot get array length of a
scalar", seed 2184 #1 "operand, lower bound, and upper bound cannot be NaN"), reduced and fixed
in round 4, as were the other two leftovers here (int4range @> d, array_agg of a domain).

## Open

- **Round 6 seeds, found, not fixed** (agent 118):
  - FULL JOIN checks run per query level as it is planned: a FULL JOIN in a FROM subquery that a
    strict WHERE above reduces after pull-up (`(SELECT f2.b c0 FROM f1 FULL JOIN f2 ON f1.a >
    f2.b) s WHERE s.c0 > 0`), or in a correlated sub-select whose outer join to it is removed,
    fails in the engine (0A000) where PostgreSQL first reduces it. Dropping the level's
    deferred error when the statement's final plan has no such join was tried and backed out:
    a sub-select PostgreSQL plans apart (and fails) can be gone from the engine's final plan
    (seed 5517 #133).
  - Executor laziness (round 5): `DELETE FROM t USING v` over an empty `t` computes the view's
    failing output (the hash join builds its inner side first; ExecHashJoin reads the outer
    side's first row first when its startup cost is below the hash's total: an empty outer side
    builds nothing); its statement trigger then never fires (seed 5069). Agent 112's area
    (`exec/nodes.rs` HashJoinNode, whose parallel probe pipeline a first-row peek would bypass).
  - EXPLAIN only (round 5): a comparison of a domain column shows `(d = 1)` where PostgreSQL
    shows the relabel to the base type, `((d)::integer = 1)`.
  - Cursor plans: EXPLAIN's other choices at the final node (a grouping hashed or sorted with
    no ORDER BY, a DISTINCT) still compare total costs, not the cursor's fraction; a plain
    `SELECT DISTINCT a ... ORDER BY a` is a HashAggregate under a Sort where PostgreSQL's
    add_path keeps the Unique over a Sort within its fuzz factor (`fuzzy_better`, the Plan
    choice item below). PL/pgSQL cursors (OPEN, explicit cursors) aren't planned for the
    fraction yet.
  - Built-in functions with OUT parameters: `(f()).x` works over a direct call; the engine also
    resolves it through a sub-select or VALUES column of type record, where PostgreSQL fails
    ("could not identify column ... in record data type", "record type has not been
    registered"); `pg_get_object_address('table', '{pg_catalog,pg_class}', '{}')` doesn't find
    the catalog; `pg_stat_get_backend_subxact` is missing.
  - A virtual generated column in a whole-row NULL test (`t.* IS NULL`) is computed; PostgreSQL
    splits the test per field and stops before it.
  - Reduced from seeds 6000–6099, not fixed: a CASE folded at bind time loses the explicit
    collation its arms derive, so a later conflict (42P21) isn't raised (`bind_case`,
    `note_coll_of` keeps it only for a constant); LEAST / GREATEST over constants aren't folded
    (`greatest(70000, 1)::int2` over an empty table, 22003 in PostgreSQL); a pulled-up
    subquery's constant outputs aren't folded again into the expressions above
    (`SELECT s.c2::int2 FROM (SELECT 2147483647 AS c2 FROM t2) s`); deferred folding errors
    come out in bind order, not subquery_planner's (the target list before the quals, only
    referenced outputs); a FULL JOIN whose clause failed to fold reports the FULL JOIN error
    instead of the folding error; PostgreSQL's merge join costing (mergejoinscansel over
    histograms) raises 42P22 for a join of columns under conflicting collations whatever join
    it picks, the engine only when it picks a merge join; an outer-level aggregate in a
    sub-select in WHERE gives a later column error first; float input of hex (`'0x10'::float8`,
    strtod) is refused; a JSON error's token after `-` is one character (`found "-"` for
    `"-13"`); binary COPY checks a stream's framing (data after the trailer) before a row
    before it fails NOT NULL; a FULL JOIN on a pulled-up subquery's constant output fails once
    a join above reads it (the output isn't a PlaceHolderVar there).

- **Round 4 seeds, reduced, not fixed** (cases in the round's notes above):
  - Errors PostgreSQL raises while planning that the engine never raises: a regular
    expression under a nondeterministic collation in a qual over an empty table (seed 4066
    #49: patternsel compiles it for its estimate); an implicit-collation conflict in a CASE's
    branches that only an ORDER BY over it needs (seed 4069 #40); a failing constant in a
    DML's sub-select under `HAVING NULL` (seed 4071 #80, under planner settings).
  - Executor laziness: a PL/pgSQL `RETURN QUERY` whose column types don't match fails in the
    engine where PostgreSQL never calls the function (the join's other side is empty, seed
    4059 #2); "could not determine which collation to use for string hashing" from a FULL
    JOIN on a domain column against a nondeterministic one, in a cursor (seed 4033 #102).
  - FOR NO KEY UPDATE over a join with a FULL JOIN subquery under `WHERE false` isn't refused
    (seed 4082 #55; the locked-nullable-side item below).

- **Wrong rows (JSON_VALUE / JSON_QUERY, a PostgreSQL 18.6 quirk)**: after a row whose call
  fell back to its ON ERROR (or, without an ON ERROR step, ON EMPTY) default, a row whose
  input is NULL returns that default too instead of NULL: the NULL-input step falls through
  to the ON ERROR check and `jsestate->error` still holds the previous row's (only the path
  step resets it). Seed 1801 #53 (`JSON_VALUE(t2.c, '$.a' RETURNING int DEFAULT -1 ON ERROR)`
  over a LEFT JOIN's null-extended row). Not emulated: it needs per-call-site state across
  rows, and the result depends on the row order, which differs between plans. A second one
  (round 4): JSON_VALUE ... RETURNING json / jsonb returns NULL for every row after one that
  fell back to its ON EMPTY default (`JSON_VALUE(j, '$.s' RETURNING jsonb)` over `{"n": 1}`,
  then `{"s": "abc"}`: NULL, where alone it is `"abc"`); not emulated either, and the round 4
  grammar avoids both.
- **Plan choice**: `SELECT DISTINCT a FROM t ORDER BY 1 DESC` is a HashAggregate under a Sort in
  the engine where PostgreSQL has a Unique over a Sort (ASC agrees), so which of two equal
  values (`'1 mon'` / `'30 days'`) is kept differs (planner costs).
- Constant folding: a qual over a pulled-up subquery's output whose constant fails, ANDed
  with NULL above a subquery found empty (`... t WHERE t.c3 > 0 AND NULL` with `c3`
  `jsonb_array_length('{}')` under `WHERE NULL`), doesn't fail the statement as in
  PostgreSQL. (A dummy projection kept over an empty relation passes on its input's
  per-column constants since round 4.)

- **Constant folding, left**: a UNION ALL arm found empty (`... WHERE false UNION ALL ...`)
  is dropped with its outputs, which PostgreSQL still simplifies; a strict call over a pulled-up subquery's output whose expression
  fails (`NULL + s.c` with `c` = `width_bucket('NaN', ...)`: PostgreSQL substitutes first,
  the binder folds `NULL + s.c` before any pull-up; smoke s12 #52); a CASE arm a pulled-up
  constant makes unreachable; a pulled-up constant output read by a scalar sub-select in the
  select list (substituted after binding, at planning, so `fold::unfolded` doesn't see it);
  seed 1926 #43 itself, whose relation is also found empty (its sub-select's failing
  constants come from the substitution, which the empty relation's kept constants don't
  model).

- Grouping keys' occurrences are matched by their text as written (`pass_marks`): one written
  differently but equal (`abs(+9)` for a key `abs(9)`) is still folded with its surroundings;
  PostgreSQL compares the transformed expressions. (An untyped literal an operator coerces
  matches the constant it becomes since round 3, batch 2.)
- EXPLAIN only: a single-table FROM subquery on the nullable side whose WHERE is a
  sub-select (`c LEFT JOIN (SELECT x FROM a WHERE (SELECT false)) s ON ...`): PostgreSQL
  checks it as the outer join's Join Filter, the engine gates the scan with a Result.
- **Wrong rows (row order)**: DISTINCT ON keeping another of two equal values
  (`'24:00:00'` / `'1 day'`) whose input order comes from a different join order (seed 176).
- wrong_state cases of the round 2 sweep not reduced yet (seeds 116, 179, 211, 216): DML
  after earlier statements; some are physical row order after UPDATE (below).
- A locked partitioned table without partitions on the nullable side of an outer join isn't
  refused (the check finds no locked scan under it).
- `ungrouped column` checks of sub-selects that fold away run after the level's own checks,
  so with two ungrouped columns the error can name another one than PostgreSQL's (and its
  position points at the column's first mention).
- Hash join sides chosen differently for tiny unanalysed tables, and a DISTINCT ON sub-select
  not kept as a Subquery Scan (`UPDATE t x ... FROM (SELECT DISTINCT ON (id) ...) s`): the
  rows' physical order after the UPDATE differs, which an `INSERT ... SELECT ... ON CONFLICT
  DO NOTHING` then shows (planner costs; agents 26/28/31/33's area).
- FULL JOIN error decisions where PostgreSQL plans an inner FULL JOIN under an `ON false` join
  (PostgreSQL errors, the engine doesn't), and dummy relations other than a constant-false
  WHERE directly over the FULL JOIN.
- Collation conflicts (42P21) PostgreSQL finds in branches the engine folds away, and the other
  way round (an implicit-collation conflict across a UNION with a NULL-collation column).
