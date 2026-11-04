No blocking correctness bug in the core index-access paths. I did find four places that diverge from PostgreSQL in the new index-row-size checks, two of them medium. Nothing a163 changed explains the `serializable_transfers_retried` 6003/300 result, and a trial merge onto the shared parent is textually clean. This was a static read only, so it can't establish that the source is ready.

**Range note:** `d119ef44..cf27eed0` contains six commits, not five. `9a9fb6e9` adds the bench tool (`tools/bench/access.rs`, `tools/bench/bench.rs`). `d64d55c6` also carries `BENCH.md` and `HANDOFF.md`. A source-only cherry-pick should leave out or settle those docs.

### Findings

**1. Medium — serial `CREATE INDEX` ignores INCLUDE columns in both size checks.**
- **Where:** `cf27eed0` `crates/pg-engine/src/session/ddl/table.rs:3727` and `:3729` call `on_form(...)` and `btree_oversized(...)` with `row = &[]`. `btree_size` (`exec/rowsize.rs:53`) then reads each INCLUDE column as NULL.
- **Inconsistent with:** INSERT, UPDATE and the parallel build (`parbuild.rs:122`, whose `ix_types` and `vals` include the INCLUDE columns).
- **Trigger:** on a small, modelled table, `INSERT (1, a163_long(85))` followed by `CREATE INDEX ON t(id) INCLUDE (t)`. PostgreSQL fails with "index row size … exceeds btree version 4 maximum 2704". The engine builds the index, then a later INSERT of the same row fails. The 8191 limit is missed the same way.
- **Missing test:** an INCLUDE case in `a163_index_row_size.sql`.

**2. Medium/Low — `surely_small` under-estimates `char(n)`.**
- **Where:** `exec/rowsize.rs:30-34`. It counts `Value::Text(s)` as `s.len() + 8`, but `relsize` pads `Bpchar` values to n characters, and in a non-UTF8 database encoding the byte length can also be larger.
- **Trigger:** `CREATE TABLE b(c char(2800)); CREATE INDEX ON b(c); INSERT` a 2650-character md5-hex string. The engine estimates 2674 bytes, skips the real sizing, and accepts the row. PostgreSQL pads to about 2816 bytes, can't compress it, and errors.
- **Fix:** return `false` from the fast path for `Bpchar` keys and for non-UTF8 encodings, or add the padding.
- **Missing test:** a `char(n)` case.

**3. Low — serial unique build reports errors in a different order from PostgreSQL.**
- **Where:** `table.rs:3727` vs `:3749`. The serial step checks for duplicates row by row, in heap order. PostgreSQL forms every index tuple during the heap scan (so the 8191 error can come from any row) and checks uniqueness later, during the sort.
- **Trigger:** rows `'a'`, `'a'`, then one incompressible value over 8191 bytes, then `CREATE UNIQUE INDEX`. PostgreSQL reports "index row requires N bytes"; the engine reports "is duplicated". The parallel path (`parbuild.rs:122`, then `unique_check`) gets it right, so the serial and parallel paths disagree.
- **Missing test:** this case.

**4. Low (plausible) — GIN build reports the first oversized element in heap order.**
- **Where:** `exec/rowsize.rs::gin_entries` as called from the build step. PostgreSQL forms GIN entries when the build accumulator is dumped, in key order, so with several oversized rows its error can name a different size.
- **Missing test:** two oversized rows whose sizes differ.

**5. Low (performance) — reusing the kept measure turns `may_toast` on permanently.**
- **Where:** `vacuum.rs:1530/1560/1633`. When `kept_live` is used, `stored` is always None, so `may_toast` becomes true and is saved into the new `Measured`.
- **Effect:** later VACUUMs with `toast_frozen` lose the kept-measure skip (`:1665`). Results stay correct; the skip optimisation is lost.

**6. Low/info — leftover index entries in existing data directories.**
- **Where:** `exec/dml.rs:1528`. Entries are now deleted only when `stores_entries` is true.
- **Effect:** GiST, GIN, BRIN and other SP-GiST entries written before this patch are never deleted until REINDEX or DROP. Nothing reads them, so this is a storage leak, not a wrong result. There is no upgrade step.

### Checked and found sound
- **`stores_entries` vs. readers:** every path that reads entries is now limited to btree/hash. The exclusion `by_value` check (`dml.rs:1127`) reads entries only for a GiST index with a leading `=` — the case that still stores them. `add_exclusion` and partition cloning attach the constraint before `build_index`, and table rewrites rebind with constraints, so those indexes get their entries.
- **SSI:** `wrote_entry` is still recorded for every index when tracking is on.
- **Speculative insert:** the arbiter pre-check runs first. Size errors after a concurrent conflict match PostgreSQL, which inserts into every index.
- **`written_since` without the rdel guard:** `delete_range` journals the transaction's own writes it removes. `get_latest` and `buf_get` hide committed rows covered by the transaction's own range deletes. `derived` is cleared on rollback.
- **Bitmap filter dropping conditions (`scan.rs:542-556`):**
  - Every bitmap path checks `orig` against the row (or exact-type entry values) at the same snapshot.
  - OR branches are left alone, and the drop is skipped under row marks.
  - EvalPlanQual still has the full conditions: `epq_scan` checks the original `s.filter` and the bitmap quals, and UPDATE/DELETE uses the plan-time `plan.recheck`.
  - **No test covers it** — this is the largest test gap in the batch.
- **KNN over time/interval:** exact distances come from the ORDER BY operator, and `Dist::within` handles f64 rounding. Infinite intervals are covered by the golden.
- **Other areas:** `stored_tuple_len`, `skip_value`, the pgstat batching (readers call `flush_returned`), the ginsim split clamp, and the index-only-run skip in `storage/mod.rs` look correct.

### `serializable_transfers_retried` (6003/300)
The test uses `accounts(id int PRIMARY KEY, balance)` with no secondary indexes. In `update_row_rls`, `changed[i]` is false for the pkey, and a163 touches no lock, first-updater or SSI-commit code. Statically, I can't attribute the result to anything in a163.

### Integration with the shared parent (58ddbac7; 63f7c0b4 and e4214aa4 are ancestors)
- `git merge-tree` of `cf27eed0` onto 58ddbac7 is textually clean. This wrote an unreferenced tree object; no refs or worktree were changed.
- **Overlaps, all in different places:**
  - `dml.rs`: the shared COPY `multi_insert` / `row_done` hooks sit about 25 lines from a163's `at_flush` `PROGRAM_LIMIT_EXCEEDED` change, and they combine correctly.
  - `storage/mod.rs`: the shared `discard` / `read_once` changes are in flush, compaction and scan; a163's change is in the `untouched` handling of `ingest_stream_locked`.
  - `exec/mod.rs`, `vacuum.rs`, `txn/mod.rs`: changes are next to each other but don't overlap.
- The shared side adds no new callers of `index_entry_key`, `written_since`, `SKIPS`, `count_fetched` or `count_blocks`.
- Apply a163 as a patch on top of the newer files. Replacing whole files with the old-base versions would drop `multi_insert`, `copy_batch`, `discard` and `read_once`.

### Not covered
- GiST, SP-GiST and BRIN tuple sizes; the module documents these as accepted gaps.
- Size checks during table rewrites (ALTER TYPE).
- PostgreSQL parity of the golden files: I didn't verify the `.out` files against a real PostgreSQL.
- Performance claims: none were accepted.
