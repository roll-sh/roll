# Third independent storage review: candidate `79746f31` vs merge base `5c312235`

**Verdict:** I found no new HIGH or MEDIUM issues in the code. The change can't be called source-ready, though, because no gate has run on this exact source yet.

**What I checked.** This was a read-only review of committed objects only. HEAD is `79746f31`, and the tree has one untracked `crates/pg-engine/results/` directory, which I left alone. Ancestry checks out: `fadf2751` → `8c4b686b` → `79746f31`, merge base `5c312235` contains `e4214aa4`, and `79746f31` is an ancestor of `aa06109e`, which is on `origin/wip/resume-20261104-a169`. Beyond the `results/` files, `aa06109e` changes no source.

## Findings on `79746f31`

**LOW-1: no closed check in the close-time rewrite.** In `storage/mod.rs:1833-1860`, `rewrite_linked_l1_for_close` takes `bg` but never checks `closed`. `compact` (`:1684-1689`) and `collect_ranges` (`rdel.rs:201-204`) both return early once the store is closed, because "a closed store's files may belong to a store opened after it". The close gate at `:1870` checks `closed` only before this work begins.
- I can't build a realistic failing case. A concurrent close finishes with no linked L1 left, and once the store is closed, commits are rejected, so the loop would find nothing.
- It is an inconsistency, not a demonstrated bug. A one-line guard under `bg` would match the other two functions.

**LOW-2: a leaked output file if the manifest write fails.** In the same function (`:1840-1855`), if `write_manifest` fails after `rewrite_without` has produced its output, that output's reader is dropped without `mark_obsolete`. The file then stays on disk until the next open removes unlisted SSTs. `compact` behaves the same way, so this isn't a regression.

**LOW-3: the new unit test checks fewer things than it could.** `close_rewrite_preserves_unrelated_l1_and_pending_ingest` (`mod.rs:3202-3242`):
- After reopening, it checks only that the `a` and `z` ranges each scan 256 rows. It doesn't check values or exact `(ts, value)` pairs.
- It calls `rewrite_linked_l1_for_close()` directly rather than going through `close()`.
- The real close path (compact → collect → rewrite) is covered by the integration test `clean_close_rewrites_direct_linked_l1_for_prelinked_readers` (`tests/storage_linked_gc.rs:83-111`). That test does check values, tombstones and compression, but it has a single linked file with no ordinary neighbours.
- So no test proves "ordinary middle file kept" and "values exact" in one end-to-end close. This doesn't block anything.

## What I verified

- **Lock order** is consistent everywhere:
  - The rewrite takes `bg` → `manifest` → `view.write`, the same as `flush_all`, `compact` and `collect_ranges`.
  - `commit_sorted` takes `commit` → `bg.try_lock` → `manifest` → `view`. It never blocks on `bg`, so nothing is inverted.
- **Manifest and view L1 agree.** Only code holding `bg` changes L1: compact, collect, the rewrite, and `commit_sorted`'s direct link, which happens only if `try_lock` succeeds.
  - The rewrite holds `bg` for its whole loop. No new direct L1 file can appear during it, so the loop always ends.
  - `m.clone()` is taken under the manifest lock, so a concurrent ingest's manifest L0 entry is kept. The rewrite never touches L0 or `imm`.
- **Crash consistency:**
  - Outputs are written with `SstWriter::create` (fsynced) under fresh ids, and `write_manifest` fsyncs the directory.
  - A crash before the manifest write leaves an unlisted output, which open deletes (`:371-386`).
  - A crash after it leaves the old linked file unlisted, which open also deletes.
  - Either way, exactly one copy is listed.
- **Timestamps and tombstones.** The rewrite reads through `level_uncached` → `block_with(fill=false)` → `decode_linked`, so keys carry the commit timestamp and run tags are stripped.
  - With an empty deletion list, every entry is kept, tombstones included. The horizon value of `0` is never used.
  - Output `min_ts` and `max_ts` equal the commit timestamp, and the output has no `commit_ts`.
  - Bottom-level tombstone reclamation still happens only in `compact`, past `gc_horizon`, so nothing is reclaimed early.
