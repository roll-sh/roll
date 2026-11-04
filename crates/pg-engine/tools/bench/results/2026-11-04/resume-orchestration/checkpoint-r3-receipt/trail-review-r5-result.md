I found no blocking or medium defects in the checkpoint-r3 corrections. Both medium findings from review r4 are fixed, and all five lower-severity findings (LOW3–LOW7) are too. Nothing in the checkpoint claims more than was tested, and the changes touch evidence files only. It is clear to commit. I left three new low-severity notes below; none of them blocks the commit.

## What I checked
- **Integrity:** `SHA256SUMS` verifies, and it lists 456 files, which is every file except itself. Those files add up to 256,516 bytes. While I worked, the parent staged exactly the 457 checkpoint-r3 files. Nothing else is staged, the working tree has no unstaged changes, and HEAD is still `eb450f4e`.
- **Copies:** every review result, launch receipt, audit JSON and both service proof directories are byte-identical to the copies in `~/.cache/pg-engine-resume-20261104`. The original proof is unchanged. `verification-readiness.json` is byte-identical to the sealed `4b99a106:…/dump-only-v5/readiness.json`.
- **Trail:** the canonical trail is `~/.cache/pg-engine-sol-resume/decisions.tsv`, 211 lines. Lines 192–211 match the excerpt exactly. Every row from the earlier r1 and r2 excerpts is still present word for word, and timestamps never go backwards. So the trail is append-only, and no earlier timestamp was rewritten.

## The two medium findings from r4
- **MEDIUM1 (review conclusions cited from launch receipts) is fixed.** There are now 7 saved `*-result.json` files. Each one says `completed` and `result_available`, and its summary matches the `.md` beside it apart from a trailing newline. The trail's characterisations match those results: catalog "no new HIGH/MEDIUM", index "INCLUDE/Bpchar", storage "no HIGH/MEDIUM", models "HIGH merge hazard", executor "MEDIUM EXPLAIN". The 4 launch receipts still say `running` and `summary: null`, but they are kept only as launch records.
- **MEDIUM2 (the service suite gating pg-engine) is fixed.** `acceptance-scope.json` sets `does_not_exercise_pg_engine: true` and lists the pg-engine build, debug and release tests, Clippy and focused parity as a separate requirement. Neither gate claims to have run, and `full_product_acceptance` is false everywhere. `scripts/test-postgres.sh:10,29,42` at `eb450f4e` confirms the scoping is right.

## What I reran (fresh /tmp scratch, now deleted)
- **Service proof v2:** I copied only `prove.py` and `hardened.sh`, as instructed. It printed `PASS 15`, and its `results.json` is byte-identical to the saved one. The network-failure case exits 23 before mocked pnpm starts, which proves the real `network_rc` capture works. `hardened.sh` is the same file as the live `verification/check-full-service.sh` (sha `9236a1ff…`). In every case's `command.sh`, only the W and V lines differ from it. e2e.rs at `eb450f4e` has no `#[ignore]`, so requiring `0 ignored` cannot reject a real run that passed.
- **Validator replay:** `replay-validator-controls.py` printed `PASS 10`. Its output is byte-identical to `validator-replay-results/` and to the original saved stdout and stderr files. The bundled `audit-array.py` and `array-header.sql` are identical to the parity-v4 originals.
- **Storage audit:** `audit-storage-final.py` printed PASS with 102 payload blobs, 2367 source entries and 14 exits. Its output JSON is byte-identical to the saved one.

## New low-severity notes (none blocking)
1. **The OLTP summary leaves out the review's two medium findings.** Trail row 8 and the README's "Other code remains held" paragraph describe the OLTP r2 review only through the hidden-function objection, which the review itself rates LOW 3. Its two medium findings appear nowhere in the trail or README:
   - MEDIUM 1: scrollable or non-immutable cursors now keep every row they fetch (`cursor.rs:644,682`).
   - MEDIUM 2: no gates have run on the integrated tree.

   Nothing is falsely accepted, since OLTP is HELD and the full result is saved. A one-clause mention in a later trail row would make the summary accurate.
2. **"Focused integrated regression/golden/parity checks" is not defined** in `acceptance-scope.json` under `combined_pg_engine.required`. It doesn't say whether the full regression, isolation and dump parity runs must be repeated on the final combined source. The baselines 51148/51150, 121/122 and 14817/0 were measured only on `3997cf00`. Naming the required runs now would keep a later gate from passing on focused checks alone.
3. **The storage audit can't be replayed once its wip branch is deleted.** `audit-storage-final.py` hard-codes the repository path and asserts that the remote ref `wip/resume-20261104-a169-raw-seal2` still exists. The push-continuously policy deletes `wip/` branches after landing, and after that the script fails before writing anything. It also writes into its own directory. Rerun in place, it either rewrites identical bytes or fails that assertion, so it can't silently change sealed evidence. The README's scratch-only rerun note doesn't cover this script yet.

The rerun hardened guard still rejects any log containing `panicked` or an `error:` line, even when the run passed. That can only cause a false failure, never a false pass. None of the five service crates has `should_panic` or `catch_unwind`, so it shouldn't happen.

I didn't verify what owners reported beyond the snapshot, I didn't review engine source, and I didn't poll any jobs. No services, builds or tests ran, nothing in the checkpoint changed, and I made no git writes.
