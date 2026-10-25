# PostgreSQL regression suite: engine parity report

Re-run (about an hour; needs the `roll-engine-ref` container, or a private reference: HANDOFF.md):

```sh
CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
```

Rebuild only this report from the saved outputs in `$PGREGRESS_DIR/out`: add `PGREGRESS_REPORT_ONLY=1`. Run a subset (after `test_setup`): `PGREGRESS_ONLY=int4,join` (its report goes to `$PGREGRESS_DIR/report`). A full run writes this report, `results.tsv` and STATUS.md's generated blocks (`PGREGRESS_UPDATE_STATUS=1` when its report went elsewhere). The harness is `crates/pg-engine/tests/harness/regress.rs`; its header documents the method.

Run: full schedule, commit `242734d`, 2026-10-25 02:05 UTC, reference container `a50-ref1`, `PGREGRESS_TIMEOUT=300`; cluster-global objects the reference had before the run: none.

Suite: `src/test/regress` at `REL_18_6`, `parallel_schedule` run serially in one database. Reference: PostgreSQL 18.6 (`a50-ref1`). Both outputs come from the same `psql -X -a -q` with pg_regress's environment. A *unit* is one psql execution unit: a statement (or several on one line), a meta-command, or a COPY with its data. A unit matches when its whole psql output (rows, tags, notices, errors) is byte-identical to the reference's.

## Summary

