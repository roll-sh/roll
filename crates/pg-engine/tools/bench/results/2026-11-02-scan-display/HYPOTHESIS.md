# Scalar scan-condition display experiment

Expected claim, only after verification: omitting display-only scalar scan
conditions while estimating plans improves checked warm 200,000-row ordered
range throughput and reduces server CPU per operation against the frozen
`5cfc7904` engine binary. No PostgreSQL superiority or removable-CPU-share claim
follows from the saved ancestry profiles.

Keep the existing cost formulas, result extraction, ordering expressions, titles,
targets, SubPlan registration, and parallel decisions. Initially suppress only
`planned_scan` text for Index Cond, Recheck Cond, and Filter. Preserve empty
`Detail::Text` placeholders and removed-row details. Accept only a conservative
scalar expression subset with actual registry identity for built-in calls.
Subqueries, outer references, grouping, aggregate/window/SRF placeholders,
composite/domain/XML expressions, and unsupported variants use existing rendering.
User-visible EXPLAIN keeps rendering all conditions.

Before acceptance, compare complete estimator trees with only those eligible
condition strings normalized, exercise fallback cases and parallel decisions,
and run SQL-visible regression checks. Use the unchanged checked wire client,
identical Cargo target selection and dependencies, independent copies of the
same stopped fixture, fixed server and client CPU sets, and five alternating
measured pairs plus warmup. Keep a range8 gain only when it exceeds observed
full spreads and lowers CPU/op, with no measured point1 regression. Preserve
negative results. Run remaining release gates before accepting source changes.

After a candidate passes, compare it to a freshly checked, tuned PostgreSQL
reference under matched durability and resource limits. Report workload-specific
medians, ranges, CPU/op, memory, errors, and the observed limiter. The broader
compatibility, idle, recovery, cost, and distribution goals remain open.
