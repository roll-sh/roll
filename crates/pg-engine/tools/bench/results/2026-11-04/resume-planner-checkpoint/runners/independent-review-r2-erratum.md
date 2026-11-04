R2 is an immutable static review of c8618834 vs ab3f64b8: no HIGH/MEDIUM. It ran no gates.

Its statement that v3/focused checks had not run describes the review snapshot. The completed current-source focused stages now each exited0 with expected summaries: lifecycle2, prepared4, statistics1, end-monitoring1. Full v3 debug is running; 455/0/18/61 remains a frozen expected map, not a gate pass. Earlier ab3 complete gates and focused wire101 remain historical evidence only.

Guarded removal is fixed: lookup/Arc::ptr_eq/removal are atomic under the Shared mutex. A separate older LOW remains: a same-PID registration can displace another still-live session slot, delaying its idle counts until flush/end. Normal wire live PIDs are unique. No wire overlap or general same-PID lifecycle correctness claim. No source expansion/probe requested or performed for this residual.

Fast-own tests assert xid0, blocking_pids, an ungranted waiter lock, commit wakeup and cleanup. Their printed pg_locks rows provide raw type/mode observations, but assertions do not establish type/mode/fastpath parity. The pg_locks.fastpath LOW remains open. Snapshot case SELECT1 holds no table lock.

Other retained LOWs: unmeasured flush_idle contention/early-exit frequency, noted_due retry edge, two Txns sharing a backend hypothesis; SearchPath invalidation/temp/activity coverage limits remain disclosed. No new probes, source changes, services or benchmarks while c861 v3 is pinned.
