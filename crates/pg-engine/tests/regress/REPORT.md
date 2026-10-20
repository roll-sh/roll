# PostgreSQL regression suite: engine parity report

Re-run (about an hour; needs the `roll-engine-ref` container):

```sh
CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

Rebuild only this report from the saved outputs in `/tmp/pgregress/out`: add `PGREGRESS_REPORT_ONLY=1`. Run a subset (after `test_setup`): `PGREGRESS_ONLY=int4,join`. The harness is `crates/pg-engine/tests/pg_regress.rs`; its header documents the method.

Suite: `src/test/regress` at `REL_18_6`, `parallel_schedule` run serially in one database. Reference: PostgreSQL 18.6 (`roll-engine-ref`). Both outputs come from the same `psql -X -a -q` with pg_regress's environment. A *unit* is one psql execution unit: a statement (or several on one line), a meta-command, or a COPY with its data. A unit matches when its whole psql output (rows, tags, notices, errors) is byte-identical to the reference's.

## Summary

| | |
|---|---|
| files | 4 |
| units compared | 2466 |
| units matching | 2401 (**97.4%**) |
| units matching, excluding by-design | 97.4% (2401/2466) |
| files fully matching | 1 |
| one-line server-side `COPY ... :'file'` run as `\copy` on both sides | 7 |
| reference output == expected/*.out | 1 of 4 files |
| engine output == expected/*.out (pg_regress "ok") | 0 files |
| engine restarted after (crash/timeout) | none |
| engine panics logged | 0 |

## Differences by class

| class | units | % of differing |
|---|---:|---:|
| wrong EXPLAIN | 42 | 64.6% |
| NOTICE/WARNING differs | 8 | 12.3% |
| wrong result (rows / command output differ) | 6 | 9.2% |
| knock-on (object whose CREATE failed on the engine earlier; aborted transaction) | 6 | 9.2% |
| missing error (reference errors, engine succeeds) | 2 | 3.1% |
| feature missing (engine errors, reference succeeds) | 1 | 1.5% |

## Gap categories, ranked by units affected

Quoted names and numbers are masked (`"…"`, `N`) so similar messages group together. *Feature missing* entries are the engine's error. Many `relation "…" does not exist` units are knock-on effects of an earlier failed CREATE.

| # | units | files | class | category | example files | example statement |
|---:|---:|---:|---|---|---|---|
| 1 | 42 | 3 | wrong EXPLAIN | `EXPLAIN plan text differs` | updatable_views (19), rowsecurity (15), select_views (8) | updatable_views:291 `EXPLAIN (costs off) UPDATE rw_view2 SET aaa=5 WHERE aaa=4;` |
| 2 | 6 | 1 | knock-on | `transaction already aborted by an earlier error` | rowsecurity (6) | rowsecurity:1743 `SELECT ctid, * FROM current_check_2;` |
| 3 | 4 | 1 | notice/warning | `missing NOTICE: f_leak => beafsteak` | select_views (4) | select_views:88 `SELECT * FROM my_property_normal WHERE f_leak(passwd);` |
| 4 | 3 | 2 | wrong result | `SELECT: row order differs` | select_views (2), updatable_views (1) | updatable_views:1665 `SELECT * FROM v1 WHERE b=8;` |
| 5 | 3 | 1 | notice/warning | `missing NOTICE: snooped value: Dick` | updatable_views (3) | updatable_views:1523 `SELECT * FROM rw_view1 WHERE snoop(person);` |
| 6 | 2 | 1 | missing error | `ERROR: query would be affected by row-level security policy for table "…"` | rowsecurity (2) | rowsecurity:2388 `select rls_f(c) from test_t order by rls_f;` |
| 7 | 2 | 1 | wrong result | `MERGE: row order differs` | updatable_views (2) | updatable_views:193 `MERGE INTO rw_view1 t` |
| 8 | 1 | 1 | feature missing | `ERROR: column "…" does not exist` | rowsecurity (1) | rowsecurity:1739 `CREATE POLICY p1 ON current_check_2 AS PERMISSIVE` |
| 9 | 1 | 1 | wrong result | `SELECT: values differ` | rowsecurity (1) | rowsecurity:1779 `SELECT (string_to_array(polqual, ':'))[7] AS inputcollid FROM pg_policy WHERE polrelid = '` |
| 10 | 1 | 1 | notice/warning | `missing NOTICE: f_leak => N-N-N-N` | select_views (1) | select_views:112 `SELECT * FROM my_credit_card_normal WHERE f_leak(cnum);` |

## Most frequent engine errors, unmasked (does not exist / not supported / syntax)

| units | engine error | files |
|---:|---|---|
| 1 | `column "ctid" does not exist` | rowsecurity |

## Per file

`ref=exp` / `eng=exp`: output identical to `expected/<name>[_N].out`. Hunks: `tests/regress/hunks/<name>.diff` (`-` reference, `+` engine).

| file | units | match | rate | engine run | secs (ref/engine) | ref=exp | eng=exp | top gap |
|---|---:|---:|---:|---|---|---|---|---|
| test_setup | 79 | 79 | 100% | ok | 0.2/1.2 | no | no |  |
| updatable_views | 1109 | 1084 | 98% | ok | 0.4/3.3 | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (19) |
| rowsecurity | 1226 | 1201 | 98% | ok | 0.3/6.0 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (15) |
| select_views | 52 | 37 | 71% | ok | 0.1/0.3 | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |

## Limits of the measurement

- Files run serially; pg_regress runs most groups in parallel, so tests that look at concurrent sessions behave as in a serial run on both sides.
- The reference container has no `regress.so`, so `CREATE FUNCTION ... AS :'regresslib'` fails on both sides and later statements using those functions (`widget_in`, `interpt_pp`, `binary_coercible`, `test_canonicalize_path`, ...) error on the reference too. Those units are classed *by design*; where both sides fail identically they count as matching.
- Server-side files (`COPY ... TO :'filename'`, `pg_read_file`) are on the host for the engine and in the container for the reference; psql-side files (`\copy`, `\o`, `\lo_import`) are always in the container, so a `\copy FROM` of a file the server wrote reads the reference's copy.
- The database is `gxg_regression` (not `regression`); `GRANT ... ON DATABASE regression` and psql's `\d host.regression.x` patterns are rewritten. When comparing with `expected/*.out` the name is mapped back.
- A unit's output includes everything psql printed until the next unit, so one wrong statement in a multi-statement line marks the whole line. Pipeline tests (`\startpipeline`) print results at `\endpipeline`, so differences land on that unit.
- Units after a `\quit` that the reference reached but the engine did not count as *not reached*; units neither side reached are not counted.

