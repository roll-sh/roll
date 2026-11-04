Bounded owner handoff, 2026-11-04

All four original owner worktrees are preserved. Parent and main checkouts were untouched. No new optimization round, timing, service suite, crash rerun, or SSI/storage probe ran. All owned heavy work used existing targets/shared slots, nice 19, jobs 2, and effective CPUs 4-7,22-25. No extra target directory was created.

Owner 148: work-a148 at 2bc84a5a. INCONCLUSIVE; disk FALSE ZERO is invalid. Source/evidence backed at 65b84c79. The original sealed 95-file bundle remains unchanged. No further bulk timing.

Owner 149: work-a149 at 26f4bfac. The prefix_end endpoint correction remains. Original frozen ddl_scaling failed 101; historical 6003 attribution remains unproven. The single isolation preflight was DEFERRED and its target NOT RUN due to overlap. No repeated timing. Evidence backed at 0e050abf. Existing target is 66 GB; no cleanup performed.

Owner 167: work-a167 at d9e2279c9d5b55a21add46fe4632aa73e3f3c849.
- Actual PG18.6/engine red: production 278 reported InitPlan loops 6 versus 1 and external CTE producer rows 54/loop 1 versus 9/1. The correlated control correctly reported 54 loops.
- Test-only commits: 641c593c + 1137c618. Repair 1f17968b snapshots only owned inner counters and rescanned correlated plans, including local WITH producers and excluding external CTE producers. Scan/index/bitmap expressions are covered. The stray unasserted test pass at parallel_scan.rs:955 was removed.
- Initial repair compile failure E0606 is preserved remotely; one-line boxed ScanPlan pointer correction is d9e2279c.
- Exact d9e focused PASS: three a167 golden modes, one reordered heap path test, library Clippy exit 0 with no new warnings relative to the ten baseline library warnings. Raw: a167-owned-focused-r2/. Remote: 7eff5c45, resume-executor167-r3-focused-green.
- L1 remains plausible: method=None can survive planner fallback for more than eight items, but the bounded nine-item PG plan showed no Materialize. No runtime closure claimed. Pre-existing Gather/barrier/partial-consumption/pgstat limits were not widened into this fix.

Owner 167 crash gate remains independently FAILED:
- Exact 278: build/debug 447/release 447 (18 ignored in each test gate)/Clippy passed; known new stray warning was retained. Crash30 exited 101, actual summary 1 passed/1 failed, seed 14 round 2. There were 691 journal entries; the mismatch was first observed after open + settle, with 2,833 rows versus 2,832 and value mismatches. Postgres unit gate was NOT RUN due to fail-fast.
- Sealed terminal: aa48d3c9, resume-executor167-weighted-terminal. No causal attribution or rerun. Storage group owns read-only recovery triage.
- Failed seed14 data/oracle was not identified; the harness unwinds TempDir on panic. Two older unattributed temporary directories remain untouched. Their inventory and the raw 409-byte MANIFEST were backed at acff1024.

Owner 170: work-a170 at 1b6a80b9918db481d27527511d70ae716b1bdecf.
- PG18.6 retained 667 main-heap pages after DROP COLUMN, with row/value checks intact. COPY of five 2,000-byte values and a MAIN 8,150-byte value each succeeded with one main page and the expected MD5.
- Production 047 + test-only 4b7db3a9 failed 101: two passed/three failed. Dropped generic row measurement was 33 pages versus 667; both COPY shapes overflowed HeapPacker. Red backed at 3f3a16b0.
- Separate fixes: 1884b7dd preserves raw small generic tuple bytes, nulling only virtual columns; 1b6a80b9 declines the optional cached Load measure for raw sizes requiring TOAST sizing through existing sized=false behavior. Row/xmin/may_toast bookkeeping and COPY/read_once calls remain. No packer clamp, index tuple/parbuild semantic change, or broad model redesign.
- Exact 1b6 focused PASS: five VACUUM/COPY cases, two kept-header tests, 25 related TOAST/model tests, library Clippy exit 0 with no new baseline warnings. Direct unchanged.valid(...).live assertions cover 33 and 667 pages. Raw: a170-r3-focused/. Remote: a313c23a, resume-models170-r3-focused-green.

Remaining gates and limits:
- Full build/debug/release/all-target Clippy/crash/unit gates on new d9e and 1b6 sources are UNRUN. The independent 278 crash hold survives the instrumentation repair's focused passes.
- Parent owns combined 163/170 modeled parallel oversize/INCLUDE validation and actual worker proof. Verification group owns the one final combined service gate.
- Physical recomputation of already-toasted pre-DROP tuples under current STORAGE/COMPRESSION/toasttarget/pinned external state remains a held limitation. Pre-existing PLAIN oversized INSERT acceptance is outside this patch. No performance, footprint, or global PostgreSQL superiority claim.
- No broader full successor is queued. At 19:05:12 UTC there were no owned active build/test/reference processes or reference containers; a167 and a170 targets were released for the parent's first combined compile/library proof. Both targets are 40 GB.

Every completed raw output, command/actual exit, source/binary identity, and limitation is exported on owner WIP branches with closed-set SHA256SUMS and verified ls-remote. The pre-handoff remote audit covers 62 bundles, 770 files, 708 hashes, with no missing/extra/hash mismatch. See remote-evidence-audit.json, r3-slot-target-release.json, and r3-final-owner-state.json.
