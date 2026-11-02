# PostgreSQL replacement acceptance

The product goal is unchanged PostgreSQL applications, lower operating cost,
nearly zero idle compute, and reliable distribution. This goal is **not verified**.
This document separates requirements from evidence. Passing a suite is evidence
for its tested versions and cases, not a universal compatibility percentage.

The initial scope includes 230 upstream regression files, 122 isolation specs,
34 contrib modules, driver and application suites, dump/restore, replication,
recovery, active workloads, and idle lifecycle measurements. Reliable multi-host
ownership and failover require additional architecture and fault tests.

## Release requirements

| Area | Required observable behavior | Current evidence and open work |
| --- | --- | --- |
| Queries and protocol | Matching results, types, errors, notices, transaction state, cancellation, authentication, TLS, catalogs, and prepared statements on unchanged application tests. | Release source `5b32046d` matches 51,148/51,150 upstream units. One EXPLAIN expression and one internal-counter difference remain. Both the direct engine and proxy currently decline TLS negotiation. This is not full compatibility. |
| Concurrency | Matching locking, isolation, deadlock, timeout, rollback, sequence, and crash behavior. | Isolation status records 121/122 specs but its named full-run prose refers to an older 120/122 run. Reconcile provenance and rerun before acceptance. |
| Existing applications | Zero engine-only failures for pinned driver/ORM suites and real application migrations, including schema changes and connection pools. | Fresh node-postgres 8.16.0 results match across 149 IDs, with 146 passes and three shared failures. Prisma 7.10.1 matches across 47 passing migration, introspection, and client checks. These direct-server plaintext tests use the frozen verified baseline. Full ActiveRecord coverage remains open. |
| Extensions and operations | Existing extension and backup/replication workflows work, or the endpoint uses upstream PostgreSQL to provide them. | Arbitrary PostgreSQL C extensions and physical PostgreSQL WAL/base-backup workflows are not supported by the custom engine. These are replacement blockers, not resolved by the SQL regression score. |
| Active efficiency | Checked work at matching durability and resource limits. A claimed gain must reproduce with CPU, memory, storage, I/O, and generator utilization recorded. | PostgreSQL is about 4.3 times faster on the published warm 200k-row range workload. The accepted histogram optimization improves the engine, not its standing against all PostgreSQL workloads. |
| Idle resources | Measure disconnected, connected-idle, released, and suspended states separately. Preserve session state and wake correctly. | The lifecycle probe has a short baseline diagnostic and five measured connected-idle windows. Removing repeated connection timers lowers idle CPU, but the candidate was reverted after a repeated active-workload regression. Engine block I/O is unavailable in the measured cgroup. Whole-stack idle cost and suspension remain unverified. Open pools prevent control-plane suspension. |
| Durability and recovery | Recover acknowledged writes within the promised failure model, with matching data and a measured recovery time. | Fresh full Docker verification records 12 passes and four recovery failures with compute-to-host WAL HTTP timeouts. Point-in-time branching, restore-in-place, retention, and local-disk-loss recovery fail. This gate remains open; clean-suspension tests also do not establish abrupt-host-loss safety. |
| Distribution | Explicit placement, one writer, fencing, bounded recovery, replication lag, and fault behavior under node/network/storage loss. | Local Docker placement and PostgreSQL replicas exist. Cross-node ownership, automatic fenced failover, sharding, and distributed atomic commit are not established. |
| Cost | Report an explicit workload, host/storage prices, utilization, retained storage, object requests, replication traffic, and shared-service allocation. | Pricing constants alone do not establish measured cost per tenant or successful operation. |

## Idle acceptance

The first provisional CPU target is below 1,000 microseconds per second per
settled idle compute, or 0.1% of one core, with no unexplained recurring writes.
This is an engineering target, not a measured result. Report accounting resolution,
absolute memory, per-connection increments, and voluntary context switches next
to it. Counts from surviving threads are a lower bound when threads change.
The direct-server probe excludes the proxy, control plane, Docker forwarding,
and kernel work charged outside each server's cgroup. It does not measure
suspended tenant compute or certify long-period background work.

Suspended tenant compute must have no running compute process and therefore no
tenant compute CPU or resident memory. Durable storage and the proxy, control
plane, storage receiver, and object store still cost resources. Allocate those
costs explicitly. A running shared engine with unloaded databases is a different
state from suspension and must be measured separately.

Do not disconnect pools or discard session settings, temporary objects, prepared
statements, transactions, locks, LISTEN state, or advisory locks to pass an idle
gate. Transparent suspension with live sessions requires a separate correctness
contract and tests.

## Distribution acceptance

Define whether a feature provides tenant placement, read replicas, failover, or
distributed SQL before implementing it. These are separate capabilities.
Prepared transactions on one host do not establish distributed atomic commit.

Fault tests must record acknowledged writes, recovered writes, divergence,
recovery time, and whether two nodes can accept writes simultaneously. Include
abrupt compute/host loss, network partitions, receiver interruption, and object
store outage. Clean suspension before deleting local data is a useful recovery
test but does not establish abrupt-host-loss safety. Local spool acknowledgement
and delayed object upload do not establish zero-loss host failure.

## Work sequence and gates

1. Reconcile the evidence above and measure missing idle lifecycle states using
   frozen binaries. Keep this baseline before changing idle scheduling.
2. Remove repeated connection trim timeouts after their one-shot work completes.
   Keep actual session deadlines, fragmented protocol reads, cancellation,
   shutdown, memory release, and wake/reuse behavior. Measure scheduling and CPU
   before and after. Reject an unverified resource claim.
   The first implementation, `735f44ac`, was reverted at `b6711d98`: the longer
   active control records lower range throughput and higher CPU per operation.
   Preserve that result before selecting another implementation.
3. Reproduce known compatibility gaps with pinned references and repair them in
   independent batches. Select a real migration test as well as upstream suites.
4. Measure whole-stack suspension and recovery with existing PostgreSQL computes.
   Preserve the current recovery-test waiver as an open gap, not a passing gate.
5. Design multi-host ownership and fencing before adding failover or distributed
   execution. Verify the state machine and run partition/crash tests.
6. Repeat active efficiency comparisons with identical Cargo targets, dependency
   fingerprints, durability, fixtures, limits, and checked operations. Improve
   measured bottlenecks without turning off externally required behavior.

Each accepted source batch needs its focused checks, release build, crate tests,
Clippy, PostgreSQL units, and relevant matched regression or application checks.
Keep failures, waivers, source identities, binary hashes, and negative trials.
An inconclusive experiment is not a pass. Do not report the overall goal complete
while any required area above remains unverified.

## Evidence

- [Rejected idle trial and fresh migration checks](tools/bench/results/2026-11-02-product-idle/README.md).
- [Current engine handoff](HANDOFF.md).
- [Measured active workloads](BENCH.md).
- [Regression status](tests/regress/STATUS.md).
- [Isolation status](tests/isolation/STATUS.md).
- [Driver and application status](tests/clients/STATUS.md).
- [Contrib status](tests/contrib/STATUS.md).
- [Dump/restore status](tests/pg_dump/STATUS.md).
- [Logical replication runner](tools/bench/repl/README.md).
- [Control-plane recovery tests](../pg-control/tests/e2e.rs).
