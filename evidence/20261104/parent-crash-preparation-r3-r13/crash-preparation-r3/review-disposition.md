# Full R2 review disposition — preparation-r3

Status: UNCOMPILED / UNFORMATTED / UNRUN. These are static candidate corrections,
not compiler, parser-control or engine results. The complete incoming R2 review is
retained verbatim as `independent-review-r2.json`; the R1 review is also retained.
Neither review grants execution permission. R1 and R2 remain immutable.

1. HIGH, certain R2 compile blocker: restore `#[derive(Clone, Default, Debug)]` on
   Model in the candidate AND the assembly generator. Source text assertions check the
   emitted derive. This fixes the identified missing trait declaration at source level;
   R3 remains uncompiled, so no claim that all possible compile blockers are cleared.
2. Model size/digests/budgets: use streaming Debug formatting into SHA256 + byte counter,
   emit the descriptor FIRST, then bounded 32 KiB raw-byte chunks, then completion/length.
   Every serialized event is capped at 1 MiB; each full dump is capped at 16 MiB raw bytes.
   Model and query result dumps share this mechanism, avoiding eager full Debug strings.
   History has 128 MiB, history summaries 8 MiB, failure data 256 MiB and failure summaries
   another 8 MiB PER PROCESS, with independent handles/budgets. Pass history cannot consume
   the failure reserve. Ordered row dumps retain duplicates. A cap/truncation is incomplete
   evidence, never successful full-model retention.
3. Channel loss: preallocate a separate 4 KiB status slot before opening each event file.
   Budget, serialization, builder-panic, open and write failure paths attempt an explicit
   channel_closed update; setup failures also try an independent fallback marker. Parent
   marker writes sync outside the child's kill window. Child writes NEVER fsync; the parent
   syncs child status markers after exit. A filesystem-wide I/O failure can also defeat
   marker writes: stderr records the attempt and missing/invalid markers remain incomplete.
4. Passed-check perturbation: remove ALL snapshots after passed checks, including round 2.
   Snapshots run only after an original settle/check panic; no diagnostic SQL after passes
   can leave RESET/statement_timeout state for continued workload. Therefore the requested
   after-pass RESET-failure gate is unnecessary in this simpler candidate. Snapshot SET/
   RESET errors after failure are recorded and cannot replace the original panic. The
   original settle -> read_log -> check order and internal prepared-check order stay intact.
5. Binary/source/profile/revision: embed both test files with include_bytes!, hash their
   embedded bytes at begin(), and compare exact paths, after_sha256 AND after_bytes against
   the manifest. Validate base/revision. No hardcoded self-hash or manifest/binary hash cycle.
   Every newly generated event and JSON evidence sidecar is wrapped with diagnostic_revision,
   event_schema and the UNCOMPILED/UNFORMATTED/UNRUN preparation status. Manifest/PREPARATION
   carry the same status. Future compilation must use RELEASE to match sealed CRASH stage.
   Runtime plan invokes the preverified release test executable DIRECTLY, never Cargo.
6. Original child oracle precedence: evaluate/catch the original `code().is_none()` assertion
   with its unchanged `the child failed: {why}` message BEFORE diagnostic assertions. Capture
   exit/output next, before any copy, and resume the exact original payload if it failed even
   when capture returned an error. No pre-oracle signal==9 assertion or diagnostic copy
   unwrap remains. After the original oracle passes, capture/status errors stop distinctly as
   CRASH_DIAGNOSTIC_INCOMPLETE. All signals are recorded, not silently treated as planned kills.
7. Pure parser seams/controls: `flags(io::Result<String>)` contains the exact legacy parser,
   including read-error -> empty; read_log delegates to it after observation. `audit(&[u8])`
   is pure and annotates only. Add digits-only merged tear, empty file, empty line and initial
   NotFound controls, retaining the two earlier merged-marker controls. All controls are UNRUN.
8. Medium/profile correction: the retained aa48d3c9 weighted-terminal plan explicitly uses
   cargo test --release for crash30 and TMPDIR=/home/meszmate/.cache/a167/tmp. Its bytes are
   retained as original-failure-plan.json with object provenance. Current /proc/self/mountinfo
   reports ext4 at the original configured TMPDIR and preparation cache. This is CURRENT
   metadata, not historical identity: actual filesystem type at failure was NOT captured.
   Future begin records configured/current type, rejects tmpfs/ramfs and mismatched types;
   fsync-latency/medium differences remain a crash-boundary perturbation limit.
9. Watchdog and panic-hook details: preallocate a four-byte child state file, outside images.
   Record started/armed and watchdog trigger (entry before arm, entry after arm, post-arm).
   The primary killer still performs no diagnostic I/O. A watchdog writes one byte through a
   preopened handle, without fsync, then kills; write failure aborts with a different signal.
   After the original oracle and image capture, the parent rejects pre-arm exits, any recorded
   watchdog trigger, missing/invalid state and non-SIGKILL signals as diagnostic incomplete.
   Even armed SIGKILL without a recorded watchdog is not proof of exact signal origin. A
   diagnostic-only thread-local panic-hook tag distinguishes caught diagnostic panics while
   the original hook remains active for original oracle panics. No rustfmt was run.
10. Prior safeguards retained: disabled closures perform zero model/JSON/SQL formatting;
    no child model dump, re-audit, result dump or diagnostic fsync inside the kill window;
    original append_log fsync and RNG draw order retained; all a/b columns with ctid/xmin,
    all three requested scan modes and pg_class indexes after failure; never-opened data+steps
    images; streaming 64 KiB copy/hash; separate child exit/output before copy; no source,
    production, dependency, index, ref or target mutation. Captures remain bounded/incomplete
    on error, with the 30 GiB free floor, hard 12 GiB budget and whole-group deadline in plan.

Model digests are defined over the exact Rust Debug UTF-8 bytes of this instrumented binary,
not a cross-version serialization. Seed alone cannot reconstruct the retained original model
or reproduce the original failure; even seed+journal is not full interrupted-state evidence.
Preserved full models/digests remain necessary. Instrumentation changes binary, scheduling,
I/O and crash boundaries. No runtime permission, original exit-101 clearance, storage cause,
power-loss equivalence or production-correctness claim follows from this preparation.
