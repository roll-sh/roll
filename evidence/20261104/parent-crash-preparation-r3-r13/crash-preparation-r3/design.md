# crash14 diagnostic preparation-r3

Status: PREPARATION ONLY — UNCOMPILED / UNFORMATTED / UNRUN. No execution is authorized.
This is a replacement two-file test-only patch against exact
278a28966a8921da90c6ead9cfeb3f332306d788, tree
95a87e3cacd4100a09adc93f5c5362e7de2900b4. It is not a patch on top of r1/r2.
Both previous cache directories remain immutable. The complete R1 and R2 incoming
review results are retained, with R2 findings answered in review-disposition.md.

Original failure: exit 101, still unattributed. Original binary SHA256:
a35916fbfff20bcbe555d5a5c1d77d0688d24686f9696a1148ce2e1c8ec59ec5.
Original sealed evidence: aa48d3c9be313a6a67810e6d6d69b1f633685823.
No diagnostic pass can clear it. There is no storage-cause claim.

## Exact source and build identity

Only tests/crash_random.rs and tests/support/crash14_diagnostic.rs change. The candidate
and its assembly generator restore Model's missing Debug derive. This corrects the R2
review's certain source-level compile blocker; it is not a compiler verdict for R3.
The original sourcecopy equals the exact base Git blob. The source manifest identifies
882 production/build-input objects and verifies their byte lengths/SHA256 without writing
a checkout, ref or index. No production, dependency or Cargo changes are proposed.

Both test files are embedded with include_bytes!, relative to the support file. At begin,
SHA256 and byte length of the EMBEDDED bytes are compared to each manifest after_sha256/
after_bytes, and the exact base/revision is checked. The manifest also records patch hash.
No self-hash is hardcoded: the raw source file is data in the compiled program, and its
hash is computed from those embedded bytes. This does not recursively expand the source
or introduce a source/manifest hash cycle. The instrumented executable is hashed separately.
This source tie does not prove every compiler input or release setting; the future plan
requires verifying those independently and executing that preverified binary directly.

Every newly generated event and JSON sidecar is wrapped with diagnostic_revision=
preparation-r3, event_schema=3 and preparation_status=UNCOMPILED/UNFORMATTED/UNRUN. That
last field describes this preparation artifact, not evidence that a future build happened.
The raw four-byte child-state sidecar has its interpretation recorded in the versioned
child-exit JSON. Historical review/plan files are retained verbatim, not rewritten.

## Original behavior and ordering

The disabled path evaluates no event JSON, full model/result/operation formatting or SQL
hashing/formatting. event and debug_dump check the cached enable flag before doing work.
The default kill-thread body, append_log including ORIGINAL fsync, replay, step generator,
row_diff, schema, prepared-transaction decisions and check assertions remain intact. Small
additional branches/calls and different machine code still exist; timing identity is not
claimed. The RNG draws remain at then delay, with no new random draws.

flags(io::Result<String>) is the original unwrap_or_default().lines().map(ends_with(" c"))
parser expression. read_log delegates after recording its original result. Missing/error
reads still produce empty flags. audit(&[u8]) is a pure strict annotation of bytes, offsets,
IDs, status fields and newline completeness; it never repairs or overrides those flags.
It is called only at original read_log sites or after child exit, not after child appends.
The offline controls cover read errors including normal initial NotFound, invalid UTF-8,
empty file/line, partial markers, both status merges, and a digits-only merge. None ran.

There is no added child SQL, full check, snapshot or nextval before workload. A pre-workload
model dump remains outside the kill window. After the child exits, evaluate/catch the
ORIGINAL code().is_none() assertion with its original child-failed message. Then capture
status, stdout/stderr and child state before any image copy. If the original assertion
failed, resume the same panic payload even if diagnostic capture also failed. Diagnostic
capture methods return Result, with unexpected diagnostic panics converted to capture
errors. No pre-oracle signal==9 assertion or copy unwrap can replace the original panic.

Once the original child oracle passed, diagnostic capture errors stop distinctly as
CRASH_DIAGNOSTIC_INCOMPLETE. Then preserve the never-opened pre-parent-open image, validate
child state/signal, open the working database, and run ORIGINAL settle -> read_log -> check.
No snapshot SQL occurs before that check, including after a successful internal check in
settle. There are NO snapshots after ANY passed check, even in target round 2. Thus earlier
rounds introduce no diagnostic SQL history and no after-pass RESET failure can leak into
continued original work. Full-model formatting/history still perturbs timing/resource use.

On an original settle/check panic, switch to reserved failure channels, attempt diagnostics,
then resume the exact original panic payload regardless of diagnostic errors. The check
failure also preserves its expected full model on the reserved channel. The original
row_diff code and its own possible failure behavior remain unchanged. The diagnostic-only
thread-local panic-hook flag gives caught diagnostic panics a distinct stderr tag; the
original hook still reports original workload/oracle panics normally. No global silence
of original panic messages is introduced.

## Bounded models, records and failure reserve

At every model boundary, stream the exact Debug UTF-8 bytes through a SHA256/byte counter,
without building a full string. Emit that digest+byte_length descriptor FIRST on the summary
channel; only after it succeeds emit the dump as ordered raw-hex chunks (32 KiB raw each).
End with dumped_bytes/complete. Debug bytes define the representation for THIS instrumented
binary; this is not a cross-compiler/version canonical serialization. Query results and
mismatch actual/expected lists use the same digest-first chunk mechanism. Ordered rows
preserve duplicates, instead of lossy set diffs.

