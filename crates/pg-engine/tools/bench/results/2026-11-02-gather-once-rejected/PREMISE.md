# Diagnose the repeated fallback regression

Rejected premise: eliminating duplicate serial describe is sufficient to remove
the material fallback slowdown introduced by the first proof candidate.

The first candidate failed fallback throughput at about -5%. The integrated
candidate's retained-fixture tests directly show the same serial describe,
Planner::rel and partial_scans counts as the baseline for zero, positive and
extended-statistics refusals. Nevertheless its prospective zero fallback gate
failed again: paired range ratio 0.9481805, point 0.9492649. Thus duplicate serial
costing is not a sufficient causal explanation. It may have contributed to the
first candidate, but no profile established its share.

No third production fix will be written on that premise. First test whether
binary-arm/harness/host effects alone can create a comparable fallback failure.
Run a diagnostic A/A with the identical retained baseline SHA on both arms,
unchanged fixture, zero setup/tuple cost and parallel size minimums, same CPUs,
nice19 and alternating five15s pairs plus warmup for range8 and point1. No real
Gather side control is needed in A/A. Commit the A/A script/input hashes before
launch. Treat paired ratios outside [0.97,1/0.97] as evidence that a 3% fallback
comparison is unstable in that observation. Otherwise this one A/A provides no
evidence for an arm effect; it does not prove stability. No retry for noise.

This is a baseline harness diagnostic, never acceptance evidence for either
rejected candidate. Both rejections remain final. A/A cannot locate candidate
CPU overhead. Separate comparative stack sampling, or an isolated same-binary
baseline/original branch experiment, is needed before attributing it to probe
checks, code layout, skipped work, or any other code change.
