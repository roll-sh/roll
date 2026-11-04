Independent165 bounded source package, 2026-11-04

Candidate commits, in order: ab3f64b8dd92203123f6777ae4a6028d3a3aae67 then c8618834caf45bc0a34cf9985c07f9bc13b498ef, based on shared fe7b0023. Both are remote-backed on wip/a165-independent-resume-20261104. Tested final tree df3cf582a5ff3b1554067af296e95d4d9f708df6; original work-a165 at 7ffaee17 remains intact and remote-backed. Parent alone integrates shared source/docs.

Retained scope: 23 paths. The original independent slice has 20 production paths, PL system_query's mechanical SearchPath conversion, and twophase tests. It retains prepared-lock owner tracking/detach, fast-own transaction locks, SearchPath Arc/memo propagation, per-backend temp/settings/permission/activity/pgstat lifecycle and shared-socket changes. c861 adds only the 7-line pgstat removal-ownership guard plus a new 2-test backend_lifecycle binary. Scope/three-way/169-hunk audit remains in independent165-merge-audit.json; c861's exact two-path patch is c8618834-slot-ownership.patch.

Excluded: every deferred cursor/utility/PL OPEN/factory production change, cursor memory-load additions and cursor goldens. No newer shared whole files were overwritten; shared COPY/scan_need hooks remain. R4's cursor O(scanned) tradeoff remains unaccepted; see DEFERRED-CURSOR-R4.md. No cursor performance claim.

Current exact-source checks, all terminal:

- Focused lifecycle 2, prepared 4, idle-statistics 1, end-monitoring 1: commands0, expected summaries.
- Full debug and release: EACH 455 passed, 0 failed, 18 ignored, 61 summaries; complete per-binary frozen-map matches, command0.
- Release all-target Clippy0: same 14 warning message/path multiset as completed ab3, no corrected-path warning. Initial diagnostic-order bookkeeping assertion/export failures are retained; no frozen gate/source/threshold changed or gate rerun.
- Dist build0; roll-engine SHA256 63e672d757b7ac4da113a78307ee6908fcdbb275deb4d78ce114a3afd90c2d9c at ~/.cache/a165/bin/independent/roll-engine. Prior ab3 binary preserved separately with its original d4f2d310... hash.
- pnpm test:pg --unit EXECUTED PASS0: selected-crate build, Clippy -D warnings, 24 tests/5 summaries. This does not execute pg-engine or establish service-stack acceptance.
- Separate terminal auditor rechecked all 9 command exits, both raw maps, clean source/tree, frozen hashes, binary hash/unit classification, and absence of owned wrapper/heavy/runner. See check165ind-v3/TERMINAL.json.

Review and demonstrated fixes:

- Independent ab3 R1 and c861 incremental R2 found no HIGH/MEDIUM. Static source count reconciliation is distinguished from actual completed gate evidence.
- Public embedded API red: overlapping caller-supplied samePID sessions; old session end unregisters the new idle slot, so count remains0 after10.5s, expected1; ending new flushes1. Exit101/FAILED preserved. Guarded removal now passes through pg_stat_user_tables, also asserting no double count.
- RR SELECT1/xid0 RowExclusive LOCK both block expression-CIC; tests assert xid0, blocking_pids, ungranted waiter, commit wakeup/cleanup. Raw lock type/mode are printed, not fully asserted.

Limits retained; no additional round/probe requested:

- SamePID registration can still displace a STILL-LIVE older slot, delaying its idle counts until flush/end. Normal wire live PIDs are unique; no wire overlap case claimed.
- pg_locks fastpath/type/mode assertion gap remains LOW. flush_idle contention/early-exit frequency is unmeasured; noted_due retry and two-Txns/backend hypotheses and SearchPath invalidation/temp/activity coverage limits remain disclosed.
- Earlier focused ab3 wire101 (66pass/1failure, unchanged20s bytea gate at28.4556s) is preserved. Isolated candidate/base/candidate controls passed, and subsequent complete wire suites passed, but no causal/flake/performance attribution is made.
- Historical v2 children actually used affinity0-35, overlapping149 reservation. Only nice/jobs/test-thread caps applied then. v3 startup guard and sampled focused/debug descendants matched8-13/18-19, nice19/jobs2/testthreads<=2; this is not an exhaustive lifetime audit. Global slots4/jobs2 unchanged; no new Cargo target, service or benchmark.
- No current-source performance, footprint/scaled-resource, broad PostgreSQL parity, combined-runtime or final forced service-stack acceptance. Parent coordinates those separate gates and source integration. No global faster-than-PostgreSQL claim.

171 remains clean/ec5b9f39 and queued for bounded chosen-cost handoff; customANY EXPLAIN/public custom '=' IN mismatches remain explicit before any planner/FDW timing claims. 172 remains clean/63f7c0b4 with baseline-only contrib2350/2458 evidence; no interrupted round8 implementation existed. No timing campaign has started.