Limits per process:

- 1 MiB maximum serialized JSON line including newline.
- 16 MiB raw maximum for each complete Debug dump; larger dumps retain digest, length,
  bounded partial chunks and an incomplete end marker.
- 128 MiB history data, plus a separate 8 MiB summary/digest budget.
- 256 MiB FAILURE data, plus an independent 8 MiB failure-summary budget.

The first mismatch activates failure channels before its row dumps; other original panics
activate them on catch. Prior passes cannot spend those reserved budgets. Each channel has
one writer and one append handle. Each JSON line uses ONE write_all call including newline;
OS partial writes and a torn final line remain possible. No child diagnostic event fsync.
Serialized record buffering is bounded. Existing SQL results/row lists belong to the original
query/oracle; the diagnostic formatter does not create another full model/result string.
JSON builders for small metadata still allocate; a pathological metadata value can allocate
before serialization rejects it. Dump hashing traverses complete models at boundaries.

Each channel preallocates a separate 4 KiB channel-closed-<channel>-<pid>.json slot before
its event file. While open it says channel_status/closed=false. Budget, record-cap, builder
panic and I/O loss attempt channel_closed/closed=true with reason and emitted bytes. Open/
setup failure gets an independent fallback marker attempt. Parent updates sync outside the
child's kill window; child updates never fsync, and parent syncs those slots after exit.
Filesystem-wide I/O loss can also prevent a marker update. Missing/invalid/torn markers,
missing dump descriptors/ends, cap/closed records or failed hashes mean INCOMPLETE evidence.
There is no claim that marker writes are infallible on failed storage.

## Kill and watchdog observations

The parent preallocates a four-byte child-state file outside the imaged seed tree. The child
opens it before starting its entry watchdog. Bytes mean started, armed, watchdog trigger,
reserved zero. Watchdog trigger values are 0=none recorded, 1=entry watchdog observed before
arm, 2=entry watchdog observed after arm, 3=post-arm watchdog. An atomic armed flag fixes the
trigger's classification even if main arms before the signal actually arrives.

Main writes armed through the preopened handle before taking its monotonic Instant and
spawning the original-style sleep/kill thread. The PRIMARY killer performs no diagnostic I/O.
The entry (300 s) and post-arm (5 s) watchdogs write one byte with no JSON/model work/fsync,
then kill. A state write error aborts with another signal. A blocked watchdog write is still
bounded externally by the whole-process-group deadline. Event loss cannot prevent the
primary killer. A successful signal command kills only this child process, not every process
in its group; the EXTERNAL deadline owns group-wide termination.

Child-exit JSON records exit code/signal, flags and watchdog-before-arm state. Only AFTER the
original child-status oracle does the parent reject pre-arm exit, any recorded watchdog,
missing/invalid state or non-SIGKILL signal as a diagnostic-incomplete status. Even a valid
armed SIGKILL with no recorded watchdog is not proof of the exact signal's source or time.
External timeouts are independently incomplete; they cannot be labeled intended kills.

## Images, failure reads and resource limits

Exit/output is in a synced independent file before journal audit/imagecopy. Images contain
only data/ and steps, never events or child-state files. The harness never opens image copies;
they are the reference before parent open/settlement. Copy and source/destination hash checks
stream through 64 KiB buffers. Symlinks/special files are refused. Each image has a 2 GiB data
cap and a separate completed manifest; partial/error images stay incomplete and stop the run.
No other process may write this retained database while copying.

After original failure only, snapshots attempt RESET ALL then 2 s statement_timeout. A setup
failure skips reads and records incomplete evidence. Capture all prepared/txlog rows, pg_class
indexes including a_w, and ctid/xmin/all a/b columns under all three requested scan settings,
with both original predicate and unfiltered full rows, then e-table rows. Each query result/
error/panic is retained through bounded digest/dump records. Failed scan SET is annotated and
skips that scan. RESET errors never replace the original panic. These are separate statement
snapshots; settings do not prove actual chosen access paths. Every engine wait's timeout
coverage is unverified; the process-group deadline is mandatory.

The sealed aa48d3c9 original plan explicitly configures RELEASE crash30 and
TMPDIR=/home/meszmate/.cache/a167/tmp. Original-failure-plan.json preserves its bytes;
filesystem-provenance.json preserves exact object provenance and CURRENT mount metadata:
ext4 for that path and the preparation cache at preparation time. Actual filesystem type
AT FAILURE was not captured. Do not claim it was generic /tmp, tmpfs, or historically identical
to today's ext4. Future begin requires configured filesystem type, records current mountinfo,
checks a match and rejects tmpfs/ramfs. The future external supervisor must verify persistent
disk, the 30 GiB floor, hard 12 GiB total budget and 20-minute process-group deadline. Initial
free space must be at least 42 GiB. No such resources have been probed or reserved here.

Seed alone cannot reconstruct the retained original model or reproduce the failure. Even
seed+journal is not all interrupted model/SQL/engine state. Preserved models/digests remain
necessary; digest-only/partial captures cannot be called full model evidence. Instrumented
binary, hashing/formatting, sidecar writes, file copies, watchdog scheduling and after-failure
reads change timing, cache state, fsync latency and crash boundaries. Storage-medium identity
at the failure is unknown. No exact killed instruction, internal checkpoint timeline, power
loss, cause, original failure clearance or production correctness is established.
