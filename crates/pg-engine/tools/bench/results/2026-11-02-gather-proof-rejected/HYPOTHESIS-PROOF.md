# Current-context no-Gather proof: prospective experiment

Status: prospective contract; proof-frame.json records the lock time and exact
source/tool hashes before launch. This replaces the unselected sharing draft.
Prior rejected scan-display trials do not count. No result is accepted yet.

Claim to test: the frozen proof candidate improves the checked 200,000-row
range_scan_100 workload at eight clients in automatic plan mode by at least 2%
against the verified engine baseline. It must preserve the one-client point-read
control and avoid a material regression when the proof declines.

Primary: twelve alternating BC/CB pairs of 20 seconds per workload/side, plus
one discarded warmup pair. Each side runs range8 and point1. Both release
servers use independently verified copies of the same stopped fixture, cache
2048 MiB, server CPUs 28-31, client CPUs 32-35 and matching settings. Host and
sibling CPU/frequency observation starts before warmup. Shared cores remain a
limit; host noise does not permit a rerun.
The heavy-job wrapper runs both servers and the client at niceness 19, recorded
per server. Normal-priority work can preempt them. The phase deadline starts
after acquiring the heavy slot; queue time cannot consume its measurement budget.

Performance gate: exact one-sided paired signed-rank test on log range throughput
ratios p <= 0.01; Hodges-Lehmann log effect >= ln(1.02); median paired server
CPU/op ratio < 1; median paired point throughput ratio >= 0.98. Under saturation
CPU/op and throughput are the same result, not independent confirmation. Require
median paired peak-RSS ratio <= 1.05 and p99-latency ratio <= 1.10 in both primary
workloads; these are regression guards, not claims of lower memory or latency.
For each range sample record client CPU as a fraction of its four CPUs. Require
that fraction < 0.8 for every measured sample and median server usage >= 3 cores
to support the server CPU interpretation; otherwise the performance gate fails.

Declined-proof control: five alternating pairs of 15 seconds plus one discarded
warmup pair, range8 and point1. Both servers use setup=0, tuple=0 and minimum
parallel scan sizes=0. The small ordered range is known to decline the proof
but still have empty Gather discovery. This isolates the extra serial describe
cost on a failed proof. Require median paired range and point throughput ratios
>= 0.97, peak-RSS ratio <= 1.05 and p99-latency ratio <= 1.10. No statistical
superiority claim from this control.

Actual-Gather control: in the declined-proof phase, first create matching
4000-row integral control tables, ANALYZE, and verify matching JSON EXPLAIN
contains a Gather. Run SELECT id,balance over the complete table through binary
extended protocol, one client, five alternating 10-second pairs plus one
discarded warmup pair. Check every row's identity, uniqueness and value. Require
median paired server CPU/query ratio <= 1.05 and wall latency/query ratio <=
1.05. Record client CPU; a client limit precludes a throughput superiority claim.
These samples are separate from primary/declined-range results. Drop the table
before fixture-digest validation. The tool and thresholds freeze with the frame.

Before timed measurements, run the same release preflight with pg_stat_statements
preloaded and planning tracked. Compare the two query rows' calls, returned rows
and zero worker counters; check AccessShareLock during an explicit read transaction,
release after rollback, and no remaining worker registrations. This correctness
preflight executes no timed benchmark and supplies no performance samples.

Every invocation must have zero errors, positive work, checked_ops=ops; all
fixture digests must match before/after; plans, settings and prepared counters
must match across arms; binary/source/dependency identity must be retained.
No instrumentation in the measured release. Freeze all source and launched
tools/controllers before launch; record the launch first. One attempt; only a
zero-sample harness failure permits one relaunch. A nonzero-sample failure
rejects the candidate. The tools must not change during the trial.

Source acceptance additionally requires concrete independent review, fresh
appropriate Cargo build/test/Clippy, relevant Postgres service checks, regression
comparison and worker/lock/counter correctness. Any pre-existing gate failure
must be identified with evidence, not counted as a pass. Timing acceptance is
conditional on these source checks. No full PostgreSQL superiority, compatibility,
recovery, idle-efficiency or distribution claim follows from this experiment.
