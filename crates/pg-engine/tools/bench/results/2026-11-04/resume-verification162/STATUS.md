# Verification162 — current checkpoint

Worktree: `/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc`
Branch `work-a162`, clean pinned HEAD `3997cf00a2a301fed65f00469625ff14f223f227`. Do not move during parity.
Production candidate `6493cf49a1e29a1914270c530956504faa6bb0e4` (original a1f0010a),
based on shared63f7c0b4. Docs786599b5+3997cf00 are separate; parent cleans/squashes.

- Source gates COMPLETE: release build0; release451passed/18ignored/0failed;
  debug451/18/0; all-targetClippy0 (14diagnostic headings unchanged); PostgreSQLunits0;
  difftest compile0. `status.tsv`, `audit-current.json`, source/binarySHA files.
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
  Evidence latest confirmed `wip/resume-20261104-a162-evidence`=f77f85cb;
  see `evidence-backup.json` for newer checkpoints. Tempindex commits leave source clean.
- FDW63f7c0b4 vs5c312235 dist binaries/sourceIDs/SHA preserved. Timing INCONCLUSIVE:
  host remains busy; no accepted new performance run, no global PGsuperiority claim.
- Historical scoreboard/idle/scaled limits: `historical-scoreboard-scope.md`.
  Raw-deleted scaled measurements are weak historical prose, not current acceptance.

Existing cachetarget reused, no extra targets or global slot changes. Parent alone
pushes shared docs branch. Original a162 services retained for bounded parity; none new.
