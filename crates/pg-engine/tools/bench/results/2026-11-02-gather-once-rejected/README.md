# Integrated Gather pruning was rejected

The candidate did not pass its predeclared fallback guard. Its source was
reverted, and the retained baseline executable was restored. There is no
accepted performance gain from this experiment.

| Locked phase | Paired range throughput | Paired point throughput | Result |
| --- | ---: | ---: | --- |
| Primary, 12 pairs of 20 seconds | +8.57% median, +8.62% Hodges–Lehmann | +15.92% | Passed primary only |
| Zero setup fallback, 5 pairs of 15 seconds | -5.18% | -5.07% | Failed the required -3% floor |
| Positive setup fallback | Not run | Not run | Skipped after rejection |
| Extended-statistics refusal | Not run | Not run | Skipped after rejection |

Primary exact one-sided signed-rank p was 1/4096. Memory, tail latency, and
server-limiter guards passed. All 76 timed invocations had zero errors and
10,654,921 checked operations. The separate Gather control checked 3,314 queries
and 13,256,000 rows. Its median CPU/query ratio was 1.04226 and wall/query ratio
was 1.00057, within its 1.05 limits. All five CPU ratios exceeded one.
The client limited this control, so wall time adds little. This is a bounded control. EXPLAIN shows a
Gather, but this control does not independently observe launched workers.

`frame.json` sealed 637 inputs before launch, including all seven changed source
files. Candidate 8563c53a and baseline 5cfc7904 matched the compiler, release
profile, features, and 46 dependency fingerprints. Both arms shared CPUs 28–31,
clients used CPUs 32–35, and all inherited nice 19. These cores were not exclusive.
Host observations are descriptive. Under server saturation, CPU/op repeats the
throughput result and is not independent corroboration.

The focused final library run passed 174 tests with 2 ignored. All-target Clippy
passed with existing warnings and none in the new proof. The unsupported builtin
ALTER FUNCTION test failed and was removed because that catalog function cannot
be altered through this engine. Its production hazard check was retained.
Earlier failed test logs and the initial zero-test run are preserved. The full
workspace, PostgreSQL service, and full candidate regression acceptance gates
were skipped after rejection. These checks are not green.

The seven frozen source hashes were checked before restoring five tracked files
and removing two owned new files. The first executable restore was an atomic copy
of the retained baseline, not a rebuild. Only convenience example executables and dependency entrypoints were initially
quarantined. Hashed executables and the candidate library remained in Cargo
artifacts until the subsequent matched baseline rebuild replaced them.
`restored-build.json` records that rebuild and executable hashes. Trial cleanup verified six PIDs,
three helper IDs, and six ports, then removed six private fixture copies. The
original fixture remained unchanged.

`PREMISE.md` rejects duplicate serial costing as a sufficient explanation of the
fallback slowdown. The integrated candidate matched baseline describe, relation, and partial-path
call counts in the test-build retained-fixture diagnostic. These counters were
not present in the release binary that was timed. A separately
frozen identical-baseline A/A checked 2,476,096 operations in 24 invocations.
Descriptive median arm ratios were 1.01527 for range and 1.02166 for point.
The analyzer was written after the data, and the frozen rule did not specify
median aggregation. Three individual pairs exceeded the 3% band. The claim that
A/A passed a prospective calibration was withdrawn in
`premise-aa/analysis-qualification.json`. A/A also omitted the earlier Gather
workload from the zero control. It cannot establish host stability, rule out a
state interaction, or revise either rejected candidate's decision.

`profile-fallback/` contains a later diagnostic summary, capture metadata, client
logs, and classification scripts. The twelve profiled invocations checked
2,521,001 operations. Their 27,578 decoded samples show similar broad work
fractions on both binaries, with no demonstrated cause for the fallback loss.
Inclusive ancestry overlaps, inlining hides frames, and snapshots can truncate.
Raw perf files, complete classified stacks, and the original binaries remain in
the local cache and are not part of this portable export. No sampled weight is
reported as removable CPU or phase time per operation. Frequency reanalysis uses
approximate raw-log completion mtimes and cannot correct or rescue a result.

`verify.py` rehashes the export, recomputes the saved performance decision and
accounting, and checks saved source/build identity, preflight parity, and cleanup
records. It starts no workloads or services. It cannot recheck live processes or
original binaries, decode omitted raw perf data, or prove the source theorem.
`decisions.tsv` is a contiguous canonical trail slice beginning at the integrated
design decision. The independent review states its own transcript coverage.

This checkpoint makes no PostgreSQL superiority, complete compatibility,
durability, recovery, distribution, idle-cost, or product-acceptance claim. The
broader efficiency task remains open.
