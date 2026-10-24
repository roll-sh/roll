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
`DIFFTEST_HANG_SECS` (60), `DIFFTEST_SMOKE_SEEDS` (25), `DIFFTEST_PANIC_TRACE` (print panics).

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

`smoke_known.txt` lists the smoke run's remaining mismatches above `error_message` (62): the
smoke test fails only on one not in it.

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

## Open

- FROM sub-selects' unused outputs, unreferenced CTEs and `false AND ...` arguments: PostgreSQL
  drops them before it folds constants (left to right in AND / OR), the engine folds while
  binding and reports the error (`SELECT 1 FROM (SELECT 1 / 0 AS c FROM t) s`,
  `WITH w AS (SELECT 1 / 0) SELECT 1`, `WHERE false AND 1 / 0 = 1`: PostgreSQL returns rows).
  Most `eval_error` and `engine_error` "cannot convert NaN to integer" cases are this.
- A locked partitioned table without partitions on the nullable side of an outer join isn't
  refused (the check finds no locked scan under it).
- `ungrouped column` checks of sub-selects that fold away run after the level's own checks,
  so with two ungrouped columns the error can name another one than PostgreSQL's (and its
  position points at the column's first mention).
- Hash join sides chosen differently for tiny unanalysed tables, and a DISTINCT ON sub-select
  not kept as a Subquery Scan (`UPDATE t x ... FROM (SELECT DISTINCT ON (id) ...) s`): the
  rows' physical order after the UPDATE differs, which an `INSERT ... SELECT ... ON CONFLICT
  DO NOTHING` then shows (planner costs; agents 26/28/31/33's area).
- FULL JOIN error decisions where PostgreSQL makes the join dummy first (a constant-false
  WHERE at the join, `restriction_is_constant_false`) or plans an inner FULL JOIN under an
  `ON false` join: both directions seen.
- Collation conflicts (42P21) PostgreSQL finds in branches the engine folds away, and the other
  way round (an implicit-collation conflict across a UNION with a NULL-collation column).
