# Verification162 — current checkpoint

Worktree: `/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc`
Branch `work-a162`, clean pinned HEAD `3997cf00a2a301fed65f00469625ff14f223f227`. Do not move during parity.
Production candidate `6493cf49a1e29a1914270c530956504faa6bb0e4` (original a1f0010a),
based on shared63f7c0b4. Docs786599b5+3997cf00 are separate; parent cleans/squashes.

- Source gates COMPLETE: release build0; release451passed/18ignored/0failed;
  debug451/18/0; all-targetClippy0 (14diagnostic headings unchanged); PostgreSQLunits0;
  difftest compile0. `status.tsv`, `audit-current.json`, source/binarySHA files.
- Remaining DUMP-ONLY v5 COMPLETE0 at13:18:28Z; PID305470 completed.
  Exactwholeline18.6 all3clients+actualbindcwd preflightPASS; FIRSTpg_dump Rust1/0.
  Independent14817/0count and canonicalcompleteplans96/14609/4/108 PASS.
  Actualdumpbinary9e876733 matches captured3997+regress/isolation; cleanup0,
  noownedleftovers/containerscreated. V3/v4 sealedhashesunchanged.
  `dump-only-v5/{status.tsv,final-audit.json,raw/}` preserves rawTAP/exits/provenance.
  Production6493cf49 integration-ready within these gates; fullcombinedserviceHELD.
- Corrected parity v4 TERMINAL1 at13:06:37Z; PID4018199 completed.
  COPY old-refusal/new1000rows=PG1000 proof,2073realfixture file hashes/canonical
  paths, Docker preflight and6arrayparser controls PASS.
  Corrected fullregress51148/51150, ALLbaseline rows+bothhunks identical PASS.
  Isolation121/122specs,1647/1648permutations,12234/12238steps knownbaseline PASS.
  Dump NOT RUN: versionregex mistakenly groups literal parentheses; rawpreflight
  genuine18.6. Saved-byte proof reproducesfalse-negative and literal-matchfix.
  This failedattempt remains1. Later separatev5 satisfiesrequiredstrictTAP;
  fullserviceHELD. No suite repeated or failure reclassified.
  `parity-v4/terminal-audit.json`, raw regression/isolation streams preserved.
- Parity v3 TERMINAL FAILED, overall1. Regress Rustexit0 but frozen units gate1:
  actual49868/51150 vs required51148/51150. `regress-gate-proof/result.json`.
  Smoke and eight array cases passed; pair9000–9149 completed0new/10gone.
  Isolation/dump NOT RUN; inherited a162-rref stopped by cleanup. No retry launched.
  Read-only triage sees COPY absolute-path refusals with nested src-repo symlink
  resolving outside allow-server-files root; setup cause likely, not yet source proof.
- Preserved failures: `parity-attempt1/` invalid hyphenated DBprefix and unsafe
  conditional phase boundary; `parity-attempt2/` checker missed libtest-prefixed
  first output line. Both rejected. No source suite repeated or accepted falsepass.
  `phase-boundary-proof/` COMPLETE: all8 original/repaired controls passed.
  Oldconditional masks23/29/31+createsmarker; subshell preserves23/29/31+nomarker.
  Commands/raw/exits/implementation hashes and rerun README retained.
- Full `pnpm test:pg --force` HELD until parent supplies intended combinedsource.
  Separate from --unit. Runner uses CI ROLL_E2E_BIND=0.0.0.0, e2ethreads2;
  one bounded Docker-to-host diagnostic is prepared, not launched.
- TAP helper a895aac2+bb0a1a8c ready and parent landed canonical equivalents
  5a9df14e+d599cf05. 16preserved controls/realfixtures replayed by parent;
  queued current dump uses exact plans/counts and mounted-workdir18.6preflight.
- Remote source backup confirmed `wip/resume-20261104-a162`=3997cf00.
  Evidence prior `wip/resume-20261104-a162-evidence`=d24435ba;
  see `evidence-backup.json` for newer checkpoints. Tempindex commits leave source clean.
- FDW63f7c0b4 vs5c312235 dist binaries/sourceIDs/SHA preserved. Timing INCONCLUSIVE:
  host remains busy; no accepted new performance run, no global PGsuperiority claim.
- Historical scoreboard/idle/scaled limits: `historical-scoreboard-scope.md`.
  Raw-deleted scaled measurements are weak historical prose, not current acceptance.

Existing cachetarget reused, no extra targets or global slot changes. Parent alone
pushes shared docs branch. Original a162 services retained for bounded parity; none new.
