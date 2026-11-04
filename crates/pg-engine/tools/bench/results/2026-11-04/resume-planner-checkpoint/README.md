# Recovery checkpoint (2026-11-04)

Evidence only. No candidate source is accepted or performance advantage claimed.
All three original worktrees and histories were inspected. The T3 bindings pointed to the
parent checkout; all implementation Git operations instead used the original worktrees.

OLTP165 recovered clean at 6fde4096. jobA failed (including an executable-copy failure).
jobB's final HEAVY-EXIT 0 is FALSE SUCCESS: every cargo step failed on a missing AtomicU64
import. Its debug/release failure logs are preserved. baeee87a fixes only that import;
complete current-source debug validation has now passed446tests,0failed,18ignored.
The release/clippy/build/PostgreSQL checks and footprint/performance gates remain pending.
PostgreSQL18.6 recording of a165_cursor_stream succeeded: nonempty 397-line expected output.
The other two golden tests return early in recording mode: this is recording evidence,
not itself a successful embedded/wire/shuffled comparison. The subsequent complete
debug suite did pass all three modes; its completed log is exported separately. Snapshot 2ef850e8 retains this golden.

Historical matrix m0/m1 contain partial campaigns and lack raw pgbench output and a
complete current-source gate set. They cannot establish a performance regression or gain.
Old baseline/pre footprint outputs do not verify baeee87a. New runner scripts are retained
locally and are not reported as executed until their individual exit logs exist.

Planner171 recovered at 5c312235 with six dirty SQL files; their exact snapshot is ec5b9f39.
An earlier cargo check succeeded, but the log lacks sufficient source identity to accept
current source. Chosen-cost recording and custom-plan use remain this owner's scope.

FDW172 recovered clean at 63f7c0b4, no round8 implementation. Baseline dist and contrib
completed. Report: postgres_fdw1945/2053, dblink218/218, file_fdw187/187, aggregate2350/2458.
This is baseline evidence only and does not establish an improvement.

Owner backups were checked with ls-remote: baeee87a wip/a165-resume-20261104,
2ef850e8 wip/a165-resume-golden-20261104, ec5b9f39 wip/a171-resume-20261104,
63f7c0b4 wip/a172-resume-20261104. Shared branch and parent/main worktrees were not mutated.

The manifest records exact hashes of every exported artifact. This checkpoint includes the completed debug-suite output and exact testedtree.
The still-running release-suite output is excluded, not reported as a finished check.
