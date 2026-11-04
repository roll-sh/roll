# crash14 diagnostic preparation, revision 2

PREPARATION ONLY — uncompiled, unformatted and unrun. No execution is authorized.
This sibling replaces the proposed r1 patch; it is not an incremental patch over r1.
The original r1 cache is immutable. `independent-review-r1.json` retains the complete
incoming review and its task identity. `review-disposition.md` maps each finding.

The original failure is still exit 101 at source
`278a28966a8921da90c6ead9cfeb3f332306d788`, tree
`95a87e3cacd4100a09adc93f5c5362e7de2900b4`, original test-binary SHA256
`a35916fbfff20bcbe555d5a5c1d77d0688d24686f9696a1148ce2e1c8ec59ec5`.
Its sealed evidence is `aa48d3c9be313a6a67810e6d6d69b1f633685823`.
The failure remains unattributed. There is no storage-cause claim.

## Scope and identities

Only `crates/pg-engine/tests/crash_random.rs` and the new
`crates/pg-engine/tests/support/crash14_diagnostic.rs` change. No production,
Cargo, dependency, source checkout, target, ref or index changes are proposed or made.
The original harness sourcecopy is byte-identical to the exact base Git blob.
`source-patch-manifest.json` retains 882 object-based production/build-input hashes,
the exact base identity, and before/after test hashes. Expected inputs are not proof
of a future compiler's inputs. The artifact assembly script only constructs sourcecopies;
it does not apply the patch or execute any candidate code.

## Default behavior and oracle fidelity

`event` accepts `impl FnOnce() -> serde_json::Value`, checks a cached enable flag
first, and does no JSON construction, model formatting, result formatting, SQL hashing
or SQL formatting when disabled. Journal auditing and comparison also return before
work when disabled. Small branches/calls and the changed binary still exist; byte-for-byte
machine-code or timing identity is not claimed.

The disabled path keeps the original kill-thread body. The original step generator,
replay, row_diff, schema and append_log body are unchanged. In particular, the original
journal append's fsync is retained: r2 removes only diagnostic fsyncs from the kill window.
The original `read_log` expression still maps read errors to empty and parses flags with
`lines().map(|l| l.ends_with(" c"))`; auditing flags malformed input without repairing it.
The RNG draws remain `at` then delay in the same position in command construction.
The original check assertions and prepared-transaction decisions remain unchanged.

## Child instrumentation and kill boundary

There is no added child snapshot, check or nextval before its original workload.
The child keeps a full replayed model at its pre-workload boundary. Per-step events contain
step ID, end, existing SQL byte lengths and SHA256 digests, statement begin/result-ok,
explicit SQL CHECKPOINT marker, and journal-append completion with model row counts.
They do not format full models, operations or SQL results. There is no after-append journal
re-read. Journal audit runs only at existing read_log sites and after child exit.

Events use one append handle per channel/process, owned by a single thread, under
`CRASH_DIAG_DIR/events/`. Each JSON line includes its newline in one write_all call.
No event fsync occurs. A write_all call can still use multiple OS writes; SIGKILL can tear
its final record. Event builder panics and I/O errors are contained, and the channel closes
on an I/O failure or its 256 MiB byte cap. Missing evidence remains missing; no success
claim follows from an event write failure. A huge single boundary record may allocate
before the cap rejects it; the external disk/resource supervisor remains necessary.

The diagnostic kill thread sleeps and invokes kill directly, with no event, formatting,
model dump, journal read or fsync on that thread's path. The main thread captures Instant
and wall clock before spawning; its arm event is best effort after spawning. A separate
five-second post-arm watchdog and a 300-second child-entry watchdog also kill the child.
Each signal-command failure falls back to process abort. The disabled path starts neither
watchdog. No event error can prevent these kill paths. The external process-group deadline
is mandatory to cover hangs before/inside child launch, parent copying, engine operations,
blocked writes and descendants. SIGKILL status alone cannot distinguish the planned killer
from a watchdog; exact signal time is not observed.

## Original settle/check order and panic retention

After the child exits, the parent records complete exit status and stdout/stderr hex in a
separate synced child-exit file, before auditing the journal or copying files. Copy failure
cannot discard that completed file. Failure to write it stops capture before copying.

Before any parent Engine::open, the harness copies only the seed's `data/` and `steps`
into a new round image. Events are outside the imaged seed directory. The image is never
opened by this harness and is the reference for the state before parent open/settlement.
Copying and source/destination verification use 64 KiB streaming buffers; they do not
materialize three full files in memory. Each image has a 2 GiB data-byte cap, per-file hashes,
synced files/directories and a separate completion manifest. A cap/error leaves an incomplete
image, stops the run and never justifies a retry. Source immutability assumes the child is
exited and no other writer owns that database.

The parent then executes the original settle, original read_log, original check sequence.
There are no snapshot SQL reads before that check, including on the successful internal
prepared-transaction check in settle. The latter calls the original check body directly.
Full-model event formatting is retained at settle input/return and external check boundaries;
these are outside the child's kill window. Formatting still changes parent timing.

The outer check wrapper catches an original panic, performs best-effort diagnostics, and
resumes the same panic payload. A settle panic follows the same pattern. Snapshot panics,
SET/RESET errors and query errors cannot replace the captured original panic. The original
row_diff failure-message code remains unchanged; a panic originating there remains part of
the original oracle behavior. After a successful check, diagnostics may also collect rows;
there is no snapshot between successful settle and its first external original check.

## Diagnostic rows and incomplete evidence

The first mismatch records both complete ordered row lists and multiplicities, without SQL.
After the original panic unwinds, the snapshot attempts RESET ALL, then a two-second
statement_timeout. If either setup fails, row queries are skipped and capture is incomplete.
Queries and their errors/panics are individually recorded without assert/unwrap.

Snapshots retain prepared transactions, all txlog rows, pg_class index names including a_w,
and ctid, xmin and all columns of a and b. For each requested seqscan/indexscan/bitmapscan
setting, they capture both the original id > -1 predicate and unfiltered full rows, then
all generated e-table rows. A failed scan SET is recorded and that scan is skipped; subsequent
scan attempts continue. Final RESET ALL is also recorded, never asserted. The time limit's
coverage of every engine wait has not been verified; the process-group deadline bounds the
whole attempt. Settings request planner behavior; actual chosen plans are not verified.
Separate statements are not an atomic database snapshot. RESET failure can leave session
settings altered; a future reviewer must reject that capture as complete evidence.

## Interpretation limits

Seed and journal describe this harness's oracle reconstruction, not reproduction of the
retained original failure. Seed alone cannot reproduce the original run, and seed+journal
must not be advertised as reconstructing every interrupted SQL operation/model/engine state.
Full model boundaries are retained for that reason; per-step SQL digests are only consistency
and progress evidence. A statement may execute before its result event is written.

The instrumented binary and every observed crash boundary differ from the original.
Hashing, JSON serialization, event writes, watchdog thread scheduling, boundary formatting,
copying and after-check SQL perturb timing/caches and later outcome-dependent work. There
are no extra diagnostic child fsyncs, but the original workload/journal fsyncs remain.
No exact CPU instruction killed, internal checkpoint timeline, power-loss durability, original
sequence replay, storage cause, baseline attribution or production correctness is established.
Any timeout, missing event, cap, failed query/reset/copy, unobserved watchdog, or missing stage
makes the corresponding evidence incomplete. No pass clears original exit 101.
