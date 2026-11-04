# Inactive future plan — no execution authorized

This is a reviewable plan, not a launcher. Nothing here authorizes a build, formatter,
parser control, test, engine, service, container, storage probe, crash, SSI run or retry.
The parent must separately authorize each future phase and select its workspace/target.

1. Review r2 against the complete cached r1 review. The r1 review found no known compile
   blockers in r1; that does not establish compilation of r2. All candidate code is uncompiled,
   unformatted and unrun. Under later authorization, materialize only exact 278 production
   inputs and these two test changes in an isolated, parent-selected workspace. Confirm all
   build inputs against the source manifest. No owner/shared checkout or target mutation.
2. Under separate authorization, format the isolated sourcecopies, update hashes/patch, and
   compile using the existing heavy.sh admission slots, nice 19 and jobs 2, on the parent's
   approved existing target. Inspect live jobs and existing target identity first. Preserve
   any available original a359 binary only after verifying its bytes against its recorded
   hash and before any operation that could replace it. Record actual toolchain/compiler
   inputs, stage exits and new instrumented binary hash. Compilation failure stops the phase.
3. Under separate authorization, run only the offline journal controls, with no engine or
   real storage access. Validate exact old flags and audit annotations, including both
   merged-marker cases. A parser control is not an attribution or crash reproduction.
4. Before authorizing the one future diagnostic, choose a NEW absolute CRASH_DIAG_DIR below
   the persistent disk cache, for example a new directory under
   /home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/.
   Never put it in /tmp, /dev/shm, another tmpfs, or the build/source tree. Verify the backing
   filesystem is disk; path checks alone do not prove that. Do not delete earlier evidence.
5. Enforce a 30 GiB free-disk floor throughout and a 12 GiB hard capture-tree disk budget,
   including working DB, all three images, child-output files, events and partial files.
   Start only with at least 42 GiB free on that backing filesystem and an independently
   enforced budget/floor guard. The harness caps each image at 2 GiB and each of the four
   expected process/main event channels at 256 MiB, but those caps do not cover working DB,
   sparse allocation, output, metadata, or serialization memory. The external guard must
   terminate the whole process group on either threshold, retain artifacts and record
   incomplete capture; do not prune, restart, or silently expand the budget. Check at no
   more than one-second intervals; filesystem quota/space reservation is preferable where
   available, since polling alone is not a hard disk limit. Do not start without an enforceable
   hard budget and sufficient reserve. This preparation installs or probes none of these.
6. Set an owned OVERALL 20-minute process-group deadline, with TERM then KILL after five
   seconds, enclosing cargo/test and every descendant. GNU timeout without --foreground,
   in its own process group and without detached child sessions, is a possible approved
   mechanism. Killing only cargo/parent is insufficient and can orphan crash children.
   Confirm the wrapper's group semantics before any authorized run; never signal shared
   unrelated jobs. A separate supervisor must enforce the disk guard on the same group.
   The child has a 300-second watchdog and a five-second post-arm watchdog; neither replaces
   the overall group deadline. Any deadline is incomplete evidence and stops the one attempt.
7. Authorize exactly seed 14, three zero-based rounds 0..2, one attempt. The original parent
   settle -> read_log -> check order remains. No extra seed, fourth round, baseline, broad
   suite, retry or follow-up probe is included.

Future invocation parameters, inside the approved deadline/disk/heavy wrapper:

```text
CRASH_SEEDS=14
CRASH_ROUNDS=3
CRASH_DIAG_DIR=<new-absolute-persistent-disk-cache-capture-root>
CRASH_DIAG_SOURCE_MANIFEST=<verified-r2-source-patch-manifest.json>
CARGO_BUILD_JOBS=2
cargo test --release -p pg-engine --test crash_random -- --exact random_crashes --nocapture --test-threads=1
```

Record wrapper, compiler, test and supervisor exits separately, exact environment, source
verification before/after, and binary hashes. Inspect child exit/output files BEFORE image
manifests, never-opened data+steps copies, event completeness, timeouts and reset failures.
Do not open copied images under this plan. SIGKILL does not emulate power loss. A successful
instrumented run is a non-reproduction and cannot clear exit 101 or attribute a storage cause.
No runtime, push, ref write, export, evidence deletion or cleanup is authorized now.
