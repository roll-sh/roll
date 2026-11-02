The histogram-only source is 5b32046d. The five-pair range comparison improves
8-client throughput from 8,419 to 8,829 ops/s and server CPU from 469 to 446 us/op. PostgreSQL
still has a substantial OLTP lead. Finish final gates/evidence/publication before
starting a second engine implementation.

The next supported source investigation is repeated estimate reconstruction:
session::plan_cost_estimate calls sql::explain::estimate after a custom plan was
already built. Its scan builder calls paths::describe_need, which constructs a new
Planner, rebuilds Rel and index clauses, and calls index_path_ob. Earlier Path/IPath
objects contain costs/rows, but paths::apply preserves access details instead.
Prove whether selected-path estimates equal the final estimator's whole costs,
including projection, sort, parallel adjustments and planning charge, before
retaining or reusing them. Keep invalidation/locking/error boundaries explicit.

Do not assume Analyzed::plan substitutes parameter values. It clones the saved plan
and performs deferred planning. replannable establishes size-change reuse, not
parameter-change equivalence. bound_custom currently binds with param_consts=true;
operator binding folds $1+99 before histogram estimates. Post-analysis substitution
without equivalent refolding changes those estimates. CASE pruning/errors, LIMIT
pushdown, domain/unknown parameters and subqueries are additional barriers.

Concrete source clone sites for ordered ranges include BETWEEN expansion,
SELECT conjunct/projection assembly, Planner::rel, owned index clauses, btree
cost boundary vectors and final IndexQual construction. Profiles show activity,
not complete caller attribution. Do not blame ProjectNode from flat samples.

Portal/catalog borrows were rejected under the conservative spread gate despite a
small consistent eight-client point-read signal. The unsafe stored_tid shortcut remains rejected:
an old-snapshot tuple header need not identify the current model block that fetched
must visit. Preserve locator/horizon ordering, pruning/LP_DEAD behavior and guards.

Keep frozen same-client value checks, five alternating pairs, CPU on both sides,
full fixture checksums and actual runtime/visibility snapshots. Reuse heavy.sh,
the one target directory and owned fixtures. Do not repeat waived Docker recovery.
