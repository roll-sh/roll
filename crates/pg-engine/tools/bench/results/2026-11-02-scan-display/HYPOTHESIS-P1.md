# Prospective paired confirmation P1

The original full-spread rule failed in all three completed experiments. Those
outcomes remain unaccepted. The 60-second run showed common downward drift in
both binaries, so one final experiment will compare adjacent pairs. This is a
new prospective test, not a reclassification of the previous runs.

The hypothesis is that the scalar scan-display candidate improves the checked
ordered range workload by at least 2% against the frozen engine baseline, while
point lookups do not regress by more than 2%. This does not compare either binary
with PostgreSQL.

Use twelve alternating pairs, six in each order, with a discarded warmup pair.
Each invocation lasts 30 seconds. Each pair includes the 100-row ordered range
with eight clients and point lookups with one client. Keep the checked client,
independent fixture copies, settings, server CPUs 28–31 and client CPUs 32–35.
Both servers inherit the same parent cgroup; there are no separate memory caps
or exclusive physical cores. Record sibling CPU activity and frequencies before
warmup through cleanup. Those observations describe interference and cannot
invalidate an unfavorable result.

Freeze the rebuilt candidate, all four source files, both binary hashes, the
controller and statistics script before launch. The stronger tests added since
the first candidate build must be included. Their focused run passes all five
required scan shapes and both fallback assertions.

The primary observations are the twelve log ratios `ln(candidate / baseline)`
of range throughput. Accept the performance result only when all requirements
hold:

1. The exact one-sided Wilcoxon signed-rank p-value is at most 0.01.
2. The Hodges–Lehmann estimate, computed from the Walsh averages of those log
   ratios, is at least `ln(1.02)`.
3. The median paired range CPU-per-operation ratio is below one. The server is
   CPU saturated, so this is a consistency check, not independent confirmation.
4. The median paired point-throughput ratio is at least 0.98.
5. All operations are checked, errors and nonzero exits are zero, plans and
   counters match before timing, and fixture contents remain unchanged.

The signed-rank calculation assumes independent paired differences with a
symmetric distribution. The exact enumeration is conditional on those
assumptions; it does not remove shared-host interference or establish a
population-wide performance guarantee.

This is the last performance attempt for this candidate. A failed rule means
revert. Host noise is not a reason to rerun. Only a harness failure producing zero
samples allows one relaunch.

A passing performance result still requires the full source checks and a matched
regression or user-EXPLAIN comparison covering intermediate planner choices.
The work will pay that validation cost before accepting the source change.
Compatibility, idle resources, recovery, operating cost and distribution remain
open product requirements.
