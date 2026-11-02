# Rejected idle timer trial and baseline migration checks

The idle timer candidate reduces settled connection polling, but fails the active
workload control. Candidate `735f44ac` was reverted at `b6711d98`. Production source
again matches the previously verified histogram baseline. This batch does not
establish full PostgreSQL compatibility, lower operating cost, or distribution.

## Idle benefit and active regression

The candidate disarms the connection's two-second trim timer after its one-shot
hook completes. Positive socket reads rearm trimming, while actual session
deadlines remain active. The release build and three focused tests pass.
A CancelRequest sent after three seconds idle preserves simple and binary
extended-query reuse. Administrative termination, three SQL-visible timeouts,
transaction rollback, and a fragmented simple query
produce matching transcripts on the baseline, candidate, and PostgreSQL 18.6.
A deliberately corrupted fixture is rejected on every endpoint.

The connected-idle experiment holds 64 checked sessions per endpoint. One warmup
precedes five alternating ten-second observations. Each observation follows five
seconds of settling. Servers have matching 512 MiB memory limits, no swap,
four-core quotas, and CPU affinity 28–31. The client runs on CPUs 32–35.
Each engine uses a 64 MiB cache, a 36 MiB plan cache, and a five-second idle-close
setting. The raw records retain the PostgreSQL configuration and all SQL settings.

| Connected-idle metric | Baseline | Candidate |
| --- | --- | --- |
| Cgroup CPU, time-weighted microseconds per second | 636.47 | 69.51 |
| Connection-thread voluntary switches per second, median | 31.95 | 0 observed |
| Total surviving-thread voluntary switches per second, median | 33.75 | 1.80 |
| Cgroup memory.current, median MiB | 39.46 | 38.42 |

All measured windows have stable thread identities, zero swap, and no increases
in memory pressure or CPU throttling counters. All 2,502 fixture queries return
checked types, values, and ordering. The connection-switch result counts voluntary
context switches on surviving threads, not all scheduler wakeups. Zero observed
switches or rounded CPU counters do not establish zero actual resource use.
The trim and database-close threads still wake periodically.

PostgreSQL's CPU varies from 15.38 to 1,559.55 microseconds per second in these
short windows. Its time-weighted CPU is 918.71 microseconds per second. This
background variation prevents a general engine-versus-PostgreSQL idle ranking.
The engine's cgroup lacks block I/O accounting; missing I/O is retained as null.
No zero-write claim follows. These direct-server observations exclude the proxy,
control plane, forwarding processes, suspended tenants, and long-period work.

The active control uses the frozen checked client and independent byte-verified
clones of the stopped 200,000-row fixture. Settings, plans, prepared-statement
counter sequences, and before/after SQL digests agree. CPU affinity remains
28–31 for servers and 32–35 for clients. Active runs use matching engine settings
and process CPU/RSS accounting, rather than the idle run's cgroup memory caps.

| Range workload, 8 clients | Baseline | Candidate |
| --- | --- | --- |
| Initial ten-second throughput, median and range, ops/s | 8,817 (8,809–8,852) | 8,745 (8,692–8,792) |
| Confirmation twenty-second throughput, ops/s | 8,872 (8,854–8,901) | 8,788 (8,772–8,808) |
| Confirmation server CPU, median microseconds/op | 445.08 | 450.28 |
| Confirmation sampled process RSS, median and range, MiB | 112.0 (111.5–112.9) | 112.5 (111.8–112.7) |

Each experiment has one warmup pair and five alternating measured pairs.
The first median gap is smaller than the candidate's full spread, so a fresh-start
confirmation uses longer windows. The confirmation records 0.947% lower median
throughput and 1.168% higher server CPU per operation. Its median gap of 84 ops/s
exceeds both full throughput spreads, whose maximum is 47 ops/s. The ranges do
not overlap. Both sides use about 3.95 server cores, consistent with saturation
of the four assigned CPUs. This identifies a server CPU limit, not a specific
function responsible for the difference.

The initial one-client point control overlaps: baseline 6,823 (6,745–6,981),
candidate 6,777 (6,737–6,816) ops/s. No point equivalence claim follows.
All 36 active invocations, including warmups, complete with zero reported errors
and 3,990,166 value-checked operations. Keep the two experiments separate when
reporting performance. Their combined operation count is accounting only.

