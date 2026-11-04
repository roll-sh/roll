# Untested encoding-boundary and proof-correction successors

Status: UNRUN WIP. No source/performance acceptance. This work used source edits,
static byte/Git inspection, diff --check, isolated commits and dedicated WIP backups
only. No Rust compiler/build/test/fmt, services, containers, benchmarks, target dirs,
new worktrees, or child agents. Parent owns all execution and the reserved a170
resources. No shared checkout, owner source/index, shared branch or original backup
ref was changed. No PR was created.

Final candidate:
- Commit: cace40de4b9df2af42b08027138d9830d17abe9f
- Tree: 0d8fad517fe8bda96011bebdbae50334e465e79a
- Local branch: feat/combined-index-validation
- Remote: refs/heads/wip/combined-index-models-encoding-proof-r2-d62e3d3b

Final corrected TEST-ONLY red:
- Commit: b56ed0fb4c9a6113cfef09504e2b695a1cf17b70
- Tree: fbbdca36b12c4729276ddca646ff109ad2fccb9e
- Local/remote: refs/heads/wip/combined-index-models-red-proof-r2-d62e3d3b
- Parent: 85ed525400864c57e9e8d79b3cfac0b4ad572538, itself based on immutable b43.
- Exactly two changed test lines from b43: tuple annotation and observer condition.
  All production bytes remain b43's/b4's. Plan::row still has the old modeled early
  return. No encoding repair/test or stats fix/test is present in this red.

All nine current/original backup refs in remote-shas.txt were verified by exact
ls-remote SHA. Original 4e, e25, b43, their backups and their prior JSON artifacts
are preserved. The old artifact text calling b43 never-run is historical: the
parent has since attempted compilation, as recorded below. The NEW successors
have not been compiled or tested.

## Auditable candidate commits

1. 54bc85f932e56c135d5f0355726477d926f171f8 (parent e25): TEST ONLY, kept.rs +1/-1.
   Explicit (String, String, &str, &str) annotation for BuildCase's tuple. Resolves
   the source of the parent's E0283 report by reading; compilation remains unproven.
   Backup: refs/heads/wip/combined-index-models-test-compile-r1-d62e3d3b.
2. 9278e91303ad8ba6e847b61e00d2ac392c83156c: encoding boundary, 3 files +85/-0.
   Production: parbuild.rs +3 and collation/mod.rs +5. Existing kept::tests +77
   defines combined_index_worker_database_encoding. Backup:
   refs/heads/wip/combined-index-models-encoding-r1-d62e3d3b.
3. 6ef02669495a120574cddcd6e0b49b4892c0a8ec: cfg(test) observer only, +1/-1.
   An abort marks a waiter only if its readiness predicate is still false. Gate
   errors remain checked first and fail closed, even when ready. This common wait
   helper covers both waits in first() and completed_first(). Backup:
   refs/heads/wip/combined-index-models-proof-r2-d62e3d3b.
4. cace40de4b9df2af42b08027138d9830d17abe9f: TEST ONLY, kept.rs +9/-0.
   Adds no-conflict INSERT ... ON CONFLICT DO NOTHING size-failure stats control
   at both lengths. Expected delta follows accepted accounting, not a PG capture.

Total since e25: 4 files, +96/-2. New production change is only the 8 encoding
boundary lines. All earlier validator/accounting/fast-path corrections stay intact.

The red sequence is separately auditable:
- 85ed525400864c57e9e8d79b3cfac0b4ad572538: tuple annotation only, parent b43.
  Backup refs/heads/wip/combined-index-models-red-compile-r1-d62e3d3b preserved.
- b56ed0fb4c9a6113cfef09504e2b695a1cf17b70: observer condition only, parent 85ed.
Total from b43: 2 test files, +2/-2. Private temporary Git indexes and commit-tree
created these commits without checking out b43 or touching another checkout.

## Encoding boundary and guard semantics

The leader captures its transaction's Arc<Catalog> once immediately before entering
the thread scope. Each scoped worker calls collation::scoped_default(catalog)
before Plan::run. The catalog reference lives until scoped workers are joined.
Only the catalog's database-default Arc is installed: its DbLocale includes the
encoding and default collation. There is no whole-catalog copy, binder/cast-family
mutation, per-row guard, or broad formatting/locale rewrite. Leader TLS is untouched.
Both modeled and measured workers use this entry boundary.

The helper uses existing scoped::set_ref on collation DEFAULT. Inspection of
scoped.rs shows it replaces the value with no RefCell borrow held during the scan;
RefGuard saves the prior Option<Arc<DbDefault>> inside another Option, so a previous
None is restored too. Normal return, Result error and standard unwinding drop the
guard. The guard is constructed before the optional panic checkpoint, so even a
checkpoint panic unwinds through it. Drop uses try_with/try_borrow_mut, following
the project's existing policy of declining restoration if TLS is gone or remains
borrowed. No DEFAULT borrow spans the new worker scope, and the guard is created
and dropped on that worker. These are static observations, not an executed unwind
proof. No reusable-worker pool or other worker families were changed.

The shared relsize/format_value core, conservative non-ASCII/bpchar checks and all
size/unique/finish ordering remain untouched. This addresses the concrete default
encoding route in FORMAT-AUDIT.md statically, not every possible formatting TLS
use and not PG parity of every non-UTF8/compression case.

