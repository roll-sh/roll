# R2 requested lock coverage

Read-only review at HEAD caabeb196e268f5a0fb822e30565e633344058d1; no worktree/index/HEAD changes, no build/test launches.
Live tests/twophase.rs SHA256 f18eaee4f8e6c224b41c291d89990b9574009e4066381203886afaf937871f3c.

Existing targets:
- tests/twophase.rs::prepared_finish_preserves_reused_backend_lock_owners covers COMMIT and ROLLBACK PREPARED while original session remains connected and has started a second transaction. Its strong waiter waits on the second transaction (pg_blocking_pids {1}), and wakes on that transaction COMMIT. It does NOT wait on prepared owner {0} or disconnect preparer.
- tests/twophase.rs::prepared_transactions_survive_a_restart drops preparing sessions with prepared transactions outstanding, restarts engine, checks recovered row/relation/unique locks block NOWAIT/timeouts, and COMMIT/ROLLBACK PREPARED releases them. It does NOT keep an existing waiter asleep until prepared finish, or finish after disconnect without restart.
- tests/twophase.rs::prepared_role_changes_finish_after_a_restart also disconnects and restarts before finish; catalog behavior only.
- tests/compact_locks.rs::compact_locks_prepare_and_finish checks prepared row/key NOWAIT/timeouts then COMMIT PREPARED and retries; no blocked-waiter wake or disconnect.

Missing exact combination is proposed in r2-tests.patch, generated from a cached copy, NOT applied. git apply --check succeeded against live pinned tree.

Proposed target: tests/twophase.rs::prepared_waiters_wake_after_finish_and_disconnect.
Eight cheap cases: COMMIT/ROLLBACK PREPARED x preparer-connected/disconnected x relation/row waiter. Prepare UPDATE t.v=10. Optional drop preparer (no restart). Existing waiter either ACCESS EXCLUSIVE LOCK t (transferred relation hold) or UPDATE same row (prepared own transaction lock). Observer polls pg_blocking_pids(4) to exactly {0}, proving prepared dummy backend blocks. Finisher commits/rolls back; waiter must wake and complete without timeout. After join expect blocking_pids empty, no prepared registry entries, no pid-NULL relation/transactionid/virtualxid holds, row result matching commit/rollback (+100 for row waiter), concurrent index succeeds and AccessExclusive NOWAIT succeeds.

Poll budget3s, waiter lock_timeout5s, CIC statement_timeout2s. All engine instances/worker sessions scoped to each case. The proposal has not compiled or executed; parent must apply ONLY after current pinned job is terminal, then run through existing heavy wrapper:

cargo test -p pg-engine --test twophase prepared_waiters_wake_after_finish_and_disconnect -- --nocapture

No source acceptance claim follows from the patch proposal.
