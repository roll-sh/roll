I reviewed the full 9-commit range `5c312235..3bb04519` against shared `eb450f4e`, reading committed objects only. I made no writes, builds, test runs or merge-tree calls, and did not poll any jobs. Nothing below has been run, and it makes no readiness or performance claim.

## High

**H1. Merge hazard with a163 (cf27eed0, not yet in shared): the modelled parallel build would skip the new row-size errors.**
- **Where:** `session/ddl/parbuild.rs:110-119`. The model branch in `Plan::row` returns before the measured branch.
- **What a163 adds:** cf27eed0 puts its `INDEX_SIZE_MASK` form error and its `oversized` tracking (`g.oversized`) after the `index_tuple` computation in that measured branch. It also puts `on_form` / `btree_oversized` in the serial `step` closure in `table.rs`.
- **Why it breaks:** under models170, a modelled table's btree build goes to the parallel path (`table.rs:3767`, `!measures`). That path never calls `step` and returns before a163's per-row check. After a plain textual merge, the leader's `if let Some(..) = oversized` check would still be present, but nothing would ever set it for model rows.
- **Result:** an oversized key on a modelled table builds successfully in parallel, where it should fail with 54000. That happens when workers > 0: ≥1024 pages at the default `min_parallel_table_scan_size`, or `min_parallel_table_scan_size = 0`, or a `parallel_workers` reloption.
- **Expected textual conflict:** `table.rs:3767`. a163 changes the same line to `measures_in_build && ix.method == "btree"`. A correct resolution keeps both conditions: `(measures && ix.method == "btree") || (writes.is_some() && <stores_entries>)`. a163 also replaces `ix.id != 0` with `stores_entries(&bix)`, so models170's `ix.id != 0` gate should follow that.
- **Not affected:** a163's Bpchar fast path in `storage/row.rs` (`stored_tuple_len`) only feeds the measured branch. The models170 model branch never calls it.
- **Status:** the reasoning is concrete. Nothing has been merged or run.

## Medium

**M1. `kept::laid_out`'s debug assertion can fire when a table that was planned from its header is then laid out from its rows.**
- **Where:** `kept.rs:249-256`, called from `ensure` at `sizing.rs:1758`.
- **Mechanism:**
  1. The planner reads `kept::sizes`, which inserts `kept_sizes`.
  2. If `ensure` then lays out from rows instead of from the record, `laid_out` compares the header to a fresh layout. A fresh layout uses `heapsim::restored` with no shape, and new `Btree::build`s replace the kept images.
  3. Pages from a fresh layout can differ from the kept shape and kept btree images: trailing empty pages left by VACUUM, btree pages that split or were deleted.
- **Ways to reach the rows path:**
  - `take` returns `Take::None` because `need` lists a btree that the record lacks. `persist_inner` encodes any modelled heap, and `encode` only writes the btrees that are present.
  - The background `lay_out` bails at `kept.rs:290` (`moved || read_shape none`). `start` has already removed `kept`, so the foreground then lays out from rows.
- **Status:** the code path is concrete. That either trigger state happens in practice is unproven.
- **Effect:** in debug builds it panics while holding the relfiles write guard. In release, the first statement's plan silently uses sizes that differ from the model it then lays out.

**M2. `sql/explain.rs:11862` reads `btree_blocks(...)` with no kept-header fallback, so it gets `unwrap_or(1)`.**
- Before a29fcfe3, `rel_size` → `heap_blocks` had always laid the model out by this point.
- Now, on a kept table before layout, `index_pages` is 1 while `rel_size` and `btree_level` use the header. `btree_level` only works because `kept_sizes` happens to be filled earlier.
- **Status:** the regression path is concrete. Which EXPLAIN shapes reach that function is not verified.

**M3. `models_kept.rs::kept_sizes_plan_as_the_models_do` can't prove the header path.**
- It races the background builder (`build_in_background`), and nothing checks that `first` was actually planned from `kept::sizes`. It can pass with the header path never exercised, which would also hide M1 and M2.
- **Gate needed:** hold the builder, or add a counter, plus one `Take::None` case.

## Low