| | |
|---|---|
| files | 230 |
| units compared | 51150 |
| units matching | 51142 (**100.0%**) |
| units matching, excluding by-design | 100.0% (51142/51150) |
| files fully matching | 227 |
| one-line server-side `COPY ... :'file'` of a file psql wrote, run as `\copy` on both sides | 4 |
| reference output == expected/*.out | 203 of 230 files |
| engine output == expected/*.out (pg_regress "ok") | 200 files |
| engine restarted after (crash/timeout) | none |
| engine panics logged | 0 |

## Differences by class

| class | units | % of differing |
|---|---:|---:|
| wrong EXPLAIN | 7 | 87.5% |
| wrong result (rows / command output differ) | 1 | 12.5% |

## Gap categories, ranked by units affected

Quoted names and numbers are masked (`"…"`, `N`) so similar messages group together. *Feature missing* entries are the engine's error. Many `relation "…" does not exist` units are knock-on effects of an earlier failed CREATE.

| # | units | files | class | category | example files | example statement |
|---:|---:|---:|---|---|---|---|
| 1 | 7 | 2 | wrong EXPLAIN | `EXPLAIN plan text differs` | join (6), partition_join (1) | join:1411 `explain (verbose, costs off)` |
| 2 | 1 | 1 | wrong result | `SELECT: row count differs` | partition_prune (1) | partition_prune:737 `select explain_analyze('` |

## Most frequent engine errors, unmasked (does not exist / not supported / syntax)

| units | engine error | files |
|---:|---|---|

## Per file

`ref=exp` / `eng=exp`: output identical to `expected/<name>[_N].out`. Hunks: `tests/regress/hunks/<name>.diff` (`-` reference, `+` engine).

| file | units | match | rate | engine run | secs (ref/engine) | ref=exp | eng=exp | top gap |
|---|---:|---:|---:|---|---|---|---|---|
| test_setup | 79 | 79 | 100% | ok | 0.4/3.8 | no | no |  |
| boolean | 99 | 99 | 100% | ok | 0.3/0.4 | yes | yes |  |
| char | 32 | 32 | 100% | ok | 0.1/0.3 | yes | yes |  |
| name | 40 | 40 | 100% | ok | 0.1/0.4 | yes | yes |  |
| varchar | 22 | 22 | 100% | ok | 0.2/0.3 | yes | yes |  |
| text | 73 | 73 | 100% | ok | 0.1/0.3 | yes | yes |  |
| int2 | 76 | 76 | 100% | ok | 0.1/0.3 | yes | yes |  |
| int4 | 94 | 94 | 100% | ok | 0.1/0.3 | yes | yes |  |
| int8 | 174 | 174 | 100% | ok | 0.2/0.5 | yes | yes |  |
| oid | 37 | 37 | 100% | ok | 0.1/0.3 | yes | yes |  |
| float4 | 100 | 100 | 100% | ok | 0.2/0.4 | yes | yes |  |
| float8 | 184 | 184 | 100% | ok | 0.2/0.5 | yes | yes |  |
| bit | 133 | 133 | 100% | ok | 0.3/0.7 | yes | yes |  |
| numeric | 1058 | 1058 | 100% | ok | 0.4/23.6 | yes | yes |  |
| txid | 47 | 47 | 100% | ok | 0.2/0.3 | yes | yes |  |
| uuid | 68 | 68 | 100% | ok | 0.2/0.7 | yes | yes |  |
| enum | 172 | 172 | 100% | ok | 0.2/1.2 | yes | yes |  |
| money | 113 | 113 | 100% | ok | 0.1/0.4 | yes | yes |  |
| rangetypes | 419 | 419 | 100% | ok | 0.7/14.5 | yes | yes |  |
| pg_lsn | 31 | 31 | 100% | ok | 0.2/0.5 | yes | yes |  |
| regproc | 107 | 107 | 100% | ok | 0.2/0.5 | yes | yes |  |
| strings | 504 | 504 | 100% | ok | 0.2/1.0 | yes | yes |  |
| md5 | 14 | 14 | 100% | ok | 0.1/0.3 | yes | yes |  |
| numerology | 89 | 89 | 100% | ok | 0.2/0.4 | yes | yes |  |
| point | 43 | 43 | 100% | ok | 0.3/0.6 | yes | yes |  |
| lseg | 16 | 16 | 100% | ok | 0.2/0.3 | yes | yes |  |
| line | 35 | 35 | 100% | ok | 0.2/0.3 | yes | yes |  |
| box | 101 | 101 | 100% | ok | 0.4/5.0 | yes | yes |  |
| path | 23 | 23 | 100% | ok | 0.2/0.3 | yes | yes |  |
| polygon | 62 | 62 | 100% | ok | 0.3/8.1 | yes | yes |  |
| circle | 22 | 22 | 100% | ok | 0.2/0.3 | yes | yes |  |
| date | 271 | 271 | 100% | ok | 0.2/0.5 | yes | yes |  |
| time | 44 | 44 | 100% | ok | 0.1/0.3 | yes | yes |  |
| timetz | 57 | 57 | 100% | ok | 0.1/0.3 | yes | yes |  |
| timestamp | 177 | 177 | 100% | ok | 0.5/0.9 | yes | yes |  |
| timestamptz | 406 | 406 | 100% | ok | 0.5/1.3 | yes | yes |  |
| interval | 446 | 446 | 100% | ok | 0.3/0.8 | yes | yes |  |
| inet | 116 | 116 | 100% | ok | 0.2/0.6 | yes | yes |  |
| macaddr | 35 | 35 | 100% | ok | 0.1/0.4 | yes | yes |  |
| macaddr8 | 71 | 71 | 100% | ok | 0.1/0.4 | yes | yes |  |
| multirangetypes | 619 | 619 | 100% | ok | 0.4/4.7 | yes | yes |  |
| geometry | 162 | 162 | 100% | ok | 0.3/0.8 | yes | yes |  |
| horology | 399 | 399 | 100% | ok | 0.4/0.6 | yes | yes |  |
| tstypes | 238 | 238 | 100% | ok | 0.2/0.4 | yes | yes |  |
| regex | 105 | 105 | 100% | ok | 0.4/1.8 | yes | yes |  |
| type_sanity | 66 | 66 | 100% | ok | 0.2/1.4 | no | no |  |
| opr_sanity | 133 | 133 | 100% | ok | 0.5/169.0 | no | no |  |
| misc_sanity | 5 | 5 | 100% | ok | 0.2/0.3 | yes | yes |  |
| comments | 7 | 7 | 100% | ok | 0.2/0.3 | yes | yes |  |
| expressions | 81 | 81 | 100% | ok | 0.2/1.4 | yes | yes |  |
| unicode | 32 | 32 | 100% | ok | 0.2/0.5 | yes | yes |  |
| xid | 84 | 84 | 100% | ok | 0.3/0.4 | yes | yes |  |
| mvcc | 11 | 11 | 100% | ok | 0.2/0.9 | yes | yes |  |
| database | 16 | 16 | 100% | ok | 0.4/1.4 | yes | yes |  |
| stats_import | 150 | 150 | 100% | ok | 0.3/2.0 | yes | yes |  |
| encoding | 81 | 81 | 100% | ok | 0.2/0.5 | no | no |  |
| euc_kr | 5 | 5 | 100% | ok | 0.1/0.2 | yes | yes |  |
| copy | 118 | 118 | 100% | ok | 0.3/2.4 | yes | yes |  |
| copyselect | 36 | 36 | 100% | ok | 0.2/0.4 | no | no |  |
| copydml | 62 | 62 | 100% | ok | 0.2/0.7 | yes | yes |  |
| copyencoding | 22 | 22 | 100% | ok | 0.1/0.3 | yes | yes |  |
| insert | 399 | 399 | 100% | ok | 1.7/6.0 | no | no |  |
| insert_conflict | 261 | 261 | 100% | ok | 0.4/2.1 | yes | yes |  |
| create_function_c | 10 | 10 | 100% | ok | 0.1/0.2 | no | no |  |
| create_misc | 88 | 88 | 100% | ok | 0.2/0.7 | yes | yes |  |
| create_operator | 90 | 90 | 100% | ok | 0.2/0.6 | yes | yes |  |
| create_procedure | 115 | 115 | 100% | ok | 0.2/1.3 | yes | yes |  |
| create_table | 348 | 348 | 100% | ok | 0.7/8.1 | yes | yes |  |
| create_type | 88 | 88 | 100% | ok | 0.2/0.9 | no | no |  |
| create_schema | 30 | 30 | 100% | ok | 0.2/1.2 | yes | yes |  |
| create_index | 708 | 708 | 100% | ok | 1.2/25.7 | yes | yes |  |
| create_index_spgist | 202 | 202 | 100% | ok | 0.6/14.0 | yes | yes |  |
| create_view | 343 | 343 | 100% | ok | 0.6/11.2 | no | no |  |
| index_including | 137 | 137 | 100% | ok | 0.3/2.6 | yes | yes |  |
| index_including_gist | 52 | 52 | 100% | ok | 0.4/3.3 | yes | yes |  |
| create_aggregate | 60 | 60 | 100% | ok | 0.2/1.1 | yes | yes |  |
| create_function_sql | 168 | 168 | 100% | ok | 0.3/8.5 | yes | yes |  |
| create_cast | 24 | 24 | 100% | ok | 0.2/0.5 | yes | yes |  |
| constraints | 607 | 607 | 100% | ok | 1.1/15.1 | yes | yes |  |
| triggers | 1041 | 1041 | 100% | ok | 1.7/16.5 | no | no |  |
| select | 87 | 87 | 100% | ok | 0.2/1.3 | yes | yes |  |
| inherit | 934 | 934 | 100% | ok | 1.5/22.0 | yes | yes |  |
| typed_table | 36 | 36 | 100% | ok | 0.2/1.7 | yes | yes |  |
| vacuum | 328 | 328 | 100% | ok | 0.6/23.0 | yes | yes |  |
| drop_if_exists | 161 | 161 | 100% | ok | 0.2/2.0 | yes | yes |  |
| updatable_views | 1109 | 1109 | 100% | ok | 0.9/10.7 | no | no |  |
| roleattributes | 80 | 80 | 100% | ok | 0.2/1.1 | yes | yes |  |
| create_am | 142 | 142 | 100% | ok | 0.3/1.6 | yes | yes |  |
| hash_func | 43 | 43 | 100% | ok | 0.2/0.4 | yes | yes |  |
| errors | 87 | 87 | 100% | ok | 0.2/0.3 | yes | yes |  |
| infinite_recurse | 8 | 8 | 100% | ok | 0.6/0.3 | yes | yes |  |
| sanity_check | 3 | 3 | 100% | ok | 0.3/2.6 | yes | yes |  |
| select_into | 67 | 67 | 100% | ok | 0.2/1.0 | yes | yes |  |
| select_distinct | 101 | 101 | 100% | ok | 0.3/1.5 | yes | yes |  |
| select_distinct_on | 23 | 23 | 100% | ok | 0.1/0.7 | yes | yes |  |
| select_implicit | 44 | 44 | 100% | ok | 0.1/0.5 | yes | yes |  |
| select_having | 23 | 23 | 100% | ok | 0.1/0.3 | yes | yes |  |
| subselect | 356 | 356 | 100% | ok | 0.5/84.8 | yes | yes |  |
| union | 198 | 198 | 100% | ok | 0.3/7.0 | yes | yes |  |
| case | 64 | 64 | 100% | ok | 0.2/0.6 | yes | yes |  |
| join | 925 | 919 | 99% | ok | 1.4/16.7 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (6) |
| aggregates | 556 | 556 | 100% | ok | 0.9/15.1 | yes | yes |  |
| transactions | 433 | 433 | 100% | ok | 0.2/1.3 | yes | yes |  |
| random | 48 | 48 | 100% | ok | 0.2/0.8 | yes | yes |  |
| portals | 354 | 354 | 100% | ok | 0.2/1.5 | yes | yes |  |
| arrays | 519 | 519 | 100% | ok | 0.3/1.9 | yes | yes |  |
| btree_index | 135 | 135 | 100% | ok | 1.0/20.8 | yes | yes |  |
| hash_index | 102 | 102 | 100% | ok | 0.6/19.3 | yes | yes |  |
| update | 289 | 289 | 100% | ok | 0.8/6.6 | yes | yes |  |
| delete | 10 | 10 | 100% | ok | 0.2/0.4 | yes | yes |  |
| namespace | 42 | 42 | 100% | ok | 0.2/1.0 | yes | yes |  |
| prepared_xacts | 2 | 2 | 100% | ok | 0.1/0.3 | yes | yes |  |
| brin | 77 | 77 | 100% | ok | 0.6/9.4 | yes | yes |  |
| gin | 59 | 59 | 100% | ok | 1.1/43.1 | yes | yes |  |
| gist | 69 | 69 | 100% | ok | 0.7/10.6 | yes | yes |  |
| spgist | 31 | 31 | 100% | ok | 0.4/4.1 | yes | yes |  |
| privileges | 1398 | 1398 | 100% | ok | 1.0/32.8 | no | no |  |
| init_privs | 4 | 4 | 100% | ok | 0.2/0.3 | yes | yes |  |
| security_label | 28 | 28 | 100% | ok | 0.1/0.6 | yes | yes |  |
| collate | 149 | 149 | 100% | ok | 0.2/1.5 | yes | yes |  |
| matview | 189 | 189 | 100% | ok | 0.5/4.4 | yes | yes |  |
| lock | 134 | 134 | 100% | ok | 0.3/1.3 | no | no |  |
| replica_identity | 75 | 75 | 100% | ok | 0.3/2.9 | yes | yes |  |
| rowsecurity | 1226 | 1226 | 100% | ok | 0.7/29.6 | yes | yes |  |
| object_address | 85 | 85 | 100% | ok | 0.2/1.4 | yes | yes |  |
| tablesample | 58 | 58 | 100% | ok | 0.2/1.4 | yes | yes |  |
| groupingsets | 217 | 217 | 100% | ok | 1.0/3.7 | yes | yes |  |
| drop_operator | 12 | 12 | 100% | ok | 0.1/0.5 | yes | yes |  |
| password | 53 | 53 | 100% | ok | 0.3/2.7 | yes | yes |  |
| identity | 288 | 288 | 100% | ok | 0.6/6.3 | yes | yes |  |
| generated_stored | 468 | 468 | 100% | ok | 0.8/13.9 | yes | yes |  |
| join_hash | 279 | 279 | 100% | ok | 6.3/15.3 | yes | yes |  |
| brin_bloom | 40 | 40 | 100% | ok | 0.2/2.5 | yes | yes |  |
| brin_multi | 172 | 172 | 100% | ok | 0.5/9.5 | yes | yes |  |
| create_table_like | 191 | 191 | 100% | ok | 0.6/10.7 | yes | yes |  |
| alter_generic | 336 | 336 | 100% | ok | 0.3/8.6 | no | no |  |
| alter_operator | 65 | 65 | 100% | ok | 0.2/1.8 | yes | yes |  |
| misc | 68 | 68 | 100% | ok | 0.8/1.6 | no | no |  |
| async | 11 | 11 | 100% | ok | 0.2/0.3 | yes | yes |  |
| dbsize | 25 | 25 | 100% | ok | 0.2/0.4 | yes | yes |  |
| merge | 572 | 572 | 100% | ok | 0.6/16.5 | yes | yes |  |
| misc_functions | 148 | 148 | 100% | ok | 0.3/1.3 | no | no |  |
| sysviews | 29 | 29 | 100% | ok | 0.3/0.5 | yes | yes |  |
| tsrf | 74 | 74 | 100% | ok | 0.2/0.7 | yes | yes |  |
| tid | 41 | 41 | 100% | ok | 0.2/1.2 | yes | yes |  |
| tidscan | 49 | 49 | 100% | ok | 0.2/1.1 | yes | yes |  |
| tidrangescan | 45 | 45 | 100% | ok | 0.2/0.5 | yes | yes |  |
| collate.utf8 | 61 | 61 | 100% | ok | 0.2/0.7 | yes | yes |  |
| collate.icu.utf8 | 681 | 681 | 100% | ok | 1.1/7.8 | yes | yes |  |
| incremental_sort | 120 | 120 | 100% | ok | 0.3/3.4 | yes | yes |  |
| create_role | 144 | 144 | 100% | ok | 0.2/8.3 | yes | yes |  |
| without_overlaps | 674 | 674 | 100% | ok | 1.2/15.7 | yes | yes |  |
| generated_virtual | 489 | 489 | 100% | ok | 0.8/15.7 | yes | yes |  |
| rules | 632 | 632 | 100% | ok | 0.7/15.8 | yes | yes |  |
| psql | 1264 | 1264 | 100% | ok | 0.8/22.0 | no | no |  |
| psql_crosstab | 32 | 32 | 100% | ok | 0.2/0.6 | yes | yes |  |
| psql_pipeline | 362 | 362 | 100% | ok | 0.2/0.7 | yes | yes |  |
| amutils | 10 | 10 | 100% | ok | 0.1/1.4 | yes | yes |  |
| stats_ext | 878 | 878 | 100% | ok | 1.9/60.9 | yes | yes |  |
| collate.linux.utf8 | 2 | 2 | 100% | ok | 0.2/0.3 | yes | yes |  |
| select_parallel | 250 | 250 | 100% | ok | 3.2/44.1 | yes | yes |  |
| write_parallel | 22 | 22 | 100% | ok | 0.3/1.7 | yes | yes |  |
| vacuum_parallel | 14 | 14 | 100% | ok | 0.3/6.3 | yes | yes |  |
| maintain_every | 16 | 16 | 100% | ok | 0.2/2.4 | yes | yes |  |
| publication | 781 | 781 | 100% | ok | 3.4/37.6 | yes | yes |  |
| subscription | 179 | 179 | 100% | ok | 0.2/3.0 | no | no |  |
| select_views | 52 | 52 | 100% | ok | 0.2/1.4 | no | no |  |
| portals_p2 | 41 | 41 | 100% | ok | 0.2/0.3 | yes | yes |  |
| foreign_key | 1267 | 1267 | 100% | ok | 2.4/38.8 | yes | yes |  |
| cluster | 205 | 205 | 100% | ok | 0.5/9.6 | yes | yes |  |
| dependency | 68 | 68 | 100% | ok | 0.2/6.1 | yes | yes |  |
| guc | 222 | 222 | 100% | ok | 0.2/1.7 | yes | yes |  |
| bitmapops | 12 | 12 | 100% | ok | 0.3/10.0 | yes | yes |  |
| combocid | 62 | 62 | 100% | ok | 0.2/0.6 | yes | yes |  |
| tsearch | 451 | 451 | 100% | ok | 0.5/9.3 | yes | yes |  |
| tsdicts | 131 | 131 | 100% | ok | 0.2/1.3 | yes | yes |  |
| foreign_data | 624 | 624 | 100% | ok | 0.8/31.9 | no | no |  |
| window | 400 | 400 | 100% | ok | 0.4/3.9 | yes | yes |  |
| xmlmap | 40 | 40 | 100% | ok | 0.2/1.4 | no | no |  |
| functional_deps | 40 | 40 | 100% | ok | 0.2/2.1 | yes | yes |  |
| advisory_lock | 38 | 38 | 100% | ok | 0.1/0.3 | yes | yes |  |
| indirect_toast | 31 | 31 | 100% | ok | 0.2/2.0 | no | no |  |
| equivclass | 96 | 96 | 100% | ok | 0.2/2.6 | yes | yes |  |
| json | 469 | 469 | 100% | ok | 0.2/2.6 | yes | yes |  |
| jsonb | 1090 | 1090 | 100% | ok | 0.5/13.7 | yes | yes |  |
| json_encoding | 47 | 47 | 100% | ok | 0.1/0.3 | yes | yes |  |
| jsonpath | 224 | 224 | 100% | ok | 0.2/0.5 | yes | yes |  |
| jsonpath_encoding | 35 | 35 | 100% | ok | 0.1/0.3 | yes | yes |  |
| jsonb_jsonpath | 836 | 836 | 100% | ok | 0.3/1.6 | yes | yes |  |
| sqljson | 242 | 242 | 100% | ok | 0.2/3.0 | yes | yes |  |
| sqljson_queryfuncs | 318 | 318 | 100% | ok | 0.2/3.8 | yes | yes |  |
| sqljson_jsontable | 131 | 131 | 100% | ok | 0.2/3.9 | yes | yes |  |
| plancache | 97 | 97 | 100% | ok | 0.3/5.9 | yes | yes |  |
| limit | 85 | 85 | 100% | ok | 0.2/2.6 | yes | yes |  |
| plpgsql | 941 | 941 | 100% | ok | 0.7/27.3 | yes | yes |  |
| copy2 | 290 | 290 | 100% | ok | 0.2/3.9 | no | no |  |
| temp | 211 | 211 | 100% | ok | 0.3/17.8 | yes | yes |  |
| domain | 486 | 486 | 100% | ok | 0.5/14.7 | no | no |  |
| rangefuncs | 428 | 428 | 100% | ok | 0.5/15.8 | yes | yes |  |
| prepare | 33 | 33 | 100% | ok | 0.2/0.7 | yes | yes |  |
| conversion | 68 | 68 | 100% | ok | 0.2/3.1 | no | no |  |
| truncate | 193 | 193 | 100% | ok | 0.4/5.7 | yes | yes |  |
| alter_table | 1749 | 1749 | 100% | ok | 2.6/110.1 | yes | yes |  |
| sequence | 265 | 265 | 100% | ok | 0.3/13.4 | no | no |  |
| polymorphism | 426 | 426 | 100% | ok | 0.3/12.4 | yes | yes |  |
| rowtypes | 237 | 237 | 100% | ok | 0.3/3.6 | yes | yes |  |
| returning | 145 | 145 | 100% | ok | 0.2/3.5 | yes | yes |  |
| largeobject | 151 | 151 | 100% | ok | 0.3/1.8 | yes | yes |  |
| with | 307 | 307 | 100% | ok | 0.4/5.4 | yes | yes |  |
| xml | 281 | 281 | 100% | ok | 2.6/3.0 | yes | yes |  |
| partition_join | 614 | 613 | 100% | ok | 1.2/37.6 | yes | no | wrong EXPLAIN `EXPLAIN plan text differs` (1) |
| partition_prune | 771 | 770 | 100% | ok | 1.5/26.8 | yes | no | wrong result `SELECT: row count differs` (1) |
| reloptions | 66 | 66 | 100% | ok | 0.2/2.1 | yes | yes |  |
| hash_part | 29 | 29 | 100% | ok | 0.1/1.0 | yes | yes |  |
| indexing | 645 | 645 | 100% | ok | 1.1/54.5 | yes | yes |  |
| partition_aggregate | 137 | 137 | 100% | ok | 0.8/17.6 | yes | yes |  |
| partition_info | 73 | 73 | 100% | ok | 0.2/3.4 | yes | yes |  |
| tuplesort | 108 | 108 | 100% | ok | 1.0/33.4 | yes | yes |  |
| explain | 60 | 60 | 100% | ok | 0.2/1.7 | yes | yes |  |
| memoize | 73 | 73 | 100% | ok | 0.3/9.8 | yes | yes |  |
| stats | 482 | 482 | 100% | ok | 0.8/82.9 | yes | yes |  |
| predicate | 42 | 42 | 100% | ok | 0.2/1.4 | yes | yes |  |
| numa | 3 | 3 | 100% | ok | 0.1/0.2 | yes | yes |  |
| compression | 102 | 102 | 100% | ok | 0.3/7.6 | yes | yes |  |
| compression_pglz | 21 | 21 | 100% | ok | 0.1/0.3 | no | no |  |
| oidjoins | 1 | 1 | 100% | ok | 0.3/4.1 | yes | yes |  |
| event_trigger | 230 | 230 | 100% | ok | 0.3/18.9 | yes | yes |  |
| event_trigger_login | 14 | 14 | 100% | ok | 0.2/0.9 | yes | yes |  |
| fast_default | 275 | 275 | 100% | ok | 0.4/15.0 | yes | yes |  |
| tablespace | 261 | 261 | 100% | ok | 0.6/14.9 | yes | yes |  |

## Differing units

| file | differing | units (harness class) |
|---|---:|---|
| join | 6 | 360, 362, 363, 368, 783, 808 (wrong EXPLAIN) |
| partition_join | 1 | 204 (wrong EXPLAIN) |
| partition_prune | 1 | 410 (wrong result) |

## Limits of the measurement

- Files run serially; pg_regress runs most groups in parallel, so tests that look at concurrent sessions behave as in a serial run on both sides.
- The reference container has no `regress.so`, so `CREATE FUNCTION ... AS :'regresslib'` fails on both sides and later statements using those functions (`widget_in`, `interpt_pp`, `binary_coercible`, `test_canonicalize_path`, ...) error on the reference too. Those units are classed *by design*; where both sides fail identically they count as matching.
- Server-side files (`COPY ... TO :'filename'`, `pg_read_file`, `lo_export`) are on the host for the engine (in `PGREGRESS_DIR`, allowed with `--allow-server-files`) and in the container for the reference; psql-side files (`\copy`, `\o`, `\lo_import`) are always in the container, so a `\lo_import` of a file the server wrote reads the reference's copy (and one-line server-side COPYs of a file psql wrote run as `\copy`).
- The database is `gxg_regression` (not `regression`); `GRANT ... ON DATABASE regression` and psql's `\d host.regression.x` patterns are rewritten. When comparing with `expected/*.out` the name is mapped back.
- A unit's output includes everything psql printed until the next unit, so one wrong statement in a multi-statement line marks the whole line. Pipeline tests (`\startpipeline`) print results at `\endpipeline`, so differences land on that unit.
- Units after a `\quit` that the reference reached but the engine did not count as *not reached*; units neither side reached are not counted.

