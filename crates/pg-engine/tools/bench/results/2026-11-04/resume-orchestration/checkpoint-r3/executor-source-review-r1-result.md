## Verdict on executor167 `c1594cc6`, full range from `575ad4b2`

I found no proven high-severity bug in query results. I found one medium bug, which is in EXPLAIN ANALYZE output, plus two latent low-severity issues and four gaps in the evidence. I read committed git objects only and ran nothing: no builds, tests or merges, and no merge objects were written.

### Medium (logic I traced, not run)

**M1. EXPLAIN shows the wrong inner `loops` when a SubPlan's outer row count varies between runs.**
- **Where:** `crates/pg-engine/src/sql/explain.rs:18325-18334`, commit `80a43ef1`.
- **What it does:** it sets `rescans = saved * max(1, round(outer_rows_total / inner_reads))`. PostgreSQL's inner loop count is the total number of outer rows across all runs, with no rounding and no forced minimum of 1.
- **Reproducer:** use the a167 fixture with the same `SET enable_*` flags, then run:
  `EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT o.k, (SELECT count(*) FROM a167_r r JOIN a167_n n ON n.rk = r.rk WHERE r.rk < o.k % 3) FROM a167_o o WHERE o.k <= 6`
  - The outer rows per run are 0, 1, 0, 0, 1, 0, so the total is 2.
  - PostgreSQL shows `loops=2` on the inner scan of `n`.
  - The new formula gives `max(1, round(2/6))` or `round(2/2)`, which is 1 either way, times `saved=6`, so it shows `loops=6`.
  - Uneven runs such as 1 and 2 rows have the same rounding problem: 3/2 rounds to 2, giving 4 instead of 3.
- **Golden:** both a167 plans have exactly the same outer row count in every run, so this case is not covered. Using the raw total outer rows directly gives the same output for both goldens (30 and 20) and is exact.

### Low / latent
- **L1. Possible hang on an empty chunk list.**
  - `hand_out` (`parscan.rs:1562-1579`) only drops `jobs` once it has handed out the last chunk. If `ParScan::start` gets an empty `scans` list with `spec.free`, the workers never see the channel close, and `late.recv()` (`:1614`) blocks forever.
  - **Not reachable today:** `start_parallel` returns early on `scans.is_empty()`, the index path needs non-empty `cuts`, and the keys path needs at least `MIN_KEYS`.
  - It is a trap for any future caller. A guard, or dropping `jobs` when `pending` is already empty in `start`, would remove it.
- **L2. char(n) keys are now assumed always padded.**
  - `shows_alike` (`agg.rs:3707-3711`, commit `b9b251f4`) treats every `Bpchar(n>0)` value as padded to n. This turns off presort on the serial path too, not just the parallel `free` path.
  - It is correct as long as every source pads: `ADD COLUMN … char(n) DEFAULT` missing values, binary COPY / `bpchar_recv`, and `ALTER TYPE` from char(m) to char(n).
  - This is plausible but unproven. No test covers rows from mixed sources.

### What I checked and found correct
- **Chunk ordering and read accounting.**
  - With a carried state, the read count goes out as `Msg::Read` on the chunk's own channel before any part or error, and `pa.read` is never set, so nothing is counted twice.
  - Carried parts have `read=0`.
  - The serial `seg` rows take `carry=0` from `Done(0,0)`.
  - The error query (`100000 / (b.id - 419950)`, a plain aggregate, so `free` is true) uses the carried path, and the test compares its stats against serial (`parallel_scan.rs:329-332`).
- **Late channel lifecycle.**
  - Inline scans set `late=None` and pass `carry=None`.
  - On `Msg::Err`, the receiver clears `late`, and later sends from workers fail quietly.
  - `Drop` keeps `late` alive while joining the workers, so their sends go to an unbounded channel and cannot block.
  - The `jobs` mutex hand-off when the channel closes is fine.