- **L1. `sizes_read` (`kept.rs:240`) doesn't check the pending state the way `sizes` does.** If the background builder bails, or `ensure`'s `known_big` path marks the table estimated without calling `forget`, `modelled_blocks` (`sizing.rs:2972`) and `btree_level` keep returning the header. Hypothesis.
- **L2. `read_sizes` (`kept.rs:488`) short-circuits `&&`.** On a relfilenode mismatch the token's 8 bytes aren't consumed, so the "record after them" offset is wrong. It's harmless today only because `decode` rejects the body afterwards. Code smell, no failure found.
- **L3. Commits 45ad80d0 and 055218c7 don't compile.** `reattach` (sizing.rs:4016 at 45ad80d0) still builds a std `HashMap::with_capacity` and assigns it to the `FastMap` field `tt.rows`. 7a6ed0cd fixes it. This only breaks bisecting; squash before landing. Once fixed, `FastState` is Copy and process-seeded, so I see no lifetime problem in the rekey.
- **L4. `btree_from_build` (sizing.rs:1391) is weaker than its doc says.** The doc says it returns None if the heap "has a row the pass didn't". The code only checks that each key's row is in the heap. Now that chunked parallel scans feed it, a cheap guard would be `keys.len() == heap live rows`.
- **L5. `parbuild.rs:283`: `count_index_build()` and worker reservation run before the empty-table `return Ok(None)` at :291.** The pattern isn't new, but it now applies to modelled tables too.
- **L6. `identity_fetched` (sizing.rs:2562) has two minor gaps, neither harmful.**
  - It runs again on each `Locked::Skip`/retry loop, which is harmless.
  - For a replica identity index that isn't the primary key it prunes but marks nothing dead, unlike PG's `_bt_killitems` on that index. Minor parity gap.

## Checked with no defect found

- **3bb repair:** `blocks` (`table.rs:3684`, `heap_blocks_for_build`) gives the same values as `heap_blocks` in each case (untracked, modelled, heap-only), and it's computed before any expression is evaluated. No other new `heap_blocks` call in the build path.
- **Parallel vs serial modelled btree build:**
  - Same storage-key scan order; `scan_snap` includes the transaction's own writes.
  - Key types (`ix_types[n]` for n < nkey) and collations match. `dup_colls` equals `bix.colls` for id != 0, so nondeterministic collations are not an issue.
  - Duplicate choice, NULLS NOT DISTINCT handling, `count_returned` totals and progress totals all match.
- **HOT / `same` fast path:**
  - The variants it handles compare equal exactly when their encodings do.
  - `columns_unchanged ||` in `updated()` only skips evaluating the old row's expression. That is closer to PG.
- **pgstatindex (e86b6794):** matches PG's order (deleted → half-dead → leaf). An empty leaf now adds to leaf free space and fragments. The golden is recorded from PG 18.6.
- **Overlap with shared `eb450f4e`:** only `sql/costs.rs` is touched by both. Shared changes base lines 204-205 and models170 changes line 207, with one unchanged `}` in between. It should merge cleanly, but I didn't verify that. eb450f4e's ANALYZE `heap_tuple_stats` and its `dml.rs` `count_insert` don't touch these paths.

## SSI (scan1496003)

The only SSI-path edits in the range are `scan.rs:988` and `scan.rs:1047`, both adding a `heap_blocks` call before `btree_leaf_pages`. On a database that wasn't reopened from kept records, `heap_blocks_known` returns early and the planner has already laid the model out, so behaviour doesn't change. I found no concrete path linking the range to that failure.

## Missing tests and gates

- A modelled-table btree build with 0 vs 2 maintenance workers (`min_parallel_table_scan_size = 0`) should give the same results: Btree `nblocks`/`height`/`page_stats`, duplicate error text, INCLUDE, NULLS NOT DISTINCT, an in-transaction INSERT followed by CREATE INDEX, and `seq_tup_read`/progress.
- A deterministic kept-header test (see M3), with a `Take::None` case.
- An apply HOT/pages parity test as a test. Today the only check is the bench `apply.sh` printout.
- After the a163 merge, an oversized-key test under a modelled parallel build.
