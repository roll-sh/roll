# Verification owner recovery, 2026-11-04

Owned worktree: /home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc
Recovered original first prompt fully and activity455–504, plus provenance469–492.
Original HEAD68b13b503f28b1ecc9b752fd570ddc9e1c2d9e69, work-a162, clean.
Original unintegrated production a1f0010a, docs b1eab681+68b13b50.
Backup: original snapshot47f1e41c4b8eab0fc77da5262b04df814cf60bfb; rebased snapshot
f2ce82988e420f1675b473c990a2e1b4a741a3de. ExactHEAD3997cf00 is on
origin/wip/resume-20261104-a162 (parent independently confirmed).
Rebased cleanly onto shared63f7c0b4: production6493cf49, docs786599b5+3997cf00.
Current source-check job records exact HEAD, source patch and SHA256s, each command exit,
and requires actual successful test summaries. Stops at first failure. Old done0 wrappers
are not accepted as proof: their individual exits/logs are inspected separately.
Existing target reused: ~/.cache/a162/target (32GB). No new Cargo target dirs.
No old a162 cargo/guard jobs running at recovery. v4 and guard3 completed.
Slots4/jobs2 untouched; all new heavy work uses existing heavy.sh under nice19.

Legacy v4 (~/.cache/a162/v4/status and release/clippy/dt logs): sourcea39990c5+a1f0010a
(HEADb1eab681, later STATUS-only68b13b50): release autovacuum6,golden3,wire67 passed;
Clippy0 with warnings; difftest compile0. Does not include shared63f7c0b4.
Legacy v3 full release/regression/dump passed; regression report identifies62c85e5,
51,148/51,150. Legacy isolation121/122 specs,1,647/1,648 permutations. Legacy initial
pg_dump failed101; v3 retry passed. Earlier evidence is not current-source verification.
Legacy guard3 exact shared63f7c0b4 vs31066020: smoke passed;600 seeds,93,717 statements,
25,640 queries; no new mismatch lines,27gone. Still103wrong_rows,41engine_error and other
known classes. It excludes owner production6493cf49 and is not zero-mismatch evidence.

FDW63f7c0b4 vs parent5c312235 dist binaries built and preserved at
~/.cache/a162/ab/{5c312235,63f7c0b4}/roll-engine. No new timing launched on busy host.
TPC-B regression remains INCONCLUSIVE until bounded quiet alternating measurements
with actual work/errors/CPU affinity and limiter evidence. No timing blocks correctness.

pnpm test:pg --unit is planned; Docker recovery e2e remains the documented waiver
in tests/regress/STATUS.md (historical12passes/4WALHTTPtimeouts), not a fresh pass.

Exact current release build exit0 at2026-11-04T10:55:56Z. Fullrelease tests running.
check-full-service.sh prepared for ONE post-integration pnpm test:pg --force run;
not launched, no inherited waiver represented as a current fullpass.

Independent guard3 audit: guard3-audit.json verifies both complete600-seed
work counts and each real test result (not wrapperexit). Base410 unique lines,
head383,0new,27gone. Current fixed8case array-header probe is queued; no source edits.

Full current release suite exit0 at2026-11-04T11:22:52Z; real summaries audited
in audit-current.json. Debug suite now running on identicalHEAD3997cf00.

Strict TAP checker proved: real166 Rustfalsepass rejected; historicala16214817
complete results accepted;12 negative/positive controls pass. Current dump planned
inside knownclientbindmount with pg_dump18.6 preflight. SeeTAP-AUDIT.md.
