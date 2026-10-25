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
| wider grammar (round 2), seeds 100–299, batch 3 engine | 200 × 150 | 29,422 | 25,047 | 7,080 | wrong_rows 11, wrong_state 10, engine_error 83, missing_error 153, wrong_error 237, eval_error 744, error_message 122, error_detail 548 |
| smoke, round 2 grammar | 25 × 80 | 1,937 | 1,654 | 486 | wrong_rows 1, wrong_state 1, engine_error 9, missing_error 6, wrong_error 12, eval_error 34, error_message 10, error_detail 29 |

Round 2 of the grammar added DELETE ... USING, data-modifying CTEs, window frames with
EXCLUDE, ordered-set aggregates (percentile_disc, mode), more jsonb and integer functions.
About a quarter of the statements error on both sides (the generator's waste; the run prints
the most common such errors): array_length / jsonb functions over jsonb scalars, unknown
literals to polymorphic functions, aggregates in WHERE / JOIN ON, LATERAL-less references to
FROM items.

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

## Open

- **Constant folding, left**: a UNION ALL arm found empty (`... WHERE false UNION ALL ...`)
  is dropped with its outputs, which PostgreSQL still simplifies; a strict call over a pulled-up subquery's output whose expression
  fails (`NULL + s.c` with `c` = `width_bucket('NaN', ...)`: PostgreSQL substitutes first);
  a CASE arm a pulled-up constant makes unreachable. The five new smoke entries (s5 #6, s10
  #33, s12 #52, s20 #42, s22 #62) are these.

- **Wrong rows (bind-time folding)**: the grouping-key matching above compares folded
  expressions, PostgreSQL's (`substitute_grouped_columns`) the expressions as written:
  `1+0`, `1::int8`, `COALESCE(1, 5)`, `(1+1)*3` and `HAVING 1 = 1` over a key `1` / `1+1`
  differ in the no-input empty set's row (PostgreSQL: `(1) + 0` is NULL, `COALESCE((1), 5)`
  is 5, the HAVING drops the row).
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
