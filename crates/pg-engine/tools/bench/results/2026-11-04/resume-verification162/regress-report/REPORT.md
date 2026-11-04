# PostgreSQL regression suite: engine parity report

Re-run (about an hour; needs the `roll-engine-ref` container, or a private reference: HANDOFF.md):

```sh
CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

Rebuild only this report from the saved outputs in `$PGREGRESS_DIR/out`: add `PGREGRESS_REPORT_ONLY=1`. Run a subset (after `test_setup`): `PGREGRESS_ONLY=int4,join` (its report goes to `$PGREGRESS_DIR/report`). A full run writes this report, `results.tsv` and STATUS.md's generated blocks (`PGREGRESS_UPDATE_STATUS=1` when its report went elsewhere). The harness is `crates/pg-engine/tests/harness/regress.rs`; its header documents the method.

Run: full schedule, commit `3997cf0` (release build of the engine), 2026-11-04 12:31 UTC, reference container `a162-rref`, `PGREGRESS_TIMEOUT=300`; cluster-global objects the reference had before the run: none.

Suite: `src/test/regress` at `REL_18_6`, `parallel_schedule` run serially in one database. Reference: PostgreSQL 18.6 (`a162-rref`). Both outputs come from the same `psql -X -a -q` with pg_regress's environment. A *unit* is one psql execution unit: a statement (or several on one line), a meta-command, or a COPY with its data. A unit matches when its whole psql output (rows, tags, notices, errors) is byte-identical to the reference's.

## Summary

| | |
|---|---|
| files | 230 |
| units compared | 51150 |
| units matching | 49868 (**97.5%**) |
| units matching, excluding by-design | 97.5% (49868/51150) |
| files fully matching | 180 |
| one-line server-side `COPY ... :'file'` of a file psql wrote, run as `\copy` on both sides | 4 |
| reference output == expected/*.out | 203 of 230 files |
| engine output == expected/*.out (pg_regress "ok") | 159 files |
| engine restarted after (crash/timeout) | none |
| engine panics logged | 0 |

## Differences by class

| class | units | % of differing |
|---|---:|---:|
| wrong result (rows / command output differ) | 939 | 73.2% |
| wrong EXPLAIN | 284 | 22.2% |
| feature missing (engine errors, reference succeeds) | 41 | 3.2% |
| missing error (reference errors, engine succeeds) | 8 | 0.6% |
| psql \d / \s describe output differs | 7 | 0.5% |
| wrong error message (both error, message differs) | 2 | 0.2% |
| knock-on (object whose CREATE failed on the engine earlier; aborted transaction) | 1 | 0.1% |

## Gap categories, ranked by units affected

Quoted names and numbers are masked (`"…"`, `N`) so similar messages group together. *Feature missing* entries are the engine's error. Many `relation "…" does not exist` units are knock-on effects of an earlier failed CREATE.

| # | units | files | class | category | example files | example statement |
|---:|---:|---:|---|---|---|---|
| 1 | 491 | 24 | wrong result | `SELECT: values differ` | tsearch (134), jsonb (100), create_index_spgist (78) | create_index:119 `SELECT count(*) FROM fast_emp4000 WHERE home_base && '(1000,1000,0,0)'::box;` |
| 2 | 368 | 33 | wrong result | `SELECT: row count differs` | window (96), create_index (55), select (24) | copy:353 `SELECT tableoid::regclass, id % 2 = 0 is_even, count(*) from parted_si GROUP BY 1, 2 ORDER` |
| 3 | 284 | 25 | wrong EXPLAIN | `EXPLAIN plan text differs` | join (75), create_index (45), select_parallel (41) | create_index:376 `EXPLAIN (COSTS OFF)` |
| 4 | 75 | 3 | wrong result | `FETCH: row count differs` | portals (59), portals_p2 (13), transactions (3) | transactions:279 `FETCH 10 FROM c;` |
| 5 | 26 | 12 | feature missing | `ERROR: absolute path not allowed` | test_setup (7), btree_index (4), hash_index (4) | test_setup:138 `COPY onek FROM :'filename';` |
| 6 | 11 | 1 | feature missing | `ERROR: large object N does not exist` | largeobject (11) | largeobject:230 `\lo_export :newloid :filename` |
| 7 | 5 | 1 | psql describe | `\echo output differs` | psql (5) | psql:1244 `\echo 'number of rows:' :ROW_COUNT` |
| 8 | 3 | 2 | wrong result | `EXECUTE: values differ` | join (2), select_parallel (1) | join:972 `execute foo(true);` |
| 9 | 2 | 1 | feature missing | `ERROR: currval of sequence "…" is not yet defined in this session` | limit (2) | limit:109 `select currval('testseq');` |
| 10 | 2 | 2 | missing error | `ERROR: division by zero` | transactions (1), psql (1) | transactions:287 `FETCH 10 FROM c;` |
| 11 | 2 | 1 | missing error | `ERROR: portal "…" cannot be run` | transactions (2) | transactions:289 `FETCH 10 FROM c;` |
| 12 | 2 | 1 | psql describe | `\lo_import output differs` | largeobject (2) | largeobject:224 `\lo_import :filename` |
| 13 | 1 | 1 | feature missing | `ERROR: cursor "…" is not positioned on a row` | portals (1) | portals:523 `DELETE FROM onek WHERE CURRENT OF c1;` |
| 14 | 1 | 1 | feature missing | `ERROR: syntax error at or near "…"` | largeobject (1) | largeobject:253 `SELECT fipshash(lo_get(:newloid_1)) = fipshash(lo_get(:newloid_2));` |
| 15 | 1 | 1 | missing error | `ERROR: argument of nth_value must be greater than zero` | window (1) | window:1123 `SELECT nth_value(four, 0) OVER (ORDER BY ten), ten, four FROM tenk1;` |
| 16 | 1 | 1 | missing error | `ERROR: argument of ntile must be greater than zero` | window (1) | window:1121 `SELECT ntile(0) OVER (ORDER BY ten), ten, four FROM tenk1;` |
| 17 | 1 | 1 | missing error | `ERROR: cursor can only scan forward` | portals (1) | portals:246 `FETCH ABSOLUTE 4 FROM foo25ns; -- fail` |
| 18 | 1 | 1 | missing error | `ERROR: invalid input syntax for type smallint: "…"` | select_parallel (1) | select_parallel:460 `select (stringu1 \|\| repeat('abcd', 5000))::int2 from tenk1 where unique1 = 1;` |
| 19 | 1 | 1 | wrong result | `EXECUTE: row count differs` | prepare (1) | prepare:44 `EXECUTE q3('AAAAxx', 5::smallint, 10.5::float, false, 4::bigint);` |
| 20 | 1 | 1 | wrong result | `WITH: values differ` | with (1) | with:246 `WITH q1(x,y) AS (` |
| 21 | 1 | 1 | wrong error message | `ERROR: large object read request is too large  ⟶  engine: ERROR: large object N does not exist` | largeobject (1) | largeobject:261 `SELECT lo_get(:newloid_1);` |
| 22 | 1 | 1 | wrong error message | `ERROR: new row for relation "…" violates check constraint "…"  ⟶  engine: ERROR: absolute path not allowed` | constraints (1) | constraints:265 `COPY COPY_TBL FROM :'filename';` |
| 23 | 1 | 1 | knock-on | `transaction already aborted by an earlier error` | portals (1) | portals:524 `SELECT stringu1 FROM onek WHERE stringu1 = 'DZAAAA';` |

## Most frequent engine errors, unmasked (does not exist / not supported / syntax)

| units | engine error | files |
|---:|---|---|
| 11 | `large object 0 does not exist` | largeobject |
| 1 | `syntax error at or near ":"` | largeobject |

## Per file

`ref=exp` / `eng=exp`: output identical to `expected/<name>[_N].out`. Hunks: `tests/regress/hunks/<name>.diff` (`-` reference, `+` engine).

| file | units | match | rate | engine run | secs (ref/engine) | ref=exp | eng=exp | top gap |
|---|---:|---:|---:|---|---|---|---|---|
| test_setup | 79 | 72 | 91% | ok | 0.3/0.3 | no | no | feature missing `ERROR: absolute path not allowed` (7) |
| boolean | 99 | 99 | 100% | ok | 0.3/0.4 | yes | yes |  |
| char | 32 | 32 | 100% | ok | 0.2/0.3 | yes | yes |  |
| name | 40 | 40 | 100% | ok | 0.1/0.3 | yes | yes |  |
| varchar | 22 | 22 | 100% | ok | 0.2/0.3 | yes | yes |  |
| text | 73 | 73 | 100% | ok | 0.1/0.3 | yes | yes |  |
| int2 | 76 | 76 | 100% | ok | 0.2/0.3 | yes | yes |  |
| int4 | 94 | 94 | 100% | ok | 0.2/0.3 | yes | yes |  |
| int8 | 174 | 174 | 100% | ok | 0.2/0.3 | yes | yes |  |
| oid | 37 | 37 | 100% | ok | 0.2/0.3 | yes | yes |  |
| float4 | 100 | 100 | 100% | ok | 0.2/0.3 | yes | yes |  |
| float8 | 184 | 184 | 100% | ok | 0.2/0.4 | yes | yes |  |
| bit | 133 | 133 | 100% | ok | 0.3/0.4 | yes | yes |  |
| numeric | 1058 | 1058 | 100% | ok | 0.4/0.8 | yes | yes |  |
| txid | 47 | 47 | 100% | ok | 0.2/0.3 | yes | yes |  |
| uuid | 68 | 68 | 100% | ok | 0.2/0.3 | yes | yes |  |
| enum | 172 | 172 | 100% | ok | 0.2/0.4 | yes | yes |  |
| money | 113 | 113 | 100% | ok | 0.2/0.3 | yes | yes |  |
| rangetypes | 419 | 419 | 100% | ok | 0.4/0.8 | yes | yes |  |
| pg_lsn | 31 | 31 | 100% | ok | 0.2/0.3 | yes | yes |  |
| regproc | 107 | 107 | 100% | ok | 0.2/0.3 | yes | yes |  |
| strings | 504 | 504 | 100% | ok | 0.2/0.5 | yes | yes |  |
| md5 | 14 | 14 | 100% | ok | 0.2/0.3 | yes | yes |  |
| numerology | 89 | 89 | 100% | ok | 0.1/0.3 | yes | yes |  |
| point | 43 | 43 | 100% | ok | 0.2/0.3 | yes | yes |  |
| lseg | 16 | 16 | 100% | ok | 0.2/0.3 | yes | yes |  |
| line | 35 | 35 | 100% | ok | 0.1/0.3 | yes | yes |  |
| box | 101 | 101 | 100% | ok | 0.4/0.8 | yes | yes |  |
| path | 23 | 23 | 100% | ok | 0.2/0.3 | yes | yes |  |
| polygon | 62 | 62 | 100% | ok | 0.3/0.9 | yes | yes |  |
| circle | 22 | 22 | 100% | ok | 0.2/0.3 | yes | yes |  |
| date | 271 | 271 | 100% | ok | 0.2/0.4 | yes | yes |  |
| time | 44 | 44 | 100% | ok | 0.2/0.3 | yes | yes |  |
| timetz | 57 | 57 | 100% | ok | 0.2/0.3 | yes | yes |  |
| timestamp | 177 | 177 | 100% | ok | 0.5/0.7 | yes | yes |  |
| timestamptz | 406 | 406 | 100% | ok | 0.6/0.7 | yes | yes |  |
| interval | 446 | 446 | 100% | ok | 0.3/0.6 | yes | yes |  |
| inet | 116 | 116 | 100% | ok | 0.2/0.5 | yes | yes |  |
| macaddr | 35 | 35 | 100% | ok | 0.2/0.4 | yes | yes |  |
| macaddr8 | 71 | 71 | 100% | ok | 0.2/0.4 | yes | yes |  |
| multirangetypes | 619 | 619 | 100% | ok | 0.5/1.4 | yes | yes |  |
| geometry | 162 | 162 | 100% | ok | 0.3/0.5 | yes | yes |  |
| horology | 399 | 399 | 100% | ok | 0.6/0.8 | yes | yes |  |
| tstypes | 238 | 238 | 100% | ok | 0.3/0.6 | yes | yes |  |
| regex | 105 | 105 | 100% | ok | 0.5/1.1 | yes | yes |  |
| type_sanity | 66 | 66 | 100% | ok | 0.3/0.8 | no | no |  |
| opr_sanity | 133 | 133 | 100% | ok | 0.6/1.3 | no | no |  |
| misc_sanity | 5 | 5 | 100% | ok | 0.2/0.4 | yes | yes |  |
| comments | 7 | 7 | 100% | ok | 0.2/0.4 | yes | yes |  |
| expressions | 81 | 81 | 100% | ok | 0.3/0.6 | yes | yes |  |
| unicode | 32 | 32 | 100% | ok | 0.2/0.4 | yes | yes |  |
| xid | 84 | 84 | 100% | ok | 0.2/0.5 | yes | yes |  |
| mvcc | 11 | 11 | 100% | ok | 0.3/0.4 | yes | yes |  |
| database | 16 | 16 | 100% | ok | 0.6/0.7 | yes | yes |  |
| stats_import | 150 | 150 | 100% | ok | 0.4/0.7 | yes | yes |  |
| encoding | 81 | 81 | 100% | ok | 0.2/0.4 | no | no |  |
| euc_kr | 5 | 5 | 100% | ok | 0.2/0.2 | yes | yes |  |
| copy | 118 | 115 | 97% | ok | 0.2/0.4 | yes | no | feature missing `ERROR: absolute path not allowed` (2) |
| copyselect | 36 | 36 | 100% | ok | 0.2/0.9 | no | no |  |
| copydml | 62 | 62 | 100% | ok | 0.2/0.3 | yes | yes |  |
| copyencoding | 22 | 22 | 100% | ok | 0.2/0.3 | yes | yes |  |
| insert | 399 | 399 | 100% | ok | 0.6/0.7 | no | no |  |
| insert_conflict | 261 | 261 | 100% | ok | 0.3/0.5 | yes | yes |  |
| create_function_c | 10 | 10 | 100% | ok | 0.2/0.3 | no | no |  |
| create_misc | 88 | 88 | 100% | ok | 0.2/0.3 | yes | yes |  |
| create_operator | 90 | 90 | 100% | ok | 0.2/0.3 | yes | yes |  |
| create_procedure | 115 | 115 | 100% | ok | 0.2/0.4 | yes | yes |  |
| create_table | 348 | 348 | 100% | ok | 0.6/0.8 | yes | yes |  |
| create_type | 88 | 88 | 100% | ok | 0.2/0.3 | no | no |  |
| create_schema | 30 | 30 | 100% | ok | 0.2/0.4 | yes | yes |  |
| create_index | 708 | 579 | 82% | ok | 0.9/1.2 | yes | no | wrong result `SELECT: row count differs` (55) |
| create_index_spgist | 202 | 124 | 61% | ok | 0.5/0.5 | yes | no | wrong result `SELECT: values differ` (78) |
| create_view | 343 | 342 | 100% | ok | 0.5/1.0 | no | no | feature missing `ERROR: absolute path not allowed` (1) |
| index_including | 137 | 137 | 100% | ok | 0.3/0.4 | yes | yes |  |
| index_including_gist | 52 | 52 | 100% | ok | 0.4/0.5 | yes | yes |  |
| create_aggregate | 60 | 60 | 100% | ok | 0.2/0.3 | yes | yes |  |
| create_function_sql | 168 | 168 | 100% | ok | 0.2/0.7 | yes | yes |  |
| create_cast | 24 | 24 | 100% | ok | 0.2/0.3 | yes | yes |  |
| constraints | 607 | 603 | 99% | ok | 0.7/1.2 | yes | no | wrong result `SELECT: row count differs` (2) |
| triggers | 1041 | 1041 | 100% | ok | 1.0/1.1 | no | no |  |
| select | 87 | 54 | 62% | ok | 0.2/0.4 | yes | no | wrong result `SELECT: row count differs` (24) |
| inherit | 934 | 930 | 100% | ok | 1.1/2.0 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| typed_table | 36 | 36 | 100% | ok | 0.2/0.4 | yes | yes |  |
| vacuum | 328 | 328 | 100% | ok | 0.7/0.6 | yes | yes |  |
| drop_if_exists | 161 | 161 | 100% | ok | 0.2/0.4 | yes | yes |  |
| updatable_views | 1109 | 1109 | 100% | ok | 0.7/1.4 | no | no |  |
| roleattributes | 80 | 80 | 100% | ok | 0.2/0.3 | yes | yes |  |
| create_am | 142 | 139 | 98% | ok | 0.3/0.4 | yes | no | wrong result `SELECT: values differ` (2) |
| hash_func | 43 | 43 | 100% | ok | 0.3/0.3 | yes | yes |  |
| errors | 87 | 87 | 100% | ok | 0.2/0.3 | yes | yes |  |
| infinite_recurse | 8 | 8 | 100% | ok | 0.6/0.6 | yes | yes |  |
| sanity_check | 3 | 3 | 100% | ok | 0.3/0.3 | yes | yes |  |
| select_into | 67 | 67 | 100% | ok | 0.2/0.3 | yes | yes |  |
| select_distinct | 101 | 88 | 87% | ok | 0.4/0.3 | yes | no | wrong result `SELECT: row count differs` (8) |
| select_distinct_on | 23 | 19 | 83% | ok | 0.2/0.3 | yes | no | wrong result `SELECT: row count differs` (3) |
| select_implicit | 44 | 44 | 100% | ok | 0.2/0.3 | yes | yes |  |
| select_having | 23 | 23 | 100% | ok | 0.2/0.3 | yes | yes |  |
| subselect | 356 | 301 | 85% | ok | 0.5/0.6 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (34) |
| union | 198 | 184 | 93% | ok | 0.2/0.5 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (8) |
| case | 64 | 64 | 100% | ok | 0.2/0.3 | yes | yes |  |
| join | 925 | 817 | 88% | ok | 1.3/1.1 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (75) |
| aggregates | 556 | 461 | 83% | ok | 0.8/0.9 | yes | no | wrong result `SELECT: values differ` (70) |
| transactions | 433 | 427 | 99% | ok | 0.2/0.4 | yes | no | wrong result `FETCH: row count differs` (3) |
| random | 48 | 48 | 100% | ok | 0.2/0.4 | yes | yes |  |
| portals | 354 | 291 | 82% | ok | 0.2/0.4 | yes | no | wrong result `FETCH: row count differs` (59) |
| arrays | 519 | 493 | 95% | ok | 0.3/0.5 | yes | no | wrong result `SELECT: row count differs` (22) |
| btree_index | 135 | 113 | 84% | ok | 0.9/1.7 | yes | no | wrong result `SELECT: row count differs` (17) |
| hash_index | 102 | 85 | 83% | ok | 0.8/0.6 | yes | no | wrong result `SELECT: row count differs` (12) |
| update | 289 | 289 | 100% | ok | 0.6/0.6 | yes | yes |  |
| delete | 10 | 10 | 100% | ok | 0.2/0.2 | yes | yes |  |
| namespace | 42 | 42 | 100% | ok | 0.2/0.4 | yes | yes |  |
| prepared_xacts | 2 | 2 | 100% | ok | 0.2/0.2 | yes | yes |  |
| brin | 77 | 76 | 99% | ok | 0.4/0.7 | yes | no | wrong result `SELECT: values differ` (1) |
| gin | 59 | 59 | 100% | ok | 0.7/1.3 | yes | yes |  |
| gist | 69 | 69 | 100% | ok | 0.6/0.9 | yes | yes |  |
| spgist | 31 | 31 | 100% | ok | 0.4/7.7 | yes | yes |  |
| privileges | 1398 | 1398 | 100% | ok | 1.5/3.3 | no | no |  |
| init_privs | 4 | 4 | 100% | ok | 0.1/0.2 | yes | yes |  |
| security_label | 28 | 28 | 100% | ok | 0.2/0.3 | yes | yes |  |
| collate | 149 | 149 | 100% | ok | 0.3/0.4 | yes | yes |  |
| matview | 189 | 189 | 100% | ok | 0.4/0.6 | yes | yes |  |
| lock | 134 | 134 | 100% | ok | 0.1/0.3 | no | no |  |
| replica_identity | 75 | 75 | 100% | ok | 0.3/0.5 | yes | yes |  |
| rowsecurity | 1226 | 1226 | 100% | ok | 0.7/2.1 | yes | yes |  |
| object_address | 85 | 85 | 100% | ok | 0.2/0.4 | yes | yes |  |
| tablesample | 58 | 52 | 90% | ok | 0.2/0.3 | yes | no | wrong result `SELECT: values differ` (4) |
| groupingsets | 217 | 190 | 88% | ok | 0.7/0.4 | yes | no | wrong result `SELECT: row count differs` (17) |
| drop_operator | 12 | 12 | 100% | ok | 0.2/0.3 | yes | yes |  |
| password | 53 | 53 | 100% | ok | 0.2/0.4 | yes | yes |  |
| identity | 288 | 288 | 100% | ok | 0.4/0.7 | yes | yes |  |
| generated_stored | 468 | 468 | 100% | ok | 0.6/1.4 | yes | yes |  |
| join_hash | 279 | 277 | 99% | ok | 5.0/2.1 | yes | no | wrong result `SELECT: row count differs` (1) |
| brin_bloom | 40 | 40 | 100% | ok | 0.2/0.5 | yes | yes |  |
| brin_multi | 172 | 171 | 99% | ok | 0.4/0.8 | yes | no | wrong result `SELECT: values differ` (1) |
| create_table_like | 191 | 191 | 100% | ok | 0.5/1.1 | yes | yes |  |
| alter_generic | 336 | 336 | 100% | ok | 0.3/1.0 | no | no |  |
| alter_operator | 65 | 65 | 100% | ok | 0.1/0.4 | yes | yes |  |
| misc | 68 | 54 | 79% | ok | 0.7/0.4 | no | no | wrong result `SELECT: row count differs` (13) |
| async | 11 | 11 | 100% | ok | 0.1/0.3 | yes | yes |  |
| dbsize | 25 | 25 | 100% | ok | 0.1/0.2 | yes | yes |  |
| merge | 572 | 572 | 100% | ok | 0.5/1.5 | yes | yes |  |
| misc_functions | 148 | 146 | 99% | ok | 0.3/0.3 | no | no | wrong EXPLAIN `EXPLAIN plan text differs` (2) |
| sysviews | 29 | 29 | 100% | ok | 0.2/0.3 | yes | yes |  |
| tsrf | 74 | 74 | 100% | ok | 0.2/0.3 | yes | yes |  |
| tid | 41 | 41 | 100% | ok | 0.2/0.3 | yes | yes |  |
| tidscan | 49 | 45 | 92% | ok | 0.1/0.3 | yes | no | wrong result `SELECT: values differ` (2) |
| tidrangescan | 45 | 45 | 100% | ok | 0.2/0.3 | yes | yes |  |
| collate.utf8 | 61 | 61 | 100% | ok | 0.2/0.3 | yes | yes |  |
| collate.icu.utf8 | 681 | 681 | 100% | ok | 0.8/0.9 | yes | yes |  |
| incremental_sort | 120 | 110 | 92% | ok | 0.3/0.5 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (10) |
| create_role | 144 | 144 | 100% | ok | 0.2/0.9 | yes | yes |  |
| without_overlaps | 674 | 674 | 100% | ok | 1.2/1.3 | yes | yes |  |
| generated_virtual | 489 | 489 | 100% | ok | 0.6/1.2 | yes | yes |  |
| rules | 632 | 632 | 100% | ok | 3.5/1.0 | yes | yes |  |
| psql | 1264 | 1255 | 99% | ok | 0.7/1.8 | no | no | psql describe `\echo output differs` (5) |
| psql_crosstab | 32 | 32 | 100% | ok | 0.1/0.3 | yes | yes |  |
| psql_pipeline | 362 | 362 | 100% | ok | 0.2/0.4 | yes | yes |  |
| amutils | 10 | 10 | 100% | ok | 0.2/0.4 | yes | yes |  |
| stats_ext | 878 | 878 | 100% | ok | 1.3/3.6 | yes | yes |  |
| collate.linux.utf8 | 2 | 2 | 100% | ok | 0.2/0.2 | yes | yes |  |
| select_parallel | 250 | 184 | 74% | ok | 2.9/1.0 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (41) |
| write_parallel | 22 | 18 | 82% | ok | 0.3/0.2 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (4) |
| vacuum_parallel | 14 | 14 | 100% | ok | 0.3/0.6 | yes | yes |  |
| maintain_every | 16 | 16 | 100% | ok | 0.2/0.4 | yes | yes |  |
| publication | 781 | 781 | 100% | ok | 5.4/2.1 | yes | yes |  |
| subscription | 179 | 179 | 100% | ok | 0.2/0.4 | no | no |  |
| select_views | 52 | 50 | 96% | ok | 0.2/0.3 | no | no | wrong result `SELECT: row count differs` (2) |
| portals_p2 | 41 | 28 | 68% | ok | 0.2/0.2 | yes | no | wrong result `FETCH: row count differs` (13) |
| foreign_key | 1267 | 1267 | 100% | ok | 1.8/2.1 | yes | yes |  |
| cluster | 205 | 205 | 100% | ok | 0.4/0.8 | yes | yes |  |
| dependency | 68 | 68 | 100% | ok | 0.2/0.7 | yes | yes |  |
| guc | 222 | 222 | 100% | ok | 0.2/0.4 | yes | yes |  |
| bitmapops | 12 | 12 | 100% | ok | 0.3/0.7 | yes | yes |  |
| combocid | 62 | 62 | 100% | ok | 0.2/0.3 | yes | yes |  |
| tsearch | 451 | 315 | 70% | ok | 0.4/0.6 | yes | no | wrong result `SELECT: values differ` (134) |
| tsdicts | 131 | 131 | 100% | ok | 0.2/0.3 | yes | yes |  |
| foreign_data | 624 | 624 | 100% | ok | 0.8/2.7 | no | no |  |
| window | 400 | 298 | 74% | ok | 0.3/0.6 | yes | no | wrong result `SELECT: row count differs` (96) |
| xmlmap | 40 | 40 | 100% | ok | 0.2/0.4 | no | no |  |
| functional_deps | 40 | 40 | 100% | ok | 0.2/0.3 | yes | yes |  |
| advisory_lock | 38 | 38 | 100% | ok | 0.2/0.3 | yes | yes |  |
| indirect_toast | 31 | 31 | 100% | ok | 0.2/0.4 | no | no |  |
| equivclass | 96 | 96 | 100% | ok | 0.5/0.5 | yes | yes |  |
| json | 469 | 469 | 100% | ok | 0.3/0.4 | yes | yes |  |
| jsonb | 1090 | 988 | 91% | ok | 0.5/0.7 | yes | no | wrong result `SELECT: values differ` (100) |
| json_encoding | 47 | 47 | 100% | ok | 0.2/0.3 | yes | yes |  |
| jsonpath | 224 | 224 | 100% | ok | 0.2/0.2 | yes | yes |  |
| jsonpath_encoding | 35 | 35 | 100% | ok | 0.2/0.3 | yes | yes |  |
| jsonb_jsonpath | 836 | 836 | 100% | ok | 0.3/0.5 | yes | yes |  |
| sqljson | 242 | 242 | 100% | ok | 0.2/0.5 | yes | yes |  |
| sqljson_queryfuncs | 318 | 318 | 100% | ok | 0.2/0.5 | yes | yes |  |
| sqljson_jsontable | 131 | 131 | 100% | ok | 0.2/0.4 | yes | yes |  |
| plancache | 97 | 96 | 99% | ok | 0.2/0.4 | yes | no | wrong result `SELECT: values differ` (1) |
| limit | 85 | 63 | 74% | ok | 0.2/0.4 | yes | no | wrong result `SELECT: row count differs` (17) |
| plpgsql | 941 | 939 | 100% | ok | 0.7/2.4 | yes | no | wrong result `SELECT: values differ` (2) |
| copy2 | 290 | 290 | 100% | ok | 0.2/0.5 | no | no |  |
| temp | 211 | 211 | 100% | ok | 0.3/1.1 | yes | yes |  |
| domain | 486 | 486 | 100% | ok | 0.4/1.5 | no | no |  |
| rangefuncs | 428 | 428 | 100% | ok | 0.4/1.1 | yes | yes |  |
| prepare | 33 | 31 | 94% | ok | 0.2/0.3 | yes | no | wrong result `EXECUTE: row count differs` (1) |
| conversion | 68 | 68 | 100% | ok | 0.2/0.4 | no | no |  |
| truncate | 193 | 193 | 100% | ok | 0.3/0.5 | yes | yes |  |
| alter_table | 1749 | 1748 | 100% | ok | 2.0/5.9 | yes | no | wrong result `SELECT: row count differs` (1) |
| sequence | 265 | 265 | 100% | ok | 0.2/1.2 | no | no |  |
| polymorphism | 426 | 426 | 100% | ok | 0.3/1.2 | yes | yes |  |
| rowtypes | 237 | 224 | 95% | ok | 0.3/0.5 | yes | no | wrong result `SELECT: row count differs` (8) |
| returning | 145 | 145 | 100% | ok | 0.3/0.5 | yes | yes |  |
| largeobject | 151 | 124 | 82% | ok | 0.3/0.4 | yes | no | feature missing `ERROR: large object N does not exist` (11) |
| with | 307 | 303 | 99% | ok | 0.4/0.6 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (3) |
| xml | 281 | 279 | 99% | ok | 1.8/0.4 | yes | no | wrong result `SELECT: row count differs` (1) |
| partition_join | 614 | 614 | 100% | ok | 1.0/2.8 | yes | yes |  |
| partition_prune | 771 | 770 | 100% | ok | 1.5/2.8 | yes | no | wrong result `SELECT: row count differs` (1) |
| reloptions | 66 | 66 | 100% | ok | 0.2/0.5 | yes | yes |  |
| hash_part | 29 | 29 | 100% | ok | 0.2/0.3 | yes | yes |  |
| indexing | 645 | 645 | 100% | ok | 1.8/3.7 | yes | yes |  |
| partition_aggregate | 137 | 137 | 100% | ok | 0.8/1.5 | yes | yes |  |
| partition_info | 73 | 73 | 100% | ok | 0.2/0.4 | yes | yes |  |
| tuplesort | 108 | 108 | 100% | ok | 0.7/2.1 | yes | yes |  |
| explain | 60 | 58 | 97% | ok | 0.2/0.4 | yes | no | wrong result `SELECT: row count differs` (1) |
| memoize | 73 | 58 | 79% | ok | 0.8/0.5 | yes | no | wrong result `SELECT: row count differs` (7) |
| stats | 482 | 478 | 99% | ok | 0.9/1.4 | yes | no | wrong result `SELECT: values differ` (4) |
| predicate | 42 | 42 | 100% | ok | 0.2/0.4 | yes | yes |  |
| numa | 3 | 3 | 100% | ok | 0.2/0.3 | yes | yes |  |
| compression | 102 | 102 | 100% | ok | 0.3/0.8 | yes | yes |  |
| compression_pglz | 21 | 21 | 100% | ok | 0.2/0.2 | no | no |  |
| oidjoins | 1 | 1 | 100% | ok | 0.3/0.6 | yes | yes |  |
| event_trigger | 230 | 230 | 100% | ok | 0.4/1.8 | yes | yes |  |
| event_trigger_login | 14 | 14 | 100% | ok | 0.2/0.4 | yes | yes |  |
| fast_default | 275 | 275 | 100% | ok | 0.4/1.8 | yes | yes |  |
| tablespace | 261 | 261 | 100% | ok | 0.6/1.8 | yes | yes |  |

## Differing units

| file | differing | units (harness class) |
|---|---:|---|
| test_setup | 7 | 33, 39, 45, 49, 53, 57, 61 (feature missing) |
| copy | 3 | 69, 104 (feature missing); 105 (wrong result) |
| create_index | 129 | 30, 122 (feature missing); 47, 48, 49, 51, 65, 66, 71, 73, 75, 79, 107, 109, 111, 124, 127, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 144, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 158, 161, 162, 163, 164, 165, 166, 167, 184, 335, 337, 344, 346, 354, 356, 363, 365, 373, 374, 375, 376, 377, 378, 384, 386, 392, 394, 398, 400, 402, 404, 406, 408, 417, 419, 421, 423, 425, 427, 437, 441, 445, 447, 455, 457, 459 (wrong result); 183, 383, 385, 387, 388, 389, 390, 391, 393, 395, 397, 399, 401, 403, 405, 407, 409, 410, 411, 412, 416, 418, 420, 422, 424, 426, 428, 430, 432, 434, 436, 438, 440, 442, 444, 446, 448, 450, 452, 454, 456, 458, 460, 655, 656 (wrong EXPLAIN) |
| create_index_spgist | 78 | 15, 16, 17, 18, 19, 20, 21, 22, 23, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 47, 49, 51, 53, 55, 57, 59, 61, 63, 74, 76, 78, 80, 82, 84, 86, 99, 101, 109, 111, 113, 115, 117, 119, 121, 123, 125, 127, 129, 131, 138, 140, 142, 144, 146, 148, 150, 152, 154, 156, 158, 160, 162, 164, 166, 168, 176, 178, 180, 182, 184, 186, 188, 190, 192, 194, 196, 198 (wrong result) |
| create_view | 1 | 7 (feature missing) |
| constraints | 4 | 112 (feature missing); 113, 116 (wrong result); 115 (wrong error message) |
| select | 33 | 0, 1, 2, 3, 4, 5, 6, 11, 12, 13, 17, 18, 22, 23, 24, 56, 59, 61, 63, 65, 67, 70, 73, 75 (wrong result); 55, 57, 58, 60, 62, 64, 69, 72, 74 (wrong EXPLAIN) |
| inherit | 4 | 467, 468, 469, 472 (wrong EXPLAIN) |
| create_am | 3 | 12, 14, 16 (wrong result) |
| select_distinct | 13 | 0, 1, 2, 3, 4, 6, 37, 47, 51 (wrong result); 36, 39, 41, 55 (wrong EXPLAIN) |
| select_distinct_on | 4 | 0, 2, 5 (wrong result); 7 (wrong EXPLAIN) |
| subselect | 55 | 36, 37, 56, 57, 58, 59, 98, 99, 178, 180, 194, 198, 203, 206, 217, 224, 225, 235, 238, 241, 291 (wrong result); 177, 179, 193, 197, 202, 205, 216, 237, 240, 290, 321, 322, 323, 324, 325, 326, 327, 328, 329, 330, 331, 332, 333, 335, 336, 337, 345, 347, 348, 349, 350, 351, 352, 354 (wrong EXPLAIN) |
| union | 14 | 41, 43, 46, 53, 55, 57 (wrong result); 40, 42, 52, 54, 56, 58, 195, 196 (wrong EXPLAIN) |
| join | 108 | 111, 112, 116, 122, 126, 146, 224, 276, 277, 315, 344, 348, 409, 411, 413, 417, 419, 453, 463, 465, 467, 469, 549, 725, 727, 730, 734, 741, 743, 782, 920, 922, 924 (wrong result); 66, 123, 125, 130, 136, 137, 142, 145, 150, 218, 223, 238, 239, 240, 241, 242, 244, 246, 314, 340, 341, 342, 343, 345, 347, 368, 378, 379, 380, 381, 383, 384, 385, 386, 399, 401, 402, 403, 404, 405, 406, 407, 408, 410, 412, 416, 418, 420, 424, 452, 454, 455, 462, 464, 466, 468, 494, 543, 548, 586, 726, 728, 729, 731, 735, 736, 737, 740, 742, 781, 888, 892, 919, 921, 923 (wrong EXPLAIN) |
| aggregates | 95 | 4 (feature missing); 6, 7, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 48, 49, 50, 51, 74, 75, 76, 77, 78, 79, 80, 81, 82, 101, 102, 103, 104, 105, 106, 107, 108, 109, 111, 113, 131, 133, 135, 137, 144, 146, 148, 150, 152, 154, 156, 158, 160, 352, 353, 354, 355, 361, 362, 374, 375, 376, 377, 382, 383, 384, 386, 390, 403, 443, 489, 506, 508, 517 (wrong result); 130, 132, 134, 136, 140, 143, 145, 147, 149, 151, 153, 155, 157, 488, 505, 507 (wrong EXPLAIN) |
| transactions | 6 | 220, 222, 225 (missing error); 212, 214, 216 (wrong result) |
| portals | 63 | 295 (feature missing); 114 (missing error); 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 92, 93, 99, 100, 102, 103, 104, 109, 110, 112, 113, 124, 294 (wrong result); 292 (wrong EXPLAIN); 296 (knock-on) |
| arrays | 26 | 4 (feature missing); 172, 173, 174, 175, 176, 177, 178, 179, 180, 182, 183, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 197, 396, 397, 398 (wrong result) |
| btree_index | 22 | 6, 8, 10, 12 (feature missing); 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 48, 53, 55, 57, 59 (wrong result); 47 (wrong EXPLAIN) |
| hash_index | 17 | 6, 7, 8, 9 (feature missing); 22, 24, 26, 28, 31, 33, 35, 37, 41, 43, 45, 47 (wrong result); 30 (wrong EXPLAIN) |
| brin | 1 | 36 (wrong result) |
| tablesample | 6 | 34, 35, 37, 38, 40, 41 (wrong result) |
| groupingsets | 27 | 38, 39, 42, 43, 47, 49, 51, 78, 94, 95, 98, 99, 100, 101, 102, 103, 144, 145, 146, 147 (wrong result); 46, 48, 50, 61, 151, 152, 154 (wrong EXPLAIN) |
| join_hash | 2 | 277 (wrong result); 276 (wrong EXPLAIN) |
| brin_multi | 1 | 26 (wrong result) |
| misc | 14 | 23, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 62, 66, 67 (wrong result) |
| misc_functions | 2 | 98, 101 (wrong EXPLAIN) |
| tidscan | 4 | 39, 42 (wrong result); 38, 41 (wrong EXPLAIN) |
| incremental_sort | 10 | 99, 100, 101, 102, 103, 104, 105, 106, 107, 119 (wrong EXPLAIN) |
| psql | 9 | 757 (missing error); 752, 935, 938 (wrong result); 755, 758, 759, 761, 762 (psql describe) |
| select_parallel | 66 | 199 (missing error); 39, 42, 48, 53, 57, 65, 74, 76, 78, 81, 83, 90, 105, 116, 127, 136, 141, 144, 148, 152, 154, 157, 160, 188 (wrong result); 38, 41, 46, 47, 49, 50, 52, 56, 64, 73, 75, 77, 80, 82, 89, 104, 112, 126, 131, 132, 135, 138, 140, 143, 147, 151, 153, 156, 159, 164, 167, 170, 174, 191, 192, 195, 203, 205, 207, 208, 217 (wrong EXPLAIN) |
| write_parallel | 4 | 5, 8, 11, 18 (wrong EXPLAIN) |
| select_views | 2 | 0, 2 (wrong result) |
| portals_p2 | 13 | 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26 (wrong result) |
| tsearch | 136 | 9 (feature missing); 11, 12, 13, 14, 15, 16, 17, 18, 20, 21, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 40, 41, 42, 43, 44, 45, 46, 47, 49, 50, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 67, 68, 69, 70, 71, 72, 73, 74, 76, 77, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 100, 101, 102, 103, 104, 105, 106, 107, 109, 110, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 128, 129, 130, 131, 132, 133, 134, 135, 137, 138, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 160, 161, 162, 163, 164, 165, 166, 167, 169, 170, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 185, 187, 190 (wrong result) |
| window | 102 | 264, 265 (missing error); 4, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 34, 35, 38, 43, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 129, 130, 133, 134, 137, 138, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 325, 326 (wrong result); 392, 393, 394, 395 (wrong EXPLAIN) |
| jsonb | 102 | 3 (feature missing); 197, 199, 532, 533, 534, 535, 536, 537, 539, 540, 541, 542, 543, 544, 545, 546, 547, 549, 550, 551, 552, 553, 554, 555, 556, 557, 562, 563, 564, 565, 566, 567, 568, 569, 570, 572, 573, 575, 576, 577, 578, 579, 580, 581, 582, 583, 584, 585, 586, 587, 588, 590, 591, 593, 594, 595, 596, 597, 598, 599, 600, 601, 602, 605, 607, 609, 610, 611, 613, 616, 625, 626, 630, 631, 632, 633, 634, 635, 636, 637, 638, 639, 640, 641, 642, 643, 644, 645, 646, 647, 648, 650, 651, 652, 653, 654, 655, 656, 657, 658, 659 (wrong result) |
| plancache | 1 | 32 (wrong result) |
| limit | 22 | 54, 57 (feature missing); 0, 1, 2, 3, 5, 6, 7, 8, 53, 56, 59, 61, 67, 68, 69, 70, 71 (wrong result); 52, 58, 66 (wrong EXPLAIN) |
| plpgsql | 2 | 414, 453 (wrong result) |
| prepare | 2 | 17, 24 (wrong result) |
| alter_table | 1 | 140 (wrong result) |
| rowtypes | 13 | 59, 61, 63, 65, 67, 205, 206, 207 (wrong result); 60, 62, 64, 66, 204 (wrong EXPLAIN) |
| largeobject | 27 | 76, 94, 98, 102, 103, 104, 105, 106, 107, 108, 110, 111, 112 (feature missing); 79, 80, 81, 82, 83, 84, 85, 86, 87, 90, 96 (wrong result); 109 (wrong error message); 91, 100 (psql describe) |
| with | 4 | 32 (wrong result); 50, 51, 52 (wrong EXPLAIN) |
| xml | 2 | 30, 120 (wrong result) |
| partition_prune | 1 | 410 (wrong result) |
| explain | 2 | 22 (wrong result); 42 (wrong EXPLAIN) |
| memoize | 15 | 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 23, 56, 68 (wrong result); 55, 67 (wrong EXPLAIN) |
| stats | 4 | 57, 59, 65, 66 (wrong result) |

## Changes against the baseline run

Baseline: `tests/regress/results.tsv` (full run of commit `f24d364`, debug build, 2026-11-01 19:09 UTC, reference `t3-d4020b98-regref-f24d3643`). This run: commit `3997cf0`, release build. Newly differing units: 1280; newly matching: 0.

The two runs' engines are builds of different (or unknown) profiles.

| file | units | newly differing | newly matching |
|---|---:|---|---|
| test_setup | 79 | 33, 39, 45, 49, 53, 57, 61 |  |
| copy | 118 | 69, 104, 105 |  |
| create_index | 708 | 30, 47, 48, 49, 51, 65, 66, 71, 73, 75, 79, 107, 109, 111, 122, 124, 127, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 144, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 158, 161, 162, 163, 164, 165, 166, 167, 183, 184, 335, 337, 344, 346, 354, 356, 363, 365, 373, 374, 375, 376, 377, 378, 383, 384, 385, 386, 387, 388, 389, 390, 391, 392, 393, 394, 395, 397, 398, 399, 400, 401, 402, 403, 404, 405, 406, 407, 408, 409, 410, 411, 412, 416, 417, 418, 419, 420, 421, 422, 423, 424, 425, 426, 427, 428, 430, 432, 434, 436, 437, 438, 440, 441, 442, 444, 445, 446, 447, 448, 450, 452, 454, 455, 456, 457, 458, 459, 460, 655, 656 |  |
| create_index_spgist | 202 | 15, 16, 17, 18, 19, 20, 21, 22, 23, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 47, 49, 51, 53, 55, 57, 59, 61, 63, 74, 76, 78, 80, 82, 84, 86, 99, 101, 109, 111, 113, 115, 117, 119, 121, 123, 125, 127, 129, 131, 138, 140, 142, 144, 146, 148, 150, 152, 154, 156, 158, 160, 162, 164, 166, 168, 176, 178, 180, 182, 184, 186, 188, 190, 192, 194, 196, 198 |  |
| create_view | 343 | 7 |  |
| constraints | 607 | 112, 113, 115, 116 |  |
| select | 87 | 0, 1, 2, 3, 4, 5, 6, 11, 12, 13, 17, 18, 22, 23, 24, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 67, 69, 70, 72, 73, 74, 75 |  |
| inherit | 934 | 467, 468, 469, 472 |  |
| create_am | 142 | 12, 14, 16 |  |
| select_distinct | 101 | 0, 1, 2, 3, 4, 6, 36, 37, 39, 41, 47, 51, 55 |  |
| select_distinct_on | 23 | 0, 2, 5, 7 |  |
| subselect | 356 | 36, 37, 56, 57, 58, 59, 98, 99, 177, 178, 179, 180, 193, 194, 197, 198, 202, 203, 205, 206, 216, 217, 224, 225, 235, 237, 238, 240, 241, 290, 291, 321, 322, 323, 324, 325, 326, 327, 328, 329, 330, 331, 332, 333, 335, 336, 337, 345, 347, 348, 349, 350, 351, 352, 354 |  |
| union | 198 | 40, 41, 42, 43, 46, 52, 53, 54, 55, 56, 57, 58, 195, 196 |  |
| join | 925 | 66, 111, 112, 116, 122, 123, 125, 126, 130, 136, 137, 142, 145, 146, 150, 218, 223, 224, 238, 239, 240, 241, 242, 244, 246, 276, 277, 314, 315, 340, 341, 342, 343, 344, 345, 347, 348, 378, 379, 380, 381, 383, 384, 385, 386, 399, 401, 402, 403, 404, 405, 406, 407, 408, 409, 410, 411, 412, 413, 416, 417, 418, 419, 420, 424, 452, 453, 454, 455, 462, 463, 464, 465, 466, 467, 468, 469, 494, 543, 548, 549, 586, 725, 726, 727, 728, 729, 730, 731, 734, 735, 736, 737, 740, 741, 742, 743, 781, 782, 888, 892, 919, 920, 921, 922, 923, 924 |  |
| aggregates | 556 | 4, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 48, 49, 50, 51, 74, 75, 76, 77, 78, 79, 80, 81, 82, 101, 102, 103, 104, 105, 106, 107, 108, 109, 111, 113, 130, 131, 132, 133, 134, 135, 136, 137, 140, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 160, 352, 353, 354, 355, 361, 362, 374, 375, 376, 377, 382, 383, 384, 386, 390, 403, 443, 488, 489, 505, 506, 507, 508, 517 |  |
| transactions | 433 | 212, 214, 216, 220, 222, 225 |  |
| portals | 354 | 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 92, 93, 99, 100, 102, 103, 104, 109, 110, 112, 113, 114, 124, 292, 294, 295, 296 |  |
| arrays | 519 | 4, 172, 173, 174, 175, 176, 177, 178, 179, 180, 182, 183, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 197, 396, 397, 398 |  |
| btree_index | 135 | 6, 8, 10, 12, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 47, 48, 53, 55, 57, 59 |  |
| hash_index | 102 | 6, 7, 8, 9, 22, 24, 26, 28, 30, 31, 33, 35, 37, 41, 43, 45, 47 |  |
| brin | 77 | 36 |  |
| tablesample | 58 | 34, 35, 37, 38, 40, 41 |  |
| groupingsets | 217 | 38, 39, 42, 43, 46, 47, 48, 49, 50, 51, 61, 78, 94, 95, 98, 99, 100, 101, 102, 103, 144, 145, 146, 147, 151, 152, 154 |  |
| join_hash | 279 | 276, 277 |  |
| brin_multi | 172 | 26 |  |
| misc | 68 | 23, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 62, 66, 67 |  |
| misc_functions | 148 | 98, 101 |  |
| tidscan | 49 | 38, 39, 41, 42 |  |
| incremental_sort | 120 | 99, 100, 101, 102, 103, 104, 105, 106, 107, 119 |  |
| psql | 1264 | 752, 755, 757, 758, 759, 761, 762, 935, 938 |  |
| select_parallel | 250 | 38, 39, 41, 42, 46, 47, 48, 49, 50, 52, 53, 56, 57, 64, 65, 73, 74, 75, 76, 77, 78, 80, 81, 82, 83, 89, 90, 104, 105, 112, 116, 126, 127, 131, 132, 135, 136, 138, 140, 141, 143, 144, 147, 148, 151, 152, 153, 154, 156, 157, 159, 160, 164, 167, 170, 174, 188, 191, 192, 195, 199, 203, 205, 207, 208, 217 |  |
| write_parallel | 22 | 5, 8, 11, 18 |  |
| select_views | 52 | 0, 2 |  |
| portals_p2 | 41 | 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26 |  |
| tsearch | 451 | 9, 11, 12, 13, 14, 15, 16, 17, 18, 20, 21, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 40, 41, 42, 43, 44, 45, 46, 47, 49, 50, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 67, 68, 69, 70, 71, 72, 73, 74, 76, 77, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 100, 101, 102, 103, 104, 105, 106, 107, 109, 110, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 128, 129, 130, 131, 132, 133, 134, 135, 137, 138, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 160, 161, 162, 163, 164, 165, 166, 167, 169, 170, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 185, 187, 190 |  |
| window | 400 | 4, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 34, 35, 38, 43, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 129, 130, 133, 134, 137, 138, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 264, 265, 325, 326, 392, 393, 394, 395 |  |
| jsonb | 1090 | 3, 197, 199, 532, 533, 534, 535, 536, 537, 539, 540, 541, 542, 543, 544, 545, 546, 547, 549, 550, 551, 552, 553, 554, 555, 556, 557, 562, 563, 564, 565, 566, 567, 568, 569, 570, 572, 573, 575, 576, 577, 578, 579, 580, 581, 582, 583, 584, 585, 586, 587, 588, 590, 591, 593, 594, 595, 596, 597, 598, 599, 600, 601, 602, 605, 607, 609, 610, 611, 613, 616, 625, 626, 630, 631, 632, 633, 634, 635, 636, 637, 638, 639, 640, 641, 642, 643, 644, 645, 646, 647, 648, 650, 651, 652, 653, 654, 655, 656, 657, 658, 659 |  |
| plancache | 97 | 32 |  |
| limit | 85 | 0, 1, 2, 3, 5, 6, 7, 8, 52, 53, 54, 56, 57, 58, 59, 61, 66, 67, 68, 69, 70, 71 |  |
| plpgsql | 941 | 414, 453 |  |
| prepare | 33 | 17, 24 |  |
| alter_table | 1749 | 140 |  |
| rowtypes | 237 | 59, 60, 61, 62, 63, 64, 65, 66, 67, 204, 205, 206, 207 |  |
| largeobject | 151 | 76, 79, 80, 81, 82, 83, 84, 85, 86, 87, 90, 91, 94, 96, 98, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112 |  |
| with | 307 | 32, 50, 51, 52 |  |
| xml | 281 | 30, 120 |  |
| explain | 60 | 22, 42 |  |
| memoize | 73 | 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 23, 55, 56, 67, 68 |  |
| stats | 482 | 57, 59, 65, 66 |  |

## Limits of the measurement

- Files run serially; pg_regress runs most groups in parallel, so tests that look at concurrent sessions behave as in a serial run on both sides.
- The reference container has no `regress.so`, so `CREATE FUNCTION ... AS :'regresslib'` fails on both sides and later statements using those functions (`widget_in`, `interpt_pp`, `binary_coercible`, `test_canonicalize_path`, ...) error on the reference too. Those units are classed *by design*; where both sides fail identically they count as matching.
- Server-side files (`COPY ... TO :'filename'`, `pg_read_file`, `lo_export`) are on the host for the engine (in `PGREGRESS_DIR`, allowed with `--allow-server-files`) and in the container for the reference; psql-side files (`\copy`, `\o`, `\lo_import`) are always in the container, so a `\lo_import` of a file the server wrote reads the reference's copy (and one-line server-side COPYs of a file psql wrote run as `\copy`).
- The database is `regression_a162_resume`; `GRANT ... ON DATABASE regression` and psql's `\d host.regression.x` patterns are rewritten. When comparing with `expected/*.out` the name is mapped back.
- A unit's output includes everything psql printed until the next unit, so one wrong statement in a multi-statement line marks the whole line. Pipeline tests (`\startpipeline`) print results at `\endpipeline`, so differences land on that unit.
- Units after a `\quit` that the reference reached but the engine did not count as *not reached*; units neither side reached are not counted.

