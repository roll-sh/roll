# Deferred fetch visibility setup

The checked 100-row range workload gains about **5.3% throughput** and uses
**4.4% less server CPU per query** in two independently required blocks, including
one with reversed slots. Point queries and the dirty-heap control stay near
baseline. Full source acceptance remains false: the fresh PostgreSQL service
suite reproduces four existing recovery preflight failures. Final disposition is
**retained under the existing recovery waiver**, with recovery unverified;
this is not an accepted source batch or product acceptance. The exact disposition
is recorded in [final-decision.json](final-decision.json).

The production change defers `db.xids()` and `vis_for()` until after `fetched`
has checked for a missing or entirely all-visible heap under its existing model
read lock. Those cases need no visibility horizon. The dirty path still reads a
fresh horizon and repeats its lookup under the write lock when pruning is needed.
Existing callers already construct visibility state under model locks. The
skipped catalog access can optionally fill a detached-view cache; this change
does not skip a required visibility mutation. No cache or planner change is added.

## Prospective experiment

The [hypothesis](HYPOTHESIS.md), tools and 698 input hashes were sealed before
the single launch. Its original draft wording is preserved. Each all-visible
block has 12 alternating pairs of 20-second measurements, plus a discarded
warmup pair, for the ordered 100-row range at eight clients and point lookup at
one client. The second block reverses copy/start/port and runner target order.
Both must independently pass; results are neither pooled nor selected.

| Range workload | Primary baseline | Primary candidate | Confirmation baseline | Confirmation candidate |
| --- | ---: | ---: | ---: | ---: |
| Median throughput, queries/s | 8,845 | 9,310 | 8,846.5 | 9,328 |
| Throughput min–max, queries/s | 8,780–8,871 | 9,247–9,353 | 8,791–8,896 | 9,265–9,339 |
| Median server CPU, microseconds/query | 446.63 | 427.06 | 446.23 | 427.31 |

The paired Hodges–Lehmann throughput estimates are **+5.3054%** and **+5.2917%**.
All 12 range pairs favor the candidate in each block; the exact one-sided
signed-rank p value is **1/4096** for each. Median paired CPU ratios are
0.95575 and 0.95607; point-throughput ratios are 1.00368 and 1.00362.
These paired statistics differ from ratios of the unpaired medians above.
The frozen evaluator uses rounded client throughput; recalculating from operations
and elapsed time gives +5.303% and +5.295%, with the same gate outcome.

The five-pair, 15-second dirty control has zero all-visible pages before and
after timing. Its median throughput ratios are **1.00321 range** and
**1.00610 point**, inside the frozen two-sided [0.97, 1.02] band. The setup digest
scan prunes pages before timing, so this primarily checks the slow lookup path;
it is not a write-lock pruning performance test. Both primary blocks and the
control pass the declared memory, p99 and generator-utilization guards.

Across 128 invocations, including warmups, **19,178,971 operations are fully value
checked with zero reported errors**. Matching plans, prepared-plan counters,
settings and logical fixture digests are checked outside timing. Plans use
ordinary Index Scan; all-visible heaps skip visibility setup for fetched rows.
Untimed instrumented queries also check lock lifetime and zero parallel workers.

## Identity and measurement limits

The candidate is source `a39915dc+7b0b5ae9` (HEAD plus the full two-file diff hash),
binary `c49d2387…12ef18b`. Baseline binary `5cfc7904…e51ffd1` comes from
`5b32046d`; its production engine tree equals HEAD before this patch. The exact
release command, toolchain, profile, features and all 46 dependency fingerprint
entries match. The unchanged checked client is `9f5b0222…b8e28`.

Both servers use CPUs 28–31 and the client 32–35, nice 19, and a 2,048 MiB cache
setting. Range queries use about 3.95–3.98 server cores; the client uses about
0.83–0.87 cores. The server is the limiter. CPU/query and throughput under this
saturation are coupled measurements. There are no exclusive physical cores,
separate memory caps or per-engine CPU quotas. Host CPU/frequency observations
are retained; swapping slots does not remove binary-layout effects or host drift.

