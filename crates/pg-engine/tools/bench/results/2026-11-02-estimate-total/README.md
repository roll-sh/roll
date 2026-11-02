# Rejected total-only estimate experiment

Candidate `1713aa40` passes its SQL-driven bitwise cost test and live plan/result
checks, but fails the predeclared performance gate. Both production changes were
reverted at `e8425dea` and `7cdbee00`. The accepted histogram engine remains unchanged.

The measured candidate binary has a small negative range signal. A build-control
mismatch discovered during restoration prevents attributing that signal to the
source changes: the candidate selected only `--bin roll-engine`, while the
frozen baseline also selected `--example bench --example scan_bench`. Cargo
dependency fingerprints differ for `thiserror`, `pg_query`, `tokio`, `serde`, and
`clap`. Rebuilding reverted source with the candidate command also produced a
different binary hash. The source-only performance effect remains unresolved.
The gate did not establish a gain, so the source changes remain reverted.
[bin-only-restored-build.json](bin-only-restored-build.json) preserves this finding.

The candidate shared needed-column masks in `estimate_builder` and added an
`estimate_total` entry point for session costing. It retained the same builder,
parallel settlement, InitPlan charges, and custom planning charge. It omitted the
unused width/output-column and other estimate-result extraction.

## Five alternating measured pairs

The same frozen checked client runs on independent clones of the same stopped
200,000-row fixture. One warmup pair precedes five alternating ten-second pairs.
Server CPUs are 28–31; client CPUs are 32–35. The workload uses named prepared
statements with binary INT4 parameters and results.

| Workload and metric | Histogram baseline | Candidate |
| --- | --- | --- |
| Range, 8 clients, operations/s | 8,871 (8,859–8,888) | 8,820 (8,741–8,842) |
| Range server CPU, µs/op | 445.22 (444.35–445.59) | 447.22 (446.64–451.81) |
| Range process RSS, MiB | 112.9 (112.4–114.1) | 110.3 (110.1–111.1) |
| Point, 1 client, operations/s | 6,885 (6,674–6,912) | 6,830 (6,744–6,858) |
| Point server CPU, µs/op | 109.94 (109.56–114.03) | 111.23 (109.20–112.55) |
| Point process RSS, MiB | 110.7 (110.2–111.4) | 109.4 (109.2–109.5) |

Values are medians and full measured ranges. Range throughput has a small negative
signal of 0.575%, with higher CPU per operation in this binary comparison.
The median gap of 51 operations/s is below the candidate's full spread of 101; the throughput ranges do not overlap.
This fails the required gain threshold. Point throughput ranges overlap.
These observations do not establish zero benefit from the omitted work in every
context. No memory-efficiency claim is made from these process RSS samples.

The baseline uses about 3.95 server cores at the measured range throughput and CPU
per operation, consistent with its four-core budget. No profiler is active in
these timings. The separate CPU-ancestry diagnostic motivates investigating the
planning paths; it does not predict this small extraction change's effect.

All 24 invocations, including warmup, complete with zero errors and 1,879,306 checked
operations. Every point field and every ordered range ID/balance pair is checked.
Complete fixture digests match before and after both endpoints. Settings, verbose
JSON plans, exact extended-protocol results and twenty-execution plan-count
trajectories agree. Auto range remains custom for all twenty executions; point
switches after five. [HYPOTHESIS.md](HYPOTHESIS.md) preserves the acceptance rule.

## Verification and restoration

The focused release test compares estimator totals bit for bit through real bound
SQL plans covering ranges, points, projections, sorting, grouping, subqueries,
InitPlans, system scans and parallel settings. It passes, as does the release build.
The test's call-order checks use warmed state and do not establish independent cold
initialization. Its Gather assertion does not specifically prove a parallel-aware
partial path. Source review found no correctness blocker.

Only that focused test and the release build ran for the candidate. Full candidate
gates and regression were not run after the performance gate failed. Prior published
baseline verification must not be attributed to this candidate. The restored source
matches `c1a1d2d2` and the measured histogram source `5b32046d`. Rebuilding with
the baseline Cargo target selection succeeds and produces byte-identical engine
SHA-256 `5cfc7904…e51ffd1`, as recorded in
[restored-build.json](restored-build.json). The existing Docker recovery e2e
waiver continues to apply.

Candidate SHA-256 is `81e690c510d8e46cbdb5106d8dad58820b3542cb57cf0552e30b815b26b8ffde`.
Baseline SHA-256 is `5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1`.
Checked client SHA-256 is `9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28`.
Runtime records check the actual executable identities and all-thread affinity.
The candidate build records source cleanliness, compiler version and selected build
environment. Its selected Cargo targets differ from the baseline, as described
above. These records do not establish a fully reproducible toolchain or environment.

## Portable audit

Run the read-only checks with Python 3 on a POSIX host:

```sh
python3 /path/to/2026-11-02-estimate-total/verify.py
```

The audit hashes exports, parses every client/resource record, recomputes exact
summary statistics and the acceptance decision, compares preflight plans/settings,
and checks recorded fixture, build and cleanup facts. It regenerates the expected
fixture digest independently. Build exits, source/binary restoration and cleanup
remain recorded facts. It does not rerun services or source gates, rehash the
omitted fixture files, or inspect omitted executable bytes. Captured absolute paths
retain cache provenance.

The candidate binary and cloned fixture data remain in
`~/.cache/pg-engine-perf-20261102/estimate-total/` and are omitted from this export.
The baseline engine and frozen client remain at their recorded paths under
`~/.cache/pg-engine-perf-20261101/oltp-20261102/`.
All owned engine PIDs are absent, ports 53340–53341 refuse connections, and the exact
owned helper container has been removed. Saved data is retained.
