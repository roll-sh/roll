# Integrated Gather pruning: prospective trial

Draft until frame.json is sealed. No timed samples have been collected. The
previous no-Gather candidate failed its fallback gate and was reverted; none
of its gains count toward this candidate.

Test claim: on the checked 200,000-row accounts fixture, integrated pruning
improves automatic-plan range_scan_100 at eight clients by at least 2% versus
baseline5cfc7904, without material regressions on point reads or fallback cases.
This is an engine comparison, not superiority over PostgreSQL or rolld acceptance.

Use the same stopped fixture, frozen value-checking client, matching release
profile and dependencies, cache2048MiB, server CPUs28–31 and client CPUs32–35.
Both servers and clients inherit nice19 from heavy.sh. Host CPU/frequency and
sibling observations start before warmup, but cores are shared. Record every
runtime binary/source SHA. A queued heavy slot does not consume the phase budget.

Primary:12 counterbalanced pairs of20s per side and workload, plus one discarded
warmup pair. range8 and point1 are interleaved. Require the exact one-sided
signed-rank p on paired log range ratios <=0.01, Hodges–Lehmann gain >=2%, median
paired range CPU/op ratio<1, point throughput ratio>=0.98. CPU/op under server
saturation is the same result as throughput, not independent corroboration.

Three separate fallback phases follow only if preceding phases pass, each five
counterbalanced15s pairs plus a discarded warmup, range8 and point1:

- zero: setup0,tuple0,min parallel scan sizes0;
- positive: setup1,tuple0,min parallel scan sizes0;
- refused: ordinary primary cost settings, but CREATE STATISTICS
  gather_once_refusal ON id,balance FROM accounts, without collecting extended
  statistics. The metadata alone refuses pruning after a promising numeric gate.

The retained-fixture diagnostic checked all four phases in automatic, custom
and generic modes, including the exact range and full five-column point queries.
For each query and mode, primary proved with one serial describe/one Rel; all
fallback phases attempted then refused with exactly baseline describe/Rel/partial
counts. These are uninstrumented timing controls only after a new release freeze.

Every fallback range and point must have median paired throughput ratio>=0.97,
peak RSS<=1.05 and p99 latency<=1.10. No superiority test is claimed for controls.
Primary uses the same memory and tail guards. For all range samples require
client CPU<80% of its four CPUs and median server usage>=3 cores, otherwise the
server-performance interpretation fails. Account for every operation and error.

Actual Gather: in the zero phase, run the checked4000-row full Seq scan on both
arms, require matching EXPLAIN with an actual Gather, verify every returned row,
and run five20s pairs plus a warmup. Require median paired CPU/query and wall
latency/query ratios<=1.05. Twenty seconds improves tick resolution over the
previous10s control; client serialization can still limit it, so it supports
only the stated regression guard. These queries/rows are reported separately.

First run monitored release correctness with pg_stat_statements planning tracked:
matching calls/rows and planned/launched workers; AccessShareLock while a read
transaction is open, release on rollback, no worker backends after completion.
This phase runs no timed benchmark.

Zero errors, positive work and checked_ops==ops in every sample; fixture digests
before/after, prepared counters, plans and settings must match between arms.
Freeze complete source, scripts/controllers, external harness dependencies and
binaries before launch. One prospective attempt with no relaunch, including zero-sample failures.
Any harness/evaluation failure rejects this candidate. Host noise is not grounds
to retry. Do not change tools or gates after launch. Any failed gate
rejects the candidate; do not rescue it with a cache or a changed threshold.

Timing passing only permits full source acceptance checks: appropriate Cargo
build/test/Clippy, Postgres service checks, full regression comparison, and
independent concrete source review. Existing failures must be described with
evidence and cannot be called green. No compatibility, durability, idle-cost,
recovery, distribution or PostgreSQL superiority claim follows from this trial.