- **Replay barriers.** After the rewrite the file is no longer a barrier. That is safe because close's `flush_all` has already deleted the flushed WAL files. Only late writers could leave WAL entries, and those have timestamps above the linked commit's.
- **Old readers.** `PointReader` and `Scan` hold their own `Arc<View>`. `mark_obsolete` runs after the view is published, so the file is deleted when its last reader drops it. Each new reader gets a fresh `cache_id`, and `Drop` calls `forget_file`.
- **No unrelated rewrites.** Each linked file is replaced in place, and every other L1 entry keeps its `Arc` and id. A linked direct-L1 file is at most 2 × the target size, so writing it as one output keeps each key's versions in one file. Peak extra disk is one file, plus old files that readers still hold.
- **Other changes since `fadf2751`:** `rewrite_without` is now `pub(super)`. The test fixes (prefix-free `b0000`/`b1999` keys; compression `true` allows compressed blocks while `false` forbids them) match the diagnosis. Exact timestamp assertions are kept and now carry stage labels. There is no format bump, and old builds read `commit_ts` as absent through `serde(default)`.

## Response to each prior finding

| Round | Finding | Status in `79746f31` |
|---|---|---|
| R1 HIGH | Clean close missed direct linked L1 | **Resolved.** The close gate includes `.chain(&v.l1)` (`:1869`), and the rewrite step covers it. |
| R1 MEDIUM | Tests only covered L0, not direct L1 or replay | **Resolved.** `storage_linked_gc` covers direct L1, replay barriers, clean close and compression. |
| R1 | Fresh runs ignored the compression option | **Resolved.** The option is threaded through `create_run`, `alone_at_bottom` checks it, and a test covers it. |
| R1 | Scratch examples, unused `get_snap_with`, global counter, docs | Cleaned up in `fadf2751`, which round 2 validated. Nothing new reappears. |
| R2 MEDIUM-LOW | Stage computed an index against manifest L0 and inserted into view L0 | **Resolved by removal.** The stage is gone and L0/`imm` are untouched. The new unit test reproduces the pending-`Frozen::Sst` state; the old code would have panicked at `l0.insert(1)` on an empty view L0. |
| R2 MEDIUM | Normal compaction rewrote every ordinary L1 file between distant linked files | **Resolved.** Each file is rewritten alone with an empty deletion list, and the unit test asserts the middle file's id survives. |
| R2 LOW | A late commit after compaction can leave linked L0 | **Disclosed** in HANDOFF (lines 3826-3834). I confirmed the gap is inherited: the base's `close` (`5c312235:mod.rs:1867`) has the same window. A late direct L1 file *is* caught, because the loop rescans. |

## Remaining blockers and missing evidence

1. **No gate has run on the exact source `79746f31`.** The committed results under `resume-owner169/pinned-checks` are for source heads `060c6f4f` (`a169-checks`) and `c21a26b1` (`a169-proof`, `a169-proof-fixed`). `storage-release.txt` shows 28 passing tests and does not include `close_rewrite_preserves_unrelated_l1_and_pending_ingest`. Still needed on `79746f31` (runner `3424941`, still in flight per the brief): debug and release proofs, the storage and integration suites, the 120-case crash suite, clippy, the build, and the Postgres e2e (`pnpm test:pg`).
2. **The 127-exit harness failure stays a harness failure.** It isn't acceptance, and nothing should be presented as if it were.
3. **No timing has been accepted.** The baseline has no source-to-binary seal, and there is no fresh accepted timing. The COPY, pgbench-init and CREATE INDEX comparisons and the footprint gate are still on hold, so no performance or readiness claims are allowed.
4. **The late-writer compatibility gap is disclosed, not fixed.** Clean-close compatibility with older builds depends on writers having drained first.

A static read can't establish source readiness. Readiness depends on the `79746f31` gate results above.
