Cache-only response to static R1 a6fe3e4e vs c8618834. NOT IMPLEMENTED, COMPILED OR EXECUTED.

Both MEDIUMs move the first-read context from parent timing to Parse timing. Source acceptance stays held regardless of the pinned full runner's outcome. Full v1 is now terminal FAILED: debug101 (partial305/2/9/33), later stages NOTRUN; raw exportfdcb8f36 remote-verified. No production repair has been applied.

Merely moving planned_total() to plan_cost()/start() is insufficient: it still reads ScanEst recorded under the Parse-time GUCs/table size/visibility. The minimum correction must use current estimates at the first existing cache read, not just current parallel settings around an old total.

Recommended narrow shape after terminal and bounded red proof:

1. Remove eager BoundQuery::costed() calls/method so Parse leaves cost/gathers OnceLocks empty, matching parent c861 timing. Do not add settings generation or table-size keys to p.bound.
2. Retain the restrictive simple scan / column-only Project eligibility, exclusions, and current parallel/setup/debug guards in a SQL paths helper. Obtain total from existing paths::describe(ctx, scan)?.total under the CALLER's context rather than ScanEst. describe -> describe_with -> describe_need already recomputes chosen-access seq/index costs from current GUCs, sizes and visibility; reuse it rather than adding a fourth cost formula. Prove this matches EXPLAIN for the supported shapes with the existing debug comparison, performed at the lazy call.
3. In Session::plan_cost's cache-miss branch, try that helper now. If Some(total), set the existing cost/gathers caches with total/empty under this first-choice context; otherwise preserve total_and_gathers/of_explain exactly. Preserve planning_charge/check_cost_estimate and Arc ownership keys. Do not touch unrelated litplan/SearchPath passing.
4. Leave exec::parallel_part.rs byte-identical to c861/a6fe. Restore parent lazy Gather discovery unchanged by removing Parse-time prefill; execution before plan_cost follows existing may_gather/full EXPLAIN. Only Session::plan_cost existing first cache miss can use the guarded live describe shortcut and set empty gathers there. Forego one-shot Gather optimization; no executor hook, cross-owner edit, or new cache generation.
5. Retire the now-unused ScanEst field/writes/initializers and the duplicate seq shortcut formula instead of leaving misleading stale metadata. This removes the latent foreign-cost drift; supported helper still rejects foreign tables. Preserve all newer rel.fdw/COPY/scan_need hooks using scoped edits, not old whole files. Original a6fe/ec5 source commits remain backed up.

This trades stored chosen costs for a live scan-only cost lookup while still avoiding EXPLAIN's full builder for eligible shapes. It is a correctness proposal, not a measured optimization. If live describe equivalence cannot be established cheaply, fall back to the unchanged parent EXPLAIN path; do not broaden planner/cache architecture in this batch.

Prepared fixtures (NOT RUN):

- 171-parse-bind-parallel.sql: same plain Scan with Parse under max_parallel_workers_per_gather=0, then Bind/Execute under2; repeat Parse under huge parallel_setup_cost then RESET before Bind. Capture EXPLAIN and actual parallel_workers_to_launch/_launched deltas, force stats flush before reads. Return one selected row, not 3000. Table3M is inherited from the review's static example, not a tuned benchmark.
- 171-parse-bind-generic.sql: 100 analyzed rows/no index; protocol Parse under seq_page_cost500, RESET, six binds of the SAME named prepared statement. Capture generic/custom counts (static expected parent/PG1/5, candidate0/6). Table growth/visibility are related unexecuted cases, not established runtime effects.

Use actual extended Parse/Sync then a separate SET/RESET command then Bind/Execute/Sync. SQL PREPARE is not an equivalent substitute. PostgreSQL control can use psql18's parse/bind_named commands. Engine reproduction should call Session::parse_message; sync; query(SET/RESET); bind_message; execute_portal; sync, or real wire Prepare followed by SET then repeated named binds. Do not reparse inside the six-bind loop. A real Engine/cluster is needed for actual worker-launch counters; common::engine_session's stub cluster is insufficient for proving launched workers.

After completed terminal/export, coordinate actual reference identity/source pins, immutable red outputs against a6fe and c861, narrow correction, focused green, independent review and fresh exact-source gates. Existing a171-pg inspect shows cpuset4-7 owned by parent oracle lane: do not execute/reconfigure until coordinated. Fixtures remain unexecuted. No timing or new service. Existing customANY/IN mismatches remain separate pre-timing gaps.
