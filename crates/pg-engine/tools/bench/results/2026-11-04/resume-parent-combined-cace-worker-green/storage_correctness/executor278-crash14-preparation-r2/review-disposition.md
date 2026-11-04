# Independent r1 review disposition

Authority for issues: the full summary in `independent-review-r1.json`, copied verbatim
from the parent's cached terminal result for crash14-diagnostic-review-r1. That review
is not execution authorization. All r2 changes below are source-level preparation only.

1. HIGH disabled overhead: event receives a lazy closure and checks enabled first.
   JSON/model/SQL/result formatting is inside closures; journal/comparison short-circuit.
   The default kill-thread body remains original. No added child check or snapshot.
2. HIGH kill-window I/O: one handle per channel, newline in one write_all, no diagnostic
   fsync, full-model dump, operation dump, SQL-result dump or journal re-audit in that window.
   Existing original append_log fsync is preserved. Events use SQL digest/length and outcome
   fields; full model boundary dumps are retained. The review's model-reconstructibility
   suggestion is deliberately qualified: the original failure cannot be reproduced from
   seed alone and seed+journal is not complete interrupted-state evidence.
3. HIGH unbounded child: the killer performs no event I/O at all. Independent 5-second
   post-arm and 300-second child watchdogs invoke kill, with abort fallback. Event errors
   are caught. The future plan also requires a 20-minute overall process-group deadline,
   persistent disk cache, 30 GiB disk floor and hard 12 GiB capture budget. These mechanisms
   are proposed, not exercised. Watchdog versus planned SIGKILL is not distinguished.
4. MEDIUM-HIGH read-before-check perturbation: parent choice implemented. No snapshot
   reads before the original settle/read_log/check sequence. An internal successful check
   in settle runs the original check body directly. Snapshots occur after original failure
   or after the external original check. The never-opened image is the pre-open reference.
5. MEDIUM partial-marker merge: offline-controls.json includes both requested byte-segment
   concatenations, exact legacy flags, and invalid audit expectations. No controls executed.
6. MEDIUM versions/scans/index: after failure, attempt all three requested scan settings,
   complete matching and unfiltered rows of a/b including ctid/xmin/all columns, and pg_class
   index entries including a_w. Per-query timeout/errors and incomplete markers are retained.
   Actual chosen access paths remain unverified; planner settings alone are not path proof.
7. MEDIUM lost evidence: a separate child-exit/output file is written and synced BEFORE
   journal audit or copying. Catch the original settle/check panic; diagnostic failures are
   caught and the original payload resumed. RESET ALL is attempted before snapshot timeout
   setup; no diagnostic SET/RESET assert can replace a panic. Existing row_diff is unchanged.
8. LOW-MEDIUM image bloat/memory: events live under root/events, never in the seed image.
   Only data/ and steps are copied, using bounded streaming buffers for copy/hash/verification.
9. LOW record/timing/format: one write_all includes newline; monotonic Instant is captured
   in main before killer spawn. No exact signal-time claim. rustfmt was NOT run because the
   current instruction explicitly forbids it; future formatting/compilation needs separate
   authorization and regenerated hashes. r1's no-known-compile-blockers verdict is limited
   to r1. R2 has only been textually inspected, not compiled or tested.

Original failure remains unattributed exit 101. No storage cause, power-loss equivalence,
original crash-boundary equivalence or clearance is claimed. A future timeout, missing
record, event cap/error, incomplete image or failed snapshot is incomplete evidence.
