# Recovery and OLTP165 review checkpoint (2026-11-04)

Evidence only. No source candidate is accepted and no performance advantage is claimed.
Only completed command outputs are exported. Ongoing diagnostic probe output is excluded.

OLTP165 recovered at 6fde4096. jobA failed; jobB was FALSE SUCCESS: every cargo
step failed compilation on missing AtomicU64, despite final HEAVY-EXIT0. Original wrappers
and failed logs are retained. baeee87a fixes that import. Its full debug and release
suites passed446tests/0failed/18ignored,60summaries each; release all-target Clippy exited0
with warnings. Its dist build was cancelled as SUPERSEDED after independent source review.
No old-source benchmark was launched.

PostgreSQL18.6 recording of cursor_stream used GOLDEN_RECORD=127.0.0.1:53310 and PgClient
in golden.rs, reference container4c7dfb3163fe4fc80a56903c187c33bee8e67d48cee599baa4e3ec66bad53edd,
image77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873.
The397-line golden is backed by2ef850e8; a fresh reference recording reproduced it byte
for byte. Recording mode itself is not embedded/wire/shuffled parity evidence.

Review fixes are committed as caabeb196e268f5a0fb822e30565e633344058d1, tree
665cf537d56ffff6d7c110f70faab7e211fce256, remotely confirmed on
wip/a165-resume-20261104. Prepared lock moves are tracked per owner and detached prepared
transactions retain no backend slot. Cursor replay rejects function/CTE/sort nodes and
requires immutable plain scans, retaining rows after its first restart. Streaming keys
are pruned with rows. Litplan passes the retained SearchPath Arc.

Old-source red tests exited101 with real FAILED summaries: prepared finish left2holds,
cursor replay disagreed with PG18.6, streaming98kmore rows retained6,000,768keybytes.
The first corrected lock matrix passed, but cursor default-SRF scrollability failed.
A trace found Join+FromCall representing Function Scan; its scrollability was corrected
and trace removed. Focused green165v2 passed lock3/3, cursor3/3(twofiles in all3modes),
memory1/1:0additional retainedbytes and backward20krows below10seconds. The initial
timestamp fixture was invalid on BOTH engines and establishes no timestamp coverage.
Its corrected refcursor fixture was freshly recorded on PG18.6 without ERROR rows.

The exact clean caab source full DEBUG suite completed448passed/0failed/18ignored across
60summaries, command exit0. Release also completed448/0/18/60; release all-target Clippy exited0 withwarnings.
Obsolete dist was cancelled as SUPERSEDED after R2. pnpm was NOT RUN. All resource/performance gates remain pending.

Source acceptance remains HELD pending review and full gates for corrected351af2ac.
R2's runtime predicate defect is PROVEN by embedded165v3 against actual PG18.6.
Domain CHECK in a scan qualifier emitted check1,2,3 on caab's first BACKWARD1,
where PG emitted only check3; the following BACKWARD ALL emitted none vs PG2,1.
Shape/record exited0, comparison101 in all3modes. Diagnostic patches/output are preserved.
Unrelated custom ANY EXPLAIN/public.= IN lookup gaps are preserved separately in v2.

351af2ac adds cursor-local embedded operator/domain safety and rechecks observable quals,
blocks rerun for Lazy.coerce, restricts transparent Join kinds to Inner, and returns an
internal error for a missing streamed current key. No global Expr walker changes.
Focused green165r2v2 passed PGrecord,4goldenfiles in3modes,4locktests and2memorytests.
Goldens cover domain, domainIN/LIMIT, runtime STABLE customANY, builtinIN, ownUPDATE/
DELETE before firstbackward, NOscrollmultiple-reference24000 afterdiscard, keyedWITHHOLD.
Default STABLEqual materialization remains O(n):20krows4,683,971bytes;100k22,055,318bytes;
CLOSE releases them (-1577bytes vsopenedbaseline). NOscroll98kmore rows retained0bytes;
backward20k143ms is a correctness bound, not an accepted performance comparison.
The new full gate job is active and its partial outputs are excluded.
Initial full-runner failed because *test also matched the fakeDocker selftest, applying
an inappropriate450test-summary assertion. No cargo step ran in that attempt. Exact-name
summary gating repaired; failed runner/log retained, no false pass claimed.

Owned cache runners now fail explicitly on logged failures (Bash ! guards do not imply
errexit). Profiler wrapper records perf/pgbench exits separately. Matrix/scaled scripts
refuse existing resources, clean only recorded created IDs, and preserve prior data.
Three fakeDocker propagation/ownership selftests now pass; results and exact runner are preserved. No frozen measurement thresholds were changed.
The planned matrix remains2interleaved rounds, exploratory only; no winner or causal
speedup can be claimed without the required repetitions, work checks and limiter evidence.
Historical m0/m1 are partial campaigns without complete raw pgbench/source gates and
cannot establish a performance regression or gain. Old footprints do not verify caab.

Planner171's six dirty SQL files are preserved in ec5b9f39, remote
wip/a171-resume-20261104. Its earlier cargo check lacks adequate source identity;
chosen-cost validation has NOT started. This scope remains exclusively171.

FDW172 recovered clean at63f7c0b4, remote wip/a172-resume-20261104, with no interrupted
round8 implementation. Completed baseline dist/contrib evidence reports
postgres_fdw1945/2053, dblink218/218, file_fdw187/187, aggregate2350/2458.
This is baseline evidence only; direct-DML/join-order work has not been implemented.

The manifest hashes every artifact except itself. Evidence checkpoints retain their
previous evidence tree so advancing source HEAD cannot add production changes to their
first-parent diff. Parent alone integrates the shared branch; parent/main worktrees
remain untouched. Current source acceptance, broader regression/client/difftest/TPC-H
checks and final combined verification remain pending with parent coordination.
