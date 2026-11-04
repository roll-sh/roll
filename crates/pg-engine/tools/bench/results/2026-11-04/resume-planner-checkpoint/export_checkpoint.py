#!/usr/bin/env python3
"""Export completed recovery evidence using a temporary Git index; leave pinned jobs untouched."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

W = '/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4'
D = Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
PREFIX = 'crates/pg-engine/tools/bench/results/2026-11-04/resume-planner-checkpoint/'
REF = 'refs/heads/wip/resume-20261104-planner-evidence'

def git(*args, data=None, env=None):
    return subprocess.check_output(['git', '-C', W, *args], input=data, env=env).decode().strip()

readme = '''# Recovery and OLTP165 review checkpoint (2026-11-04)

Evidence only. No source candidate is accepted and no performance advantage is claimed.
Only completed command outputs are exported. Ongoing diagnostic probe output is excluded.

Current owner sourcec8618834 follows completedab3 verification with a7line pgstat slot
ownership guard and2backend-lifecycle regressions. Engine::connect callers can overlap
the samePID: oldsession cleanup removed the newslot, so its idle count stayed0after10.5s,
expected1; endingnewsession flushed1. Actualred101 retained. NormalwirePIDreuse reachability
is not claimed. Arc::ptr_eq now removes only the registeredslot owned by the ending session.
Focused corrected lifecycle2/prepared4/idle-statistics1/end-monitoring1 allpassed.
Frozen fullv3 expected455/0/18/61 reconciles ab3's453/18/60 plus ONLY2tests/newbinary.
Fullv3 DEBUG andRELEASE eachcompleted455/0/18/61,command0;every frozen perbinary summary matched.
Release all-targetClippy0,same14message/pathdiagnosticmultiset ascompletedab3,no correctedpathdiagnostic.
Bookkeeping sequencecomparison initiallyFAILED on reversedbench/difftest diagnosticorder;
firstexport stoppedbeforeGit writes on missingauditfile. Bothfailures retained; no source,
frozengate orthreshold changed,no gate rerun. Diagnosticmultiplicities match,orderdoesnot.
Distpassed0,binarySHA25663e672d757b7ac4da113a78307ee6908fcdbb275deb4d78ce114a3afd90c2d9c.
pnpm test:pg --unit EXECUTED PASS0,24tests/5summaries;notpgengine/serviceacceptance.
All9commands0,ownedwrapper/heavy/runnergone,cleanpinnedc861/tree/frozenhashesverified by
separateterminalauditor. Fullv3terminalrawexported;parentcombined/resource/service/source
acceptance remainseparateandheld. INDEPENDENT165-HANDOFF.md records exactsource/limits.
Incremental staticR2 c861 vsab3 found noHIGH/MEDIUM; didnotrun gates. Its not-runstage
statement is historical: focused2+4+1+1,fullDEBUG/RELEASE455maps,Clippy/dist/unit nowpassed.
ResidualLOW: new samePIDregistration can displace STILL-LIVE oldslot, delaying its idle
counts untilflush/end; no wirePIDoverlap claim. Fast-own checks assertblocking/ungranted
waiter/release but onlyPRINT locktype/mode,do notassertfastpath parity. OtherreviewLOWs
are retained;no newsource/probes/rounds/services/benchmarks whilev3pinned.
Afterab3 terminal/rawexport, futurelaunch assessment identified heavy.sh has noaffinity.
v3 usesexisting taskset8-13,18-19, startupchecks nice19/jobs2/testthreads<=2 andaffinity.
Observed4focuseddescendantprocesses/12threads allwithin thataffinity; existingglobal4slots/
2jobs andexistinga165target unchanged. Oldfullv2 affinity0-35 limitation remains recorded.

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
The351 fulljob was SUPERSEDED after R3 found selectiveNOscroll retention and scalar
FROM defaultscroll regressions. Debug incomplete, no fullpass/fail assigned;laterstages
NOTRUN. Only verified owned root2142561/cargo2142672/golden2359500 were stopped;
wrapper2142509/heavy2142517 gone, exact cleanupreceipt preserved. Initial full-runner
failed beforecargo because *test matched fakeDocker selftest;failedrunner retained.

R3 runtime red165r3 confirms scalarPGgolden101(all3modes) and selectiveNOscroll memory101:
100000scanned/1000matched but100000stored/22,054,343bytes. Case matrix was written before
source correction, separating backward permission, observable qualrecheck and replay.
Correction7ffaee17 passes resolvedscroll to cursor setup;non-scroll does not perform
backward-only splitting. Supported Inner/Left+One+FromCall permit scalar FunctionScan.
PL/pgSQL factories defer cursor setup until OPEN resolves scrollability.
Green165r3 passed5goldenfiles×3modes,4locktests,4memorytests. SelectiveNOscroll now
stored0/10807bytes vs100000scanned/1000matched. Defaultscroll still100000stored/
22,053,528bytes:O(scanned),notO(result). CLOSE releases the spool. This resource widening
for newly rechecked observable quals remains an explicit unaccepted limitation;
no broadspill redesign or global Expr change undertaken. Ownwrite state test proves
rows discarded beforeUPDATE/DELETE and firstbackward;keyedHOLD claim remains limited.
Parent chose option(a), deferring cursor production delta.7ffa fulljob explicitly
SUPERSEDED with exact owned root3228093/cargo3231665/golden3546415 stopped andwrapper/
heavygone;debug incomplete,laterstagesNOTRUN. 14cursorfiles×3modecompatibility passed.
R4 read-only review confirms R3bugs fixed/no newHIGHorMEDIUMcorrectnessbug;O(scanned)
tradeoff remainsMEDIUM/unaccepted. Future qual-free-input restart option and LOWfrom_call/
PLrefcursor/FDW error-order hypotheses preserved,not implemented in independentbatch.

Independent candidateab3f64b8 from exactsharedfe7b0023 onownerbranch
work-a165-independent-20261104, remote wip/a165-independent-resume-20261104 independently
confirmed. Originalwork-a1657ffa unchanged.22paths:20fullproductiondeltas+onePLsystem_query
SearchPath typeconversion+preparedbehavior tests. All cursor/utility/PLcursorOPEN/factory
changes,memorycursoradditions,165cursorgoldens excluded. Three-wayclean;each retained
added/removedline exactlymatches documentedsource delta (169zero-contexthunks). Newshared
COPY/scan_need changes preserved;cursor/utility/memory bytesmatchshared. Scope/audit/patch
included. Focused lock/compact/kept/searchpath/privileges passed. Wire failed101:
66passed/1failed; big_bytea existing20s gate took28.455601813s after output checks passed.
Raw failure remains declared. Bounded candidate/sharedfe7/candidate isolated target all
passed (wire sourcehash equal);compile-inclusive times explicitly separate from harness
test times. These isolated passes do not erase fullwire failure/prove sourceeffect.
Transient control checkout restored cleanab3 before new gates;RESTORED receipt retained.

Prepared449count runner NEVER executed:parent caught wrong old-base library count before
launch. Shared tested451baseline perbinary map plus ONLY two newpreparedtest names =>
frozen453/0/18/60. Successorcheck165ind-v2 uses exact perbinary map/auditor, selfchecked
against oldcaab448summary, frozenrunnerhashes preserved. Per-candidate stack check scoped
pnpm test:pg --unit; stamp skip explicitlySKIP/noexecution; no service-stack orpgengine
acceptance. One final forced service gate held. Exactab3 fullgate terminal. DEBUG and RELEASE passed453/
0/18/60;every perbinary summary matches frozen map. Wire67/67 passed in this fullrun;
earlier fullwire101 remains preserved, no causal/performance inference. Release all-target Clippy
completed0:14diagnostics,message/path sequence equalhistoricalcaab,no candidate-changedpath.
No freshbaselineClippycomparison claimed. Distbuild passed0,binarySHA256
d4f2d31068d282cc28f98bfd77bcbb4ba85cd5d6492940ad13a7f279e5168b32.
pnpm test:pg --unit executed(notstampSKIP) andpassed0; its scopedbuild/Clippy/tests do not
execute pg-engine or establish service-stack acceptance. Separateterminalauditor rechecks
all5commandexits,completefrozenmaps,cleansource/tree,runnerhashes,binaryhash,unitclassification
andabsence ofownedwrapper/heavy/runner. Allpassed;sourceacceptance remainsheld.
Observed debugcargo593561/parallel_scan1489244 effectiveaffinity0-35,overlapping149reserved
4-7,22-25;nice19/jobs2/threads2 enforced, NO CPUisolation claim. No affinity/runnerchange;
Release cargo1812146/rustc1812721,1812723 also observed nice19 and effectiveaffinity0-35;
single read-only observation preserved, no mid-run controls changed.
Release parallel_scan2401643 and all11 existingthreads also observed effectiveaffinity0-35.
149timing deferred while overlap persists. Future launch-control assessment held until
terminalrawexport. Broader combined/runtime/resource/service gates pending. Independent exactab3 static review found noHIGH/MEDIUM,
22paths/cursor exclusionverified;rawresult andcount/reviewer-scopeerratum retained.

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
'''
files = {'README.md': readme.encode(), 'export_checkpoint.py': Path(__file__).read_bytes()}
sources = {
    'check165fixed/release-test.command': str(D / 'check165fixed/release-test.command'),
    'check165fixed/release-test.log': str(D / 'check165fixed/release-test.log'),
    'check165fixed/release-test-summary.json': str(D / 'check165fixed/release-test-summary.json'),
    'check165fixed/clippy.command': str(D / 'check165fixed/clippy.command'),
    'check165fixed/clippy.log': str(D / 'check165fixed/clippy.log'),
    'check165fixed/SUPERSEDED.json': str(D / 'check165fixed/SUPERSEDED.json'),
    'check165fixed/dist-superseded.log': str(D / 'check165fixed/build.log'),
    'runners/stop165-followon.py': str(D / 'stop165-followon.py'),
    'runners/stop165-followon-failed.log': str(D / 'stop165-followon.log'),
    'runners/embedded165.sh': str(D / 'embedded165.sh'),
    'runners/embedded165v2.sh': str(D / 'embedded165v2.sh'),
    'embedded165/shape.log': str(D / 'embedded165/shape.log'),
    'embedded165/shape.command': str(D / 'embedded165/shape.command'),
    'embedded165/cursor_replay_probe.rs': str(D / 'embedded165/cursor_replay_probe.rs'),
    'embedded165/reference.txt': str(D / 'embedded165/reference.txt'),
    'embedded165/exits.tsv': str(D / 'embedded165/exits.tsv'),
    'lock-fix/r2-tests.patch': str(D / 'lock-fix/r2-tests.patch'),
    'lock-fix/r2-NOTES.md': str(D / 'lock-fix/r2-NOTES.md'),
    'check165fixed/source.txt': str(D / 'check165fixed/source.txt'),
    'check165fixed/tree.txt': str(D / 'check165fixed/tree.txt'),
    'check165fixed/debug-test.command': str(D / 'check165fixed/debug-test.command'),
    'check165fixed/debug-test.log': str(D / 'check165fixed/debug-test.log'),
    'check165fixed/debug-test-summary.json': str(D / 'check165fixed/debug-test-summary.json'),
    'runners/cursor-embedded-functions-probe.sql': str(D / 'cursor-embedded-functions-probe.sql'),
    'runners/wrapper-selftest.py': str(D / 'wrapper-selftest.py'),
    'runners/prof.sh': '/home/meszmate/.cache/a165/scripts/prof.sh',
    'runners/prof-original-false-success.sh': str(D / 'prof-original-false-success.sh'),
    'runners/matrix-before-resource-repair.sh': str(D / 'matrix-before-resource-repair.sh'),
    'runners/conn1000-before-resource-repair.sh': str(D / 'conn1000-before-resource-repair.sh'),
    'runners/check165fixed.sh': str(D / 'check165fixed.sh'),
    'green165v2/head.txt': str(D / 'green165v2/head.txt'),
    'green165v2/source.patch': str(D / 'green165v2/source.patch'),
    'green165v2/exits.tsv': str(D / 'green165v2/exits.tsv'),
    'green165v2/lock.command': str(D / 'green165v2/lock.command'),
    'green165v2/lock.log': str(D / 'green165v2/lock.log'),
    'green165v2/cursor.command': str(D / 'green165v2/cursor.command'),
    'green165v2/cursor.log': str(D / 'green165v2/cursor.log'),
    'green165v2/memory.command': str(D / 'green165v2/memory.command'),
    'green165v2/memory.log': str(D / 'green165v2/memory.log'),
    'green165v2/goldens.sha256': str(D / 'green165v2/goldens.sha256'),
    'runners/review165.sh': str(D / 'review165.sh'),
    'runners/probe165.sh': str(D / 'probe165.sh'),
    'runners/green165v2.sh': str(D / 'green165v2.sh'),
    'oltp165/SUPERSEDED.json': str(D / 'check165/SUPERSEDED.json'),
    'oltp165/dist-superseded.log': str(D / 'check165/build.log'),
    'review165/exits.tsv': str(D / 'review165/exits.tsv'),
    'review165/head.txt': str(D / 'review165/head.txt'),
    'review165/red-source.patch': str(D / 'review165/red-source.patch'),
    'review165/red-lock.log': str(D / 'review165/red-lock.log'),
    'review165/red-cursor.log': str(D / 'review165/red-cursor.log'),
    'review165/red-memory.log': str(D / 'review165/red-memory.log'),
    'review165/red-replay.actual': str(D / 'review165/red-replay.actual'),
    'review165/green-lock.log': str(D / 'review165/green-lock.log'),
    'review165/green-cursor.log': str(D / 'review165/green-cursor.log'),
    'review165/reference-replay-invalid-timestamp.out': str(D / 'review165/reference-replay.out'),
    'review165/stream-before.sha256': str(D / 'review165/stream-before.sha256'),
    'probe165/head.txt': str(D / 'probe165/head.txt'),
    'probe165/source.patch': str(D / 'probe165/source.patch'),
    'probe165/exits.tsv': str(D / 'probe165/exits.tsv'),
    'probe165/record.log': str(D / 'probe165/record.log'),
    'probe165/record.command': str(D / 'probe165/record.command'),
    'probe165/cursor.log': str(D / 'probe165/cursor.log'),
    'probe165/corrected-reference-replay.sql': str(D / 'probe165/corrected-reference-replay.sql'),
    'probe165/corrected-reference-replay.out': str(D / 'probe165/corrected-reference-replay.out'),
    'runners/audit-footprint.py': str(D / 'audit-footprint.py'),
    'oltp165/release-complete.log': str(D / 'check165/release-test.log'),
    'oltp165/release.command': str(D / 'check165/release-test.command'),
    'oltp165/release-summary.json': str(D / 'check165/release-summary.json'),
    'oltp165/clippy-complete.log': str(D / 'check165/clippy.log'),
    'oltp165/clippy.command': str(D / 'check165/clippy.command'),
    'runners/check165.sh': str(D / 'check165.sh'),
    'runners/check171.sh': str(D / 'check171.sh'),
    'runners/bench165.sh': str(D / 'bench165.sh'),
    'runners/BENCH-PLAN.md': str(D / 'BENCH-PLAN.md'),
    'runners/matrix.sh': '/home/meszmate/.cache/a165/scripts/matrix.sh',
    'runners/conn1000.sh': '/home/meszmate/.cache/a165/scripts/conn1000.sh',
    'runners/footprint.sh': '/home/meszmate/.cache/a165/scripts/footprint.sh',
    'runners/jobA.sh': '/home/meszmate/.cache/a165/scripts/jobA.sh',
    'runners/jobB.sh': '/home/meszmate/.cache/a165/scripts/jobB.sh',
    'runners/jobB-original-false-success.sh': str(D / 'jobB-original-false-success.sh'),
    'oltp165/debug-complete.log': str(D / 'check165/debug-test.log'),
    'oltp165/debug.command': str(D / 'check165/debug-test.command'),
    'oltp165/debug-summary.json': str(D / 'check165/debug-summary.json'),
    'oltp165/debug-tested-tree.txt': str(D / 'check165/debug-tested-tree.txt'),
    'oltp165/reference.txt': str(D / 'check165/reference.txt'),
    'oltp165/jobA.log': '/home/meszmate/.cache/a165/logs/jobA.log',
    'oltp165/jobB.log': '/home/meszmate/.cache/a165/logs/jobB.log',
    'oltp165/test-debug-failed.log': '/home/meszmate/.cache/a165/logs/test-debug.out',
    'oltp165/test-release-failed.log': '/home/meszmate/.cache/a165/logs/test-release.out',
    'oltp165/m0.tsv': '/home/meszmate/.cache/a165/logs/m0.tsv',
    'oltp165/m1.tsv': '/home/meszmate/.cache/a165/logs/m1.tsv',
    'oltp165/fp-base.log': '/home/meszmate/.cache/a165/logs/fp-base.log',
    'oltp165/fp-prebase.log': '/home/meszmate/.cache/a165/logs/fp-prebase.log',
    'oltp165/record.log': str(D / 'check165/record.log'),
    'oltp165/record.command': str(D / 'check165/record.command'),
    'oltp165/record.sha256': str(D / 'check165/record.sha256'),
    'oltp165/record.patch': str(D / 'check165/record.patch'),
    'oltp165/record-source.txt': str(D / 'check165/source.txt'),
    'planner171/check.log': '/home/meszmate/.cache/a171/logs/check.log',
    'planner171/base-dist.log': '/home/meszmate/.cache/a171/logs/base-dist.log',
    'fdw172/base-engine.rev': '/home/meszmate/.cache/a172/base-engine.rev',
    'fdw172/base-dist.log': '/home/meszmate/.cache/a172/base-dist.log',
    'fdw172/contrib-base.log': '/home/meszmate/.cache/a172/contrib-base.log',
    'fdw172/REPORT.md': '/home/meszmate/.cache/a172/contrib/run-base/report/REPORT.md',
}
for campaign in ('embedded165v2', 'embedded165v3', 'green165r2', 'green165r2v2', 'check165r2-runner-error', 'check165r2', 'red165r3', 'green165r3', 'check165r3', 'focus165ind', 'bytea165compare', 'probe165-low'):
    for artifact in sorted((D / campaign).iterdir()):
        if artifact.is_file():
            sources[campaign + '/' + artifact.name] = str(artifact)
for runner in ('embedded165v3.sh', 'green165r2.sh', 'green165r2v2.sh', 'check165r2.sh', 'check165r2-runner-error.sh', 'wrapper-selftest-results.json', 'red165r3.sh', 'green165r3.sh', 'R3-CASE-MATRIX.md', 'check165r3.sh', 'independent165.patch', 'independent165-scope.json', 'independent165-merge-audit.json', 'independent165-candidate.patch', 'DEFERRED-CURSOR-R4.md', 'focus165ind.sh', 'bytea165compare.py', 'check165ind.sh', 'check165ind-v2.sh', 'audit165-summary.py', 'ind165-expected-per-binary.json', 'ind165-test-count-reconciliation.json', 'ind165-frozen-runner.sha256', 'count-map-verification.json', 'count-map-check165fixed.json', 'caab-auditor-selfcheck.json', 'oltp-independent-source-review-r1-result.json', 'independent-review-erratum.md'):
    sources['runners/' + runner] = str(D / runner)
for name in ('source.txt', 'tree.txt', 'initial.status', 'debug-test.command', 'debug-test.log', 'debug-test-summary.json', 'debug-completed-exits.tsv', 'release-test.command', 'release-test.log', 'release-test-summary.json', 'release-completed-exits.tsv', 'affinity-observation-debug.json', 'affinity-observation-release.json', 'affinity-observation-release-tests.json'):
    sources['check165ind-v2/' + name] = str(D / 'check165ind-v2' / name)
for name in ('clippy.command', 'clippy.log', 'clippy-completed-exits.tsv', 'clippy-warning-audit.json'):
    sources['check165ind-v2/' + name] = str(D / 'check165ind-v2' / name)
for name in ('build.command', 'build.log', 'engine.sha256', 'postgres-unit.command', 'postgres-unit.log', 'postgres-unit-status.json', 'postgres-unit-summary.json', 'exits.tsv', 'current.status', 'TERMINAL.json', 'debug-test-terminal-audit.json', 'release-test-terminal-audit.json'):
    sources['check165ind-v2/' + name] = str(D / 'check165ind-v2' / name)
sources['check165ind-v2/wrapper.log'] = str(D / 'check165ind-v2-wrapper.log')
sources['runners/audit165-terminal.py'] = str(D / 'audit165-terminal.py')
for name in ('a165_independent_low.rs', 'probe165-low.sh', 'ind165-future-launch-controls.json', 'c8618834-slot-ownership.patch', 'check165ind-v3.sh', 'ind165-v3-expected-per-binary.json', 'ind165-v3-count-reconciliation.json', 'ind165-v3-frozen-runner.sha256'):
    sources['runners/' + name] = str(D / name)
sources['probe165-low/wrapper.log'] = str(D / 'probe165-low-wrapper.log')
for name in ('oltp-independent-source-review-r2-result.json', 'independent-review-r2-erratum.md'):
    sources['runners/' + name] = str(D / name)
for name in ('source.txt', 'tree.txt', 'initial.status', 'startup-controls.json', 'affinity-observation-focused.json', 'lifecycle.command', 'lifecycle.log', 'prepared.command', 'prepared.log', 'statistics.command', 'statistics.log', 'end-monitoring.command', 'end-monitoring.log', 'focused-completed-exits.tsv'):
    sources['check165ind-v3/' + name] = str(D / 'check165ind-v3' / name)
for name in ('debug-test.command', 'debug-test.log', 'debug-test-summary.json', 'debug-completed-exits.tsv', 'affinity-observation-debug.json'):
    sources['check165ind-v3/' + name] = str(D / 'check165ind-v3' / name)
sources['runners/audit165-v3-terminal.py'] = str(D / 'audit165-v3-terminal.py')
sources['runners/ab3-dist-artifact-preserved.json'] = str(D / 'ab3-dist-artifact-preserved.json')
sources['runners/clippy-v3-bookkeeping-failure.json'] = str(D / 'clippy-v3-bookkeeping-failure.json')
sources['runners/INDEPENDENT165-HANDOFF.md'] = str(D / 'INDEPENDENT165-HANDOFF.md')
for name in ('release-test.command', 'release-test.log', 'release-test-summary.json', 'clippy.command', 'clippy.log', 'clippy-completed-exits.tsv', 'clippy-warning-audit.json'):
    sources['check165ind-v3/' + name] = str(D / 'check165ind-v3' / name)
for name in ('build.command', 'build.log', 'engine.sha256', 'postgres-unit.command', 'postgres-unit.log', 'postgres-unit-status.json', 'postgres-unit-summary.json', 'exits.tsv', 'current.status', 'TERMINAL.json', 'debug-test-terminal-audit.json', 'release-test-terminal-audit.json'):
    sources['check165ind-v3/' + name] = str(D / 'check165ind-v3' / name)
sources['check165ind-v3/wrapper.log'] = str(D / 'check165ind-v3-wrapper.log')
for name, source in sources.items():
    files[name] = Path(source).read_bytes()
files['manifest.json'] = (json.dumps({name: {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}
    for name, data in sorted(files.items())}, indent=2) + '\n').encode()
with tempfile.TemporaryDirectory(prefix='a165-evidence-') as temp:
    env = dict(os.environ, GIT_INDEX_FILE=temp + '/index')
    head = git('rev-parse', 'HEAD')
    previous = git('ls-remote', 'origin', REF).split()
    # Retain the previous evidence tree: advancing the source HEAD must not put
    # production changes in this evidence-only checkpoint's first-parent diff.
    git('read-tree', previous[0] if previous else head, env=env)
    for name, data in files.items():
        blob = git('hash-object', '-w', '--stdin', data=data)
        git('update-index', '--add', '--cacheinfo', '100644', blob, PREFIX + name, env=env)
    tree = git('write-tree', env=env)
    parents = ['-p', previous[0], '-p', head] if previous else ['-p', head]
    commit = git('commit-tree', tree, *parents, data=b'pg-engine: preserve completed OLTP, planner, FDW recovery evidence\n')
    git('push', 'origin', commit + ':' + REF)
    observed = git('ls-remote', 'origin', REF).split()[0]
    assert observed == commit, (commit, observed)
    (D / 'checkpoint-commit.txt').write_text(commit + ' ' + REF + '\n')
    changed = git('diff', '--name-only', commit + '^1', commit).splitlines()
    assert all(path.startswith(PREFIX) for path in changed), changed
    manifest = json.loads(files['manifest.json'])
    for name, expected in manifest.items():
        raw = subprocess.check_output(['git', '-C', W, 'show', commit + ':' + PREFIX + name])
        assert len(raw) == expected['bytes'] and hashlib.sha256(raw).hexdigest() == expected['sha256'], name
    receipt = dict(commit=commit, ref=REF, remote=observed, files=len(files),
                   manifest_entries=len(manifest), changed_paths=len(changed),
                   evidence_only_first_parent=True, source_head=head)
    (D / 'checkpoint-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt))
