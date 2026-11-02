# Rejected scalar scan-display experiment

The final prospective confirmation failed its recorded performance rule. The
candidate is rejected and its four source changes are restored to `0cb59031`.
This experiment establishes no retained performance improvement and makes no
engine-versus-PostgreSQL speed claim. Compatibility, idle resources, recovery,
operating cost and distributed operation remain open product requirements.

The candidate skips rendering demonstrably pure scalar Index Cond, Recheck Cond
and Filter text in internal estimate builders. It retains labels, counters and
all rendering that may register subplans or change builder state. User EXPLAIN
builders still render conditions. Frozen source is retained under `source-p1/`.

## Final prospective confirmation

The [P1 hypothesis](HYPOTHESIS-P1.md) was locked before launch. Twelve
counterbalanced pairs of 30-second invocations cover the checked ordered
100-row range with eight clients and point lookups with one client. A separate
warmup pair is discarded from statistics. The exact one-sided signed-rank test
on paired range log-throughput ratios gives **p = 0.101806640625**, failing the
required **p <= 0.01**. The Hodges–Lehmann gain estimate is 3.72%, above the
2% minimum, but all requirements must pass. There is no noise-based retry.

| P1 range workload | Baseline | Candidate |
| --- | --- | --- |
| Throughput median, ops/s | 7,916.5 | 8,039.5 |
| Throughput min–max, ops/s | 7,022–8,755 | 7,117–9,089 |
| Server CPU median, microseconds/op | 493.94 | 488.12 |

The unpaired median throughput difference is +1.55%; it is a different statistic
from the paired Hodges–Lehmann estimate. Nine of twelve range pairs favor the
candidate. The median paired range CPU ratio is 0.9623 and the median paired
point-throughput ratio is 1.0905. Neither rescues the failed primary test.
The point results vary widely and do not establish a general point-read gain.

All 52 invocations, including warmups, finish with zero reported errors and
**10,878,578 checked operations**. Every operation validates returned values,
types, row count and ordering. Matching plans, prepared-plan counter trajectories,
settings and fixture digests are checked outside timing. Independent fixture
copies match the stopped source manifest. The source fixture remains unchanged.

The range workload uses about 3.91–3.92 server cores of four assigned logical
CPUs; client usage stays below one core. Recovered planning call chains in the
earlier [CPU investigation](../2026-11-02-planning-cpu/README.md) motivated this
experiment. This does not isolate this patch's source-level CPU contribution.
CPU/op and throughput under server saturation are related measurements, rather
than independent evidence of a gain.

## Earlier trials remain unaccepted

The original rule required the median gain to exceed the largest full observed
spread. All completed trials failed that rule. P1 does not reclassify them.

| Trial | Seconds/invocation | Measured pairs | Range median baseline → candidate, ops/s | Gap / largest spread, ops/s |
| --- | --- | --- | --- | --- |
| r1 | 10 | 5 | 7,931 → 8,327 | 396 / 921 |
| r3 | 20 | 5 | 8,358 → 9,142 | 784 / 1,063 |
| r4 | 60 | 5 | 8,684 → 9,015 | 331 / 498 |

r2 fails its bind preflight and produces zero observations. r1 overlaps unrelated
Bun activity. r4 shows common downward drift and shared-core interference;
its host observation covers later pairs only. Independent review recommended
either reverting or one last prospective paired test, with revert on failure.
That recommendation led to P1, whose outcome is now final for this candidate.
Counts across the four completed experiments total 112 invocations and
22,594,716 checked operations, with zero reported errors. This sum is accounting,
not a combined statistical test. r4 has no point control.

## Identity, resources and scope

The frozen baseline is `5cfc7904…e51ffd1`, built from `5b32046d`. Its runtime
engine source is identical to `0cb59031`. The first candidate is
`e76ca598…3af9cfe`; P1 uses the final rebuild `3a6742e6…dd0236e`. Both candidate
builds have identical allocated ELF sections, addresses and sizes except the
build-ID section; the later changes concern tests and debug metadata. All 46
dependency fingerprint entries match the baseline. Source, tool and runtime
binary identities are retained in the manifests.

Server affinity is CPUs 28–31 and client affinity is 32–35. The servers share the
same parent cgroup and use matching 2,048 MiB engine cache settings. They have
no separate memory caps, CPU quotas or exclusive physical cores. Other work on
sibling CPUs and changing frequencies can affect each side. P1 records CPU and
frequency observations from before warmup through cleanup; they describe the
environment and cannot invalidate an unfavorable result. No memory, idle,
durability, proxy, `rolld`, cost or distribution acceptance follows from this
direct-engine read-only workload. `rolld` currently launches official PostgreSQL.

## Correctness and restoration

The candidate release builds pass. Focused run r10 passes two tests with bitwise
estimate comparisons, explicit Parallel Seq Scan, Parallel Index Scan, Index Only
Scan, Bitmap Heap Scan and partition Append assertions, plus semantic fallback
checks. Earlier failed focused runs remain in the export. Independent source
review finds no production correctness blocker; its full-regression proposal
still needs direct before/after engine-output comparison to cover existing
differences. The prepared `run_candidate_regress.py` was never executed.

Because P1 failed, the conditional full build/test/Clippy/PostgreSQL controller
skips candidate source gates. No full candidate regression runs. Focused tests
are bounded evidence, not a substitute for those gates. The restoration record
checks every candidate file against its frozen hash before restoring the two
tracked files and removing the two owned new files. Restored runtime source has
an empty diff against the previously verified baseline. The matching release
rebuild's result and binary identity are recorded in `restored-build.json`.
Prior baseline tests and regression results are reused, not rerun in this batch.
After the first evidence seal, fresh workspace `cargo clippy` passes on restored
source with 11 existing engine-library warnings. Its command status and full log
are included; this does not validate the rejected candidate.
The prior [product checkpoint](../2026-11-02-product-idle/README.md) retains the
failed recovery gate; this experiment does not resolve it.

All measured engines record clean exit zero and all helper containers are
removed. Frozen binaries and databases remain in the local cache and are
excluded from this export. Restored source leaves no candidate runtime change.
The final live cleanup record confirms P1's two PIDs and helper absent and both
ports available. Its first audit attempt failed a case-sensitive Docker error-text
assertion; a later decision row explicitly corrects the prematurely logged check.

## Portable audit

Run `python3 /path/to/2026-11-02-scan-display/verify.py` from any directory.
The standard-library audit checks complete manifest coverage, recomputes all
trial summaries and P1 statistics from raw invocation records, reconciles checked
operations, and validates captured source, identity, restoration and cleanup
records. It starts no services or workloads and cannot rehash omitted binaries
or databases. A passing audit confirms these records, not product acceptance.
Decision-trail checks establish required entries and ordering, not the historical
truth of every row. Review r1 independently recomputes outcomes and restoration;
it did not inspect the summarized transcript. Its claim that the main thread is
Claude is incorrect: the main thread uses GPT-6.1-sol and the reviewer uses
Claude Opus 5.5. The original review and explicit response are retained.
Absolute paths describe capture provenance. Other scripts describe the original
local environment and are not portable workload runners.
