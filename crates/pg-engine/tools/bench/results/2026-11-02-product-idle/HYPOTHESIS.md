# Idle connection timeout experiment

Claim to test: after the one-shot connection trim fires, removing its repeated socket timer reduces voluntary context switches for settled idle client threads without changing SQL-visible behavior, memory reclamation, session deadlines, cancellation or shutdown. It does not establish whole-product near-zero cost or active throughput superiority.

First collect a short baseline/reference lifecycle diagnostic. It is not an acceptance run. The first startup attempt produced zero samples because TCP readiness reached docker-proxy before PostgreSQL startup; its owned engine exited zero and its initialization container required forced stop. Preserve that failure.

Candidate acceptance requires five alternating ten-second connected-idle windows plus warmup on frozen baseline, candidate and PostgreSQL 18.6, with 64 checked live sessions, matching 512 MiB memory/no-swap caps, four-core quotas, CPU affinity 28-31 and client 32-35. Both engine revisions must use identical Cargo target/feature selection. Preserve identities and all raw accounting. Report context switches only for stable-thread windows and as surviving-thread counts; they are not interchangeable with all scheduler wakeups. Require at least 90% lower connection-thread recurring voluntary switches after settling, no stable memory increase beyond observed variation, no unexplained CPU regression, and complete checked responses across repeated idle/resume cycles. A helper test pass alone is insufficient. CPU uses the cgroup, not process ticks.

Also run SQL-visible idle/session/transaction timeout, cancel, termination, fragmented input and replication-poll checks. Baseline full verification does not certify a candidate. Accepted source requires release build, full crate tests, Clippy, PG units and relevant matched regression. The existing Docker recovery e2e waiver persists and remains an open product gap.

Short observations cannot certify long-period checkpoints, autovacuum, compaction, object storage, proxy/control-plane overhead, all connection-pool behavior, replicas, or whole-product operating cost. Keep warm bursts, observation intervals and instrumentation separate. No zero-CPU or zero-cost claim follows from rounded counters or this experiment.

## Diagnostic preflight history

The second attempt failed before observations because the user cgroup lacks io.stat. Both owned servers exited zero and the PG container was removed. The third attempt treats unavailable I/O as null, checks CPU/memory caps and real per-thread affinity, and completed a diagnostic only: 648 fixture checks, no errors, clean shutdown. It used one measured five-second window per state after warmup and three-second settling, so it cannot certify long-period idle behavior or cross-phase memory efficiency. Final connected-only comparison will settle for five seconds before every observed window.
