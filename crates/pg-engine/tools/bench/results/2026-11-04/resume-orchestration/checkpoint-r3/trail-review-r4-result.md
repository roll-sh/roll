# Review of checkpoint r3 and trail rows 192–201

Nothing I found is high severity. The source identity and the guard repair hold up. Two medium findings: the checkpoint cites review conclusions that have no saved result behind them, and the full-service gate doesn't actually test pg-engine. I modified nothing. I reran prove.py once in a scratch copy under /tmp, then deleted it; `git status` still shows only the untracked `checkpoint-r3/`.

## Verified directly
- **Shared source matches tested source.** Between `3997cf00` and `eb450f4e`, the only file that differs inside the stated scope (`pg-engine/src`, `tests`, both `Cargo.toml`, `Cargo.lock`) is `tests/difftest/STATUS.md`. Outside that scope, only `HANDOFF.md`, `tools/audit-tap.py` and files under `tools/bench/results/` differ. `scripts/` and `package.json` are identical.
- **eb450f4e is the five-file candidate.** `git patch-id --stable` gives the same id for `574e5c97..eb450f4e` and `63f7c0b4..6493cf49`, and the diff is exactly 5 files. `6493cf49..3997cf00` touches only `HANDOFF.md` and `STATUS.md`.
- **Remote refs** (fresh `ls-remote`): `docs/postgres-architecture-strategy` is `eb450f4e`, `a162-dump5` is `4b99a106`, and `a162-evidence` is `d24435ba`. The four raw-seal refs match `raw-log-parent-object-audit.json`.
- **Runner identity.**
  - `original.sh` is byte-identical to the sealed `d24435ba:…/resume-verification162/check-full-service.sh` (sha `6f13cd…`).
  - The live `verification/check-full-service.sh` equals `repaired.sh` (sha `ea0fb0…`).
- **Guard repair works.** The old `! rg` line under errexit does write COMPLETE on failed-summary, error-line and stamp-skip logs; the repaired version exits 1 with no marker. A network rc of 23 now exits 23 before the service would start. `bash -n` passes.
- **prove.py rerun (scratch copy):** `PASS 18 controls`. `results.json` came out byte-identical. The other artifacts differ only in `status.tsv` timestamps and the `V=` path in each `command.sh`.
- **Checkpoint integrity.**
  - `SHA256SUMS` verifies and covers every file.
  - These files are byte-identical to the parent-cache copies: the guard-proof directory, the 6 copied JSONs and `dump5-backup.json`.
  - `verification-readiness.json` is identical to the sealed `4b99a106` copy.
  - `decisions-excerpt.tsv` matches trail rows 192–201 exactly.
  - No tracked file is modified, so earlier checkpoints and the raw seals are untouched.
- **Dump validator.** Dump5 used `audit-tap.py` with sha `ec0ba048…`, which is the same blob as the shared `tools/audit-tap.py`. The sealed `final-audit.json` says 14817/0 PASS with v3/v4 left unchanged. I read that sealed record but did not re-audit the TAP output.

## Findings

**MEDIUM 1: review conclusions rest on launch receipts, not results.** This affects rows 196 (catalog: "noHIGH/MEDIUM r2 review"), 197 (index: "Independent review found correctness divergence") and 198 (OLTP: hidden-function objection). It also affects the README's "Catalog … passed … static review".
- The cited `catalog-source-review-r2.json`, `index-source-review-r1.json` and `oltp-source-review-r2.json` all show `status: running` and `summary: null`.
- Every other `*-review-*.json` in the parent cache is the same kind of receipt.
- No saved review result exists in the checkpoint or the cache.

Fix: save each terminal review summary, or a hash of it, and cite that. Otherwise, label these conclusions as owner or parent reports. I did not call `task_status` to look for the results, because reading a finished result acknowledges its delivery to the parent.

**MEDIUM 2: the full-service gate does not exercise pg-engine.** `scripts/test-postgres.sh:10,29,42` builds and tests only pg-proxy, pg-storage, pg-control, rolld and roll-cli, and runs e2e from `-p pg-control`. None of those crates' manifests mention pg-engine, and `e2e.rs` never refers to it. pg-engine reaches the run only through the shared `Cargo.toml`/`Cargo.lock`.
- Nothing is overclaimed today because full service is HELD.
- But `verification-readiness.json` ("ONE final combined pnpm test:pg --force") and the README imply this run is what gates pg-engine acceptance.
- Fix: state that a green full-service run is not pg-engine evidence, or name the gate that is.

**LOW 3: trail lag again.** All 10 new rows carry the same timestamp, 13:52:53Z. Several events they record happened earlier:

| Event | Time (UTC) |
|---|---|
| `eb450f4e` committed | 13:26:51 |
| storage-performance launched | 13:36:20 |
| validator replay | 13:37:33 |
| runner edited | 13:48:48 |
| guard proof | 13:49:26 |

The "start" row therefore comes after work it describes as starting. The row-194 remote recheck is stamped before `integration-identity.json`'s `observed_at` (13:53:42Z). The runner edit had no decision row before it was made.

**LOW 4: "18 controls exercise the exact saved guard snippets" overstates.**
- For `original`, `prove.py:23` sets the network snippet to `''`. The two original network controls are therefore constructed, not run against `original.sh` text. Their conclusion is still true by reading the script.
- No control covers the `|| network_rc=$?` capture line (`repaired.sh:21`).
- The precise wording is 16 snippet controls plus 2 constructed baselines.

**LOW 5: the guard still accepts some rc=0 logs.** In scratch I ran extra controls against the repaired tail. Each of these reached COMPLETE:
- `running 0 tests … 0 passed; 40 ignored`
- a log with only a `panicked` line
- `error[E0308]:` (the `^error:` pattern doesn't match that form)

These only matter if pnpm exits 0, so they are defense-in-depth gaps. The zero-tests case is the realistic one; consider requiring `test result: ok. [1-9]`. A NUL byte, an indented `Running`, and CRLF line endings were all correctly rejected.

**LOW 6: prove.py can't be rerun in place.** It writes into its own directory (`prove.py:26-34,54`). Running it inside the committed checkpoint would rewrite `status.tsv` timestamps and break `SHA256SUMS`. The saved `command.sh` files hard-code `V=` to the parent cache, so running one directly would write there. The README should say to copy it to a scratch directory before rerunning.

**LOW 7: `validator-parent-replay.json` is an exit-code list only.** The checkpoint has no harness, inputs or hashes for it, so it can't be checked from the checkpoint alone.

**INFO:** "Remaining scoped difference is STATUS.md" is accurate. `HANDOFF.md` and `tools/audit-tap.py` also differ but sit outside the scope; `audit-tap.py` was added on the shared branch and is the exact blob dump5 used. You could mention this for completeness.

## Owner reports I did not verify
- storage: all 14 exact-source commands pass
- catalog: debug, release and Clippy pass
- OLTP: 448 passed / 18 ignored in debug; release active
- storage-performance builds active
- SSI: 6003 vs 6000
- executor/models status
- regression and isolation counts, plus the 451/18 gates (taken from sealed JSON, not rerun)

None of these are overclaimed in the checkpoint: each is worded as held, active or unresolved.