Matching Cargo targets and dependency fingerprints avoid the previous
experiment's target-selection mismatch. These binary comparisons still do not
isolate source-level CPU attribution or establish a universal regression.
The [predeclared hypothesis](HYPOTHESIS.md) requires preserving active efficiency.
The idle improvement is insufficient to accept this implementation.

## Unchanged driver and migration checks

After the revert, the frozen verified baseline runs the repository's existing
suites against a fresh PostgreSQL 18.6 reference. Test source and adapters remain
unchanged. The runtime engine hash is `5cfc7904…e51ffd1`, the same binary used by
the earlier accepted histogram measurements. These are direct plaintext
connections. They do not test a deployed proxy, TLS, or an arbitrary application.

- node-postgres 8.16.0 matches across 149 recorded IDs: 146 passes, three shared
  failures, and zero engine-only failures. IDs combine integration files and
  cursor/stream test cases. The SSL cases run without a TLS-enabled endpoint;
  issue 130 also lacks `psql` in the client image. Shared failures are uncovered
  behavior, not compatibility passes. The runner retains failure excerpts and
  per-ID outcomes, not every successful file's complete stdout.
- Prisma 7.10.1 matches across 47 passing recorded checks. These cover two schema
  migrations, reset, status, empty schema diffs, introspection blocks, generated
  client operations, relations, transactions, and errors. They are the repository
  CLI/client checks, not Prisma's full upstream engine suite or a full user app.

The export retains suite and lockfile hashes, image identities, commands,
per-target results, raw XML/list files, Prisma logs and migrations, and cleanup.
The direct engine and proxy both currently decline TLS negotiation. Full external
compatibility therefore remains open even when these plaintext outcomes match.

## Verification and cleanup

The candidate's full crate, Clippy, PostgreSQL, and matched regression gates were
not run after the active rejection. Do not attribute baseline verification to it.
Restored-source verification is recorded separately under `restored-gates/`.
The matched restored release build passes and produces the exact verified baseline
hash. Fresh full release crate tests pass with 430 passed and 18 ignored. All-target
Clippy passes with existing warnings. `pnpm test:pg --force` passes its build,
Clippy, and 24 unit tests, but its full Docker gate exits 101 with 12 passes and
four failures:

- `point_in_time_branches`
- `restore_in_place_keeps_the_endpoint`
- `retention_removes_old_history`
- `survives_losing_the_local_disk`

These failures show WAL HTTP requests to the host timing out, followed by missing
checkpoint records during recovery. The command uses `ROLL_E2E_BIND=0.0.0.0` and
`ROLL_E2E_THREADS=2`. The underlying network cause is not established by this run.
Recovery acceptance remains open. The evidence audit checks these failed outcomes;
a successful audit does not turn the PostgreSQL gate green.
No fresh full regression starts after restoration. The pinned baseline regression
report is reused because production source and executable bytes are identical.

The idle run, both active controls, and the migration run record clean engine
exits and removed helper containers. A final live check confirms all eight recorded
engine/e2e PIDs and all four helper/reference container IDs absent. No containers
remain under this e2e run's exact home prefix, and all five measured ports refuse
connections. Database evidence remains retained. Earlier preflight failures remain:
SQL readiness initially followed a Docker forwarding listener too early, and a
second attempt required unavailable engine `io.stat`. Neither produced samples.
The first initialization container required forced stop. A third attempt records
648 fixture queries in a short diagnostic across disconnected, connected, and
released states. It is not an acceptance run or a suspended-tenant measurement.

## Portable audit

Run `python3 verify.py` from any directory with Python 3.9 or newer. The audit is
read-only and uses the standard library. It verifies manifest bytes, recomputes
resource and throughput summaries, checks every active invocation's value-check
accounting, compares migration outcomes, and checks recorded build/cleanup facts.
It does not rerun services, SQL suites, source gates, or the omitted executables.

Original binaries and database files remain in the local cache and are omitted.
Absolute paths in records preserve capture provenance. The exported command
scripts describe the historical local environment; they are not portable runners.
`run-regress-restored.sh` was prepared but cancelled before startup. Earlier failed
preflight script versions were not frozen separately; their commands and output
remain recorded.
The original baseline fixture's complete manifests are retained as records,
without exporting its data files. Manifest verification certifies export bytes,
not the historical truth of every recorded fact.
