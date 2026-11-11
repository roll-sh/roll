# Rejected btree selectivity experiment

Candidate `8b33053b` reuses an integer btree boundary selectivity within one costing
call. It was reverted at `2d6375d1` because its small throughput signal did not clear
the conservative observed-spread threshold. This does not establish zero benefit.

Five alternating ten-second pairs, after a warmup pair, give baseline range8
throughput of 8,863 operations/s, with a range of 8,777 to 8,884. The candidate gives
8,992 operations/s, with a range of 8,861 to 9,016. The median gain is 1.46%, and
server CPU falls from 445.08 to 438.87 microseconds per operation. The throughput
gap is smaller than the candidate's full spread. A shorter five-pair comparison
shows a similar 1.56% signal.

The one-client point-read median drops 3.39% in the short run and 0.84% in the
longer control. Both gaps remain within their run spreads; longer-run server CPU
is slightly lower for the candidate. No stable point-read regression is established.
There is no memory claim. The timed range workload uses about 3.95 server CPU
seconds per second within the four assigned CPUs. Client CPU stays below one core.

Both comparisons use the same frozen checked client, identical cloned 200k-account
fixtures, server CPUs 28 through 31, and client CPUs 32 through 35. Settings and
EXPLAIN output match exactly. The named extended-protocol probe validates returned
values and finds identical prepared-plan counter trajectories. Every timed read
checks values, cardinality, and supported types. Secondary aggregate checks cannot
identify all contributing rows. All 84 invocations, including warmups, have zero
errors and matching checked/completed operation counts. Fixture digests match before
and after timing.

The candidate release build and one bitwise library test pass. That test constructs
planner inputs directly. The additional SQL-activation and implementation-identity
tests at isolated commit `c5d4854a` were not compiled, run, or integrated. This record
does not establish real-SQL fast-path activation or attribute the small signal to
the skipped estimator call. Full candidate gates were not run after the performance
acceptance gate failed. The restored production source is identical to previously
verified `90b5525f`, whose verification remains in the sibling `2026-11-02/` export.
The restored release build succeeds and produces an engine binary byte-identical
to the verified histogram baseline. `restored-build.json` records both hashes.

Run the portable read-only audit from any directory:

```bash
python3 /path/to/2026-11-02-btree-selectivity/verify.py
```

The audit checks captured-file hashes, result validation, resource counters, fixture
digests, recomputed summary statistics, recorded binary identities, candidate build
status, source-restoration records, and cleanup. It does not compile code, run
servers, or inspect omitted executable bytes. `SHA256SUMS` is an integrity index,
not a signed attestation.

The comparison directories contain every invocation's output and resource record.
`runtime.json` and `runtime-long.json` preserve their respective observations.
Absolute paths, PIDs, and cache-relative references describe the original machine.
The other scripts are historical commands; `verify.py` is the portable audit.
All experiment services are stopped and data is retained. Binaries and saved data
remain at `~/.cache/pg-engine-perf-20261102/btree-selectivity/` on the original machine.