## Discriminating new test definition, never run

combined_index_worker_database_encoding runs four fresh fixtures: LATIN1 and UTF8,
each with zero or exactly two maintenance workers. It asserts the server encoding
and SQL octet_length/length of the inserted non-ASCII value. A modeled table holds
6000 rows, int key plus 16 INCLUDE text fields, each repeat('é',120).

Each individual value is 120 LATIN1 bytes or 240 UTF8 bytes, below the existing
510-byte index-compression threshold. The complete entry fits below 2704 in LATIN1
and exceeds it in UTF8. No exact PG tuple-size/page/TID golden is invented. LATIN1
must successfully build; UTF8 must return 54000 at the deferred btree limit. On the
old worker boundary the leader uses LATIN1 while fresh workers size as UTF8, so the
LATIN1 worker build is predicted to fail. That is static discrimination, not a
recorded red. The LATIN1 heap tuple is also below 2032; the UTF8 control stays below
a page and fails index validation before installation. This fixture does not prove
TOAST behavior.

The existing scoped observer is reused without architecture changes. Parallel
assertions require requested/reserved/spawned=(2,2,2), three distinct real threads
and first chunks, nonzero rows on every participant, validated==rows,
model_returns==rows, 6000 validations total, expanded non-ASCII INCLUDE bytes=240,
and per-participant deferred candidates only for UTF8. Serial asserts 6000 row
validations and no participants. Full trace prints before assertions/results are
extracted; unexpected success/failure includes the trace. Missing eligibility,
workers, chunks, model state or gate progress fails closed. Pool allowance stays
1024 and maintenance request stays 2. Every gate wait remains individually bounded
at five seconds. This does not waive extreme global ACTIVE contention or slow-host
failures. Current model/chunk/setup assumptions remain unverified.

## External execution evidence and review inputs

Parent's first immutable b43 attempt failed compilation with E0283 at kept.rs:708,
exit 101, ZERO tests. Read in full:
~/.cache/pg-engine-resume-20261104/combined-first-proof-r1/red.log and red-result.json.
Raw log SHA256: 44554d503591ca80a16c80b87ff3fc99a6c95785090cf8a399b0b673ac06a002.
Parent reports sealed backup 0d92fb1c. This is a compile failure, not a behavioral
red or proof of the HIGH. No local execution was used to validate its correction.

Read full combined-proof-review-r2-result.json. Its six R1 closures and absence of
new production objection are static review statements, not acceptance. The new
MEDIUM ready/abort race is corrected in both successor lines; no executed
race/red/green evidence exists. The speculative control is a new unrun definition,
not a captured oracle or a stats red. DESC/minimum/abort changes likewise do not
acquire red/green evidence from the original-HIGH red alone.

Read full models-source-review-r4-result.json. The measured parbuild raw/detoasted
heap-size path can still overcount toasted rows or underflow HeapPacker. It is a
separate root TOAST gap. Plan::row, including that block, is byte-identical to e25.
No 047, 1884, 1b6a, Load, VACUUM or other owner source repair was integrated. Parent
owns ordering of that work. The original combined 8300 INSERT SELECT fixture may
still fail in raw Load before CREATE INDEX; do not skip or weaken its proof.

## Static artifacts and remaining execution

verify.py records candidate/red committed blobs, lengths, SHA256, tree, exact remote
refs and per-commit scope. Candidate bytes match the clean worktree. Historical e25
manifest objects and prior JSON artifact hashes still match. preservation.json
repeats all 27 accepted-hunk/golden byte checks into this NEW directory, without
rewriting the historical reports. Candidate production-scope checks exclude only
the cfg(test) kept tail/observer; parbuild/helper additions are compared explicitly.
Plan::row is unchanged, and red's old Plan::row matches b4 exactly. git diff --check
passes for e25..final and b43..new-red. Existing imported golden EOF blanks are
preserved; no goldens were altered.

Parent proposals only, through its reserved resource wrapper/targets:

    cargo test -p pg-engine --lib combined_index_build_validation -- --nocapture --test-threads=1
    cargo test -p pg-engine --lib combined_index_worker_database_encoding -- --nocapture --test-threads=1
    cargo test -p pg-engine --lib combined_index_size_error_stats_unoracled -- --nocapture --test-threads=1
    cargo test -p pg-engine --lib -- --nocapture

Run the original-HIGH proof first on b56 red then final candidate, retaining full
raw traces and source manifests. The new encoding test exists ONLY on candidate;
filtering it on b56 would run zero tests and prove nothing. A separate parent-owned
test-only backport/revert is required for an encoding-specific red if scheduled.
The final candidate has e25 accounting/fast-path and the new encoding production
delta, which must be disclosed when interpreting any red/green pair.

Still pending: compile viability of all new definitions; worker/encoding behavior;
bounded-gate reliability including R2 race; non-UTF8 and size/stats PG captures;
2896/3016/TID oracle; remaining P6-P12 fallback/layout/visibility/progress/method
proof; all prior focused golden/isolation regressions and full terminal gates.
No broad successor execution was performed or authorized here. Older GIN byte/C,
single-batch limitations, measured count_returned gap, serial failed-build batch
writes and simultaneous immediate-error ordering remain disclosed. No source,
TOAST-subsystem, performance or footprint acceptance.