- **Carried path vs `run_job`:** they do the same work. A worker that never got a job has nothing to `take()`, so it sends nothing.
- **Order-free gate.** Carrying is only enabled when all of these hold:
  - not `seq`;
  - every aggregate passes `order_insensitive`; DISTINCT, ORDER BY and FILTER are already excluded earlier by `partial_run`;
  - every key either is redundant or looks alike when equal;
  - the sorted output covers every non-redundant key.
- **Group merge:** the empty `Vec::new()` passed as `hkey` in the non-nondeterministic branch is never read, because `hk0` uses `key` in that case.
- **Vector kernels:**
  - `VN::eval` matches the old `at` for both Out and NULL.
  - Rows that don't fit the vector are excluded from `sel`.
  - `nums` is filled only for live rows and read only for rows that are true and fit.
  - `vplan` caching is safe because each `Decode` always gets the same `spec`.
- **Date/timestamp comparisons (`VB::Instant`):**
  - `date_instant` matches `p_instant`, including out-of-range dates becoming +infinity.
  - The date-vs-date path through `p_eq`/`p_lt` compares the raw day numbers, which matches the generic `cmp`.
  - Timestamptz constants are excluded.
- **Storage columns (`cols.rs`):**
  - The numeric fast path matches `peek_num`: empty means 0, more than 16 bytes means the value doesn't fit, and the sign is extended.
  - The slow path recomputes the position correctly.
  - Block-buffer offsets are relative to each `Bytes` slice, and the buffers are reference-counted, so lifetimes are safe.
  - Truncated fixed-width values now produce an error instead of silently becoming 0, which is an improvement.
- **SSI (Scan1496003):** the candidate does not touch any predicate-lock code, and `parscan.rs` has no SSI hooks. The reordered heap path now uses `ListFill` plus batch DROP instead of `Source::Rows`. The row path it skips does no per-row SSI work, so I found no matching path.

### Tests
- **a167 golden:** I recomputed both plans by hand: rows, loops, rows removed, and every `min(s.v)` value. They are consistent with PostgreSQL semantics. I did not verify that they actually came from PostgreSQL 18.6.
- **`parallel_scan.rs`:** the tests run one at a time under the `ONE` mutex, so the global counters are attributable to the test that reads them.

### Missing evidence
1. Nothing proves the reordered `ListFill` branch actually ran. `started()` and `VECTOR_ROWS` (`:1019-1020`) also go up on the normal stored-order parallel path. The test needs a counter specific to the reordered path. Without one it could pass without testing the path, for example if heap modelling changes.
2. The new grouping tests only compare this engine's serial and parallel results with each other. That does not catch L2, because the presort change affects both.
3. No unit test of the `cols.rs` fast path with negative numerics or with scale/length of `0x80` or more.
4. No golden for M1, i.e. a SubPlan where the outer row count varies between runs.

### Integration with shared `eb450f4e`
- The only file both sides change is `sql/explain.rs`, and the hunks are far apart (shared side at 6078, candidate at 18322).
- The shared side does not touch `exec/{parscan,agg,partial,vector,scan,lookup}`, `storage/{cols,batch,row}` or `func/{ops,datetime}`.
- The number of `Decode {` and `Spec {` literals is the same on the shared side, so no new literal needs `vplan` or `free`.
- The shared `sizing.rs` change (`CopyLoad::row_done` now returns `bool`) and the `dml.rs` change have no callers in the candidate.
- e4214aa4's `in_read_once` in `sst.rs` is independent. `ColBatch` now holds reference-counted block `Bytes`, which is safe.
- The verified162 changes (vacuum/analyze dead-tuple statistics) don't affect how user heaps are reordered.
- A scoped integration should keep e4214aa4 and the 162 files exactly as they are. I expect it to merge cleanly as text.

### Accepted limitations
- No performance claims.
- The `CARRY_GROUPS`-per-worker change affects memory only.
- This was a static review; I compiled and ran nothing.
