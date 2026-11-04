# Inactive future plan — no execution authorized

Current status: UNCOMPILED / UNFORMATTED / UNRUN. This plan is not a launcher and grants
no compile, formatter, parser-control, engine, crash, SSI, storage, service or container run.
All R2 findings are addressed in the R3 source preparation/disposition before proposing any
future compile. R3 still requires independent review; textual correction is not compilation.

1. Parent reviews the full R3 patch/disposition, then separately authorizes an isolated
   workspace, target and compilation phase. Materialize exact 278 production/build inputs
   and only the two test changes. Verify manifest before/after. No owner/shared source,
   ref, index, or known binary/target may be overwritten. The current preparation creates
   neither workspace nor target. Preserve the original a359 binary only after verifying
   its bytes, before any authorized operation that might replace it.
2. Under that separate authorization, use RELEASE profile, matching aa48d3c9's sealed
   original crash30 `cargo test --release`. Format only the approved isolated test changes,
   regenerate patch/manifest/hashes and review any formatting diff. Compile without running
   using the existing heavy.sh admission slots, nice 19, jobs 2 and the parent's approved
   target. Resolve compile failures in preparation; preserve exits and evidence. No broad
   suite or run is included. Build toolchain/flags/dependencies must match the base inputs;
   include_bytes embeds both exact post-format source files, whose hashes must match the
   regenerated manifest. A build cannot be called successful from a static review.
3. Identify the produced RELEASE test executable from build output, preserve a dedicated
   copy in a parent-approved disk cache, and record its SHA256 and source/build manifest.
   Verify embedded test hashes in its begin record, and verify the actual executable SHA
   immediately before/after any later run. Do NOT invoke cargo test during runtime: that
   could rebuild and substitute a binary after preverification. Runtime uses that binary
   DIRECTLY. Set the executable location in the approved parent launcher; no guessed glob.
4. Under separate pure-control authorization, invoke candidate flags(Result<String>) and
   audit(&[u8]) using only literal inputs/injected errors from offline-controls.json. Exercise
   NotFound and PermissionDenied without actual files or chmod; do not use Engine/Session.
   Controls are unrun now. They test parser/audit annotation, not storage/cause/reproduction.
5. For the separately authorized ONE diagnostic, select a NEW absolute persistent-disk
   CRASH_DIAG_DIR under the disk cache. No reuse, deletion of evidence, /tmp, tmpfs/ramfs,
   /dev/shm, build/source tree, or unrelated workspaces. Record configured and current
   filesystem type/mount/source before runtime and pass the verified current type as
   CRASH_DIAG_FS_TYPE; begin independently reads mountinfo and requires an exact match.
   The original plan configured TMPDIR=/home/meszmate/.cache/a167/tmp, not generic /tmp;
   keep that configured TMPDIR in the recorded runtime environment. Actual filesystem at
   original failure is unknown. Current preparation metadata says ext4, which is not proof
   of original medium identity. Treat fsync-latency/medium differences as perturbation.
6. Enforce a 30 GiB free-disk FLOOR for the entire backing filesystem and a HARD 12 GiB
   capture-tree budget, including working DB, images, all event channels, raw output and
   partial files. Start only with at least 42 GiB free and independently enforceable budget/
   reserve. Prefer quota/reservation; polling alone is not a hard limit. An external guard
   checks at <=1 s intervals and terminates the owned whole process group on either floor
   or budget breach. Do not prune, expand budget, restart, or continue. No quota, reservation
   or storage probe is performed in preparation. Per-image 2 GiB and per-process channel
   caps do not bound working DB, output, metadata, sparse allocation or existing query memory.
7. Impose a 20-minute OVERALL process-group deadline, TERM then KILL after 5 s, around
   the direct executable plus heavy/resource wrapper and all descendants. GNU timeout
   without --foreground in its own group, with no detached sessions, is one possible
   approved mechanism. Killing only parent/cargo can orphan children. The disk guard must
   own the same group without affecting unrelated jobs. Verify group semantics before any
   authorized attempt. Child entry 300 s and post-arm 5 s watchdogs do not replace this.
   Record supervisor exit/deadline reason separately; every timeout is incomplete evidence.
8. Authorize exactly seed 14, three zero-based rounds 0..2, one attempt. Preserve original
   settle -> read_log -> check. There are no snapshots after passed checks. Failure-only
   snapshots never continue workload; RESET errors cannot masquerade as new workload errors.
   Any recorded watchdog/pre-arm exit, invalid state or unclassified signal stops distinctly
   AFTER the original child oracle, with original panic precedence. No retry/fourth round/
   extra seed/baseline/broad suite/follow-up image open or probe is included.

Inactive runtime parameter sketch inside the approved guard/deadline/heavy wrapper:

```text
CRASH_SEEDS=14
CRASH_ROUNDS=3
TMPDIR=/home/meszmate/.cache/a167/tmp
CRASH_DIAG_DIR=<new-absolute-persistent-disk-capture-root>
CRASH_DIAG_FS_TYPE=<preverified-current-disk-filesystem-type>
CRASH_DIAG_SOURCE_MANIFEST=<verified-r3-source-patch-manifest.json>
<absolute-preverified-release-crash_random-test-executable> --exact random_crashes --nocapture --test-threads=1
```

There is deliberately NO Cargo invocation in the runtime deadline. Record exact binary/
manifest hashes, command, environment, release-profile/toolchain inputs, stage exits and
resource supervisor results. Inspect child-exit/output before image manifests; check armed/
watchdog flags, channel-closed slots (false=open; true=loss), record/dump caps, digest/length/
chunk completion, failed snapshots and all raw artifacts. Missing evidence remains missing.
Images stay unopened. A pass cannot clear original exit 101, reproduce the retained failure
from seed alone, establish medium identity, attribute storage cause or emulate power loss.
No execution, ref write, push, export, cleanup, evidence deletion or children is authorized now.