Separate earlier baseline profiles put `fetched` ancestry near 4.72% of range
CPU and 0.22% of point CPU. These overlapping, truncated samples provide a scale,
not a strict removable-work bound or exact attribution of the gain. This is a
coupled result: primary server use increases about 0.7% while CPU/query falls
4.4%, consistent with roughly 5.3% more queries per second. Less mutex waiting
is a possible explanation, not established by a lock profile. This is a
warm, read-only 200k-row direct-engine comparison. It establishes no PostgreSQL
superiority, general workload gain, memory savings, idle cost or durability gain.

## Correctness and the failed service gate

The full engine crate run passes 431 tests with 18 ignored. It compiled the
earlier wire test; a fresh full wire run passes all 68 tests after both sessions
are forced to and checked for Index Scan. The new test checks updates, visibility
map transitions and a pinned repeatable-read snapshot. It is a behavior guard,
not differential evidence that the old implementation was incorrect.

Workspace build, remaining workspace tests, workspace Clippy and engine
all-target Clippy pass. All-target Clippy retains exactly the baseline's 16
warning occurrences, with matching messages and source paths. Existing formatter
differences remain; neither changed file adds a formatter complaint.

The fresh full release regression covers 230 files and 51,150 units: **51,148
match**, with **zero new differences** against the retained full release baseline.
The known join EXPLAIN and partition-prune result differences remain. The
baseline-comparison section and TSV are independently checked because the
underlying harness exits zero even when differences exist.

Fresh `pnpm test:pg --force` exits **101**. Service units and the actual-daemon
lifecycle (`serves_requests_and_adopts_compute_after_restart`) pass; the
control-plane suite has **12 passes and four failures**:
point-in-time branching, restore-in-place, retention and local-disk-loss recovery.
Each times out requesting WAL from inside the compute instead of getting HTTP
403 for an invalid token, exactly as in the retained service baseline. The custom
engine is separate from `rolld`'s official PostgreSQL computes. Recovery remains
unverified and the aggregate source-gate result stays failed. The previously
documented recovery waiver is preserved explicitly, never counted as a pass.
The [qualification](source-gate-qualification.json) checks names and failure
signatures against the archived baseline. No firewall changes were made.

## Portable audit and cleanup

Run `python3 -I /path/to/2026-11-02-fetch-visibility/verify.py` from any directory.
The audit checks the complete file inventory, recomputes all three statistical
blocks and accounting totals, validates identities and diagnostic records, and
keeps failed service acceptance visible. It rehashes 693 frozen inputs from the
compressed archive. Five frozen binary paths, original fixture bytes, and the
full raw regression outputs are local-only and cannot be rehashed offline. Source
snapshots, the exact patch, raw invocation logs/JSON, compressed host observations,
full regression report/hunks/TSV, baseline evidence and reviews are exported.

`qualify_source_gates.py`, `cleanup_measurements.py`, `cleanup_source_gates.py`,
`verify.py` and `export_checkpoint.py` were written **after the frame** for
reporting, cleanup and offline audit. They are not frozen prospective gates;
[post-frame-artifacts.json](post-frame-artifacts.json) identifies them. The
historical `source-r1.patch` is superseded: it contains the weaker test. The
trial patch is `candidate-source/tracked.patch`, SHA `7b0b5ae9…`.
The raw logs, TSV empty final fields, reports and patches retain their original
whitespace, including Git whitespace warnings; the production/test delta passes
the source-only whitespace check.

Live cleanup records confirm measurement and regression processes absent, owned
helpers removed and ports free. Eight timing fixture copies plus the private
regression data and executable are removed after identity/absence checks. The
stopped original fixture is unchanged. Historical stopped profiling/reference
containers are left intact. The shared worktree target remains about **66 GB**;
no additional target directory was created.
The regression teardown logs `kill: (3555680): No such process` after the engine
has exited; the live cleanup record independently confirms that PID absent.

The broader PostgreSQL replacement goal remains open: recovery, TLS and extension
compatibility, application coverage, idle resources, operating cost and multi-host
ownership/fencing are not established by this change.

## Attention

Reviewed by Anthropic Claude Opus 5.5 in two final rounds. R1 independently
recomputes the samples and checks source, identities and waiver; R2 checks the
portable export and the relevant parent transcript. Neither finds a blocker to
committing this bounded change under the explicit waiver. The failed recovery
gate and 66 GB target remain open. See [r1](final-review-r1.json),
[responses](final-review-response.json) and [r2](final-review-r2.json) for exact
coverage and limits. The final review/receipt/trail additions are sealed after
R2's snapshot; the verifier is rerun on the final staged files.
