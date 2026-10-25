# PostgreSQL isolation suite: status

All 122 specs of `src/test/isolation/isolation_schedule` (REL_18_6) run through the engine and
through the reference PostgreSQL 18.6, compared permutation by permutation and step by step,
with the harness in `crates/pg-engine/tests/pg_isolation.rs` (its header documents the method;
`REPORT.md` is the last run's table, `hunks/` its first differing permutation per spec).

```sh
# a run (about 5 minutes; the reference's outputs can be reused with PGISOLATION_REUSE_REFERENCE=1)
PGISOLATION_DIR=~/.cache/pgisolation PGISOLATION_DB=isolation_x PGISOLATION_REPORT_DIR=/tmp/iso_report \
  crates/pg-engine/tools/heavy.sh cargo test -p pg-engine --test pg_isolation -- --ignored --nocapture
```

The reference's output equals `expected/<spec>.out` (or a variant) for every spec, so the reference
run is a faithful isolationtester.

## Score

| | before (round 17 start) | now |
|---|---:|---:|
| specs matching | 30 of 122 (24.6%) | 88 of 122 (72.1%) |
| permutations matching | 921 of 1648 (55.9%) | 1378 of 1648 (83.6%) |
| steps matching | 10146 of 12238 (82.9%) | 11269 of 12238 (92.1%) |

A *step* is one `step ...` line of isolationtester's output with what follows it (its rows,
errors, notices, `<waiting ...>` and `<... completed>`); a permutation matches when all of its
output is identical. Before this round the engine had no `pg_isolation_test_session_is_blocked`
at all (isolationtester can't run without it: 0 specs); the 'before' column is the first run with
it implemented over the old lock manager.

## Remaining differences, classified

| class | specs | permutations differing |
|---|---|---:|
| D2 | reindex-concurrently-toast | 60 |
| D1 | detach-partition-concurrently-1, detach-partition-concurrently-2, detach-partition-concurrently-3, detach-partition-concurrently-4 | 51 |
| S1 | predicate-gist, predicate-gin | 37 |
| I1 | stats | 25 |
| L1 | ddl-dependency-locking, pub-concurrent-drop, drop-owned-grant | 19 |
| L3 | fk-partitioned-1, fk-partitioned-2, partition-concurrent-attach | 15 |
| E1 | eval-plan-qual, eval-plan-qual-trigger, merge-update, merge-match-recheck | 12 |
| T1 | inherit-temp, temp-schema-cleanup | 11 |
| E2 | partition-key-update-1, partition-key-update-2, partition-key-update-3 | 10 |
| L2 | intra-grant-inplace, intra-grant-inplace-db | 8 |
| E4 | insert-conflict-specconflict | 5 |
| T2 | subxid-overflow | 4 |
| I2 | horizons | 4 |
| E3 | fk-snapshot | 3 |
| L6 | reindex-concurrently, multiple-cic | 2 |
| L5 | alter-table-4 | 2 |
| P1 | multiple-row-versions | 1 |
| L4 | deadlock-parallel | 1 |

- **D1**: DETACH PARTITION ... CONCURRENTLY runs as one transaction: Postgres's two transactions (the partition marked detach-pending, a wait for every older locker of the parent, then the detach), the pending state other sessions plan around and FINALIZE after a cancel aren't there.
- **D2**: A TOAST table isn't a relation of its own a statement can name (`ALTER TABLE pg_toast.pg_toast_N RENAME`, `REINDEX TABLE pg_toast.x`): the spec's setup fails.
- **E1**: EvalPlanQual details: the TID condition rechecked against the new version's ctid, a catalog row (pg_class) updated by two transactions (row locks on catalog rows), and MERGE's rechecks of a row a concurrent MERGE / UPDATE changed.
- **E2**: Rows moved to another partition by a concurrent UPDATE: lockers now get Postgres's 'tuple to be locked was already moved' error; INSERT ... ON CONFLICT's wait for the moving transaction's new row in the other partition differs.
- **E3**: A REPEATABLE READ foreign key check on a partitioned referenced table uses the transaction snapshot where Postgres's RI check crosses into a fresh one (and fails).
- **E4**: Speculative insertion (INSERT ... ON CONFLICT) races: Postgres inserts the tuple first and checks again afterwards, waiting on the other inserter's speculative token; the engine checks once, under its key lock.
- **I1**: Cumulative statistics: when a backend's pending counts reach the shared stats (pgstat_report_stat's flush timing, stats_fetch_consistency snapshots) differs.
- **I2**: Pruning horizons (`GlobalVisState`): what an index-only / bitmap scan sees as dead after another session's snapshot is released.
- **L1**: Object locks of dependencies are taken now (recordDependencyOn's LockNotPinnedObject, AcquireDeletionLock, shared ones for roles), but what DROP OWNED / GRANT on a role and the publication functions lock in between differs (being worked on in this round).
- **L2**: GRANT's in-place catalog tuple lock (heap_inplace_lock) on the object's pg_class / pg_database row: a concurrent ALTER TABLE / VACUUM doesn't wait for it.
- **L3**: ATTACH PARTITION and inserts into a partitioned table: the locks ATTACH takes on the referencing side of a foreign key and on the default partition, and an insert's lock on the partition it routes to while another session attaches it.
- **L4**: A parallel worker's own advisory locks and the deadlock checker's queue reordering across a lock group: the engine runs the worker's part in the leader, so the order the waiters are woken in differs once.
- **L5**: A query waiting for an inheritance child's lock re-reads the child as it is once the lock is held (a dropped child is skipped, a child whose column type changed fails 'does not match parent's type'): the engine keeps the plan it made before the wait.
- **L6**: CONCURRENTLY index builds: WaitForOlderSnapshots (transactions holding a snapshot but no lock on the table) isn't modelled, and an IMMUTABLE function in an index predicate isn't folded when the index is opened (multiple-cic's advisory lock).
- **P1**: A 1,000,000-row table is too big for the heap model, which leaves its relpages 0: the planner picks a sequential scan where Postgres uses the primary key, and the scan's relation-level SIRead lock makes a conflict Postgres's tuple lock doesn't.
- **S1**: SSI over GiST and GIN indexes: their scans and insertions aren't modelled page by page, so a scan's SIRead lock is the whole index (a relation lock) and every insertion conflicts with it; Postgres's page locks let the permutations with disjoint pages through.
- **T1**: Other sessions' temporary objects: an inheritance parent's scan now skips other sessions' temp children; the cleanup of a terminated session's temp schema (and the notices it sends) differs.
- **T2**: A PL/pgSQL recursion creating thousands of subtransactions runs out of stack where Postgres doesn't.

## Per spec

| spec | permutations | steps |
|---|---:|---:|
| read-only-anomaly | 1/1 | 9/9 |
| read-only-anomaly-2 | 2/2 | 16/16 |
| read-only-anomaly-3 | 1/1 | 10/10 |
| read-write-unique | 2/2 | 13/13 |
| read-write-unique-2 | 2/2 | 13/13 |
| read-write-unique-3 | 1/1 | 5/5 |
| read-write-unique-4 | 3/3 | 19/19 |
| simple-write-skew | 6/6 | 24/24 |
| receipt-report | 210/210 | 1470/1470 |
| temporal-range-integrity | 20/20 | 120/120 |
| project-manager | 20/20 | 120/120 |
| classroom-scheduling | 20/20 | 120/120 |
| total-cash | 20/20 | 120/120 |
| referential-integrity | 35/35 | 245/245 |
| ri-trigger | 10/10 | 50/50 |
| partial-index | 20/20 | 120/120 |
| two-ids | 90/90 | 540/540 |
| multiple-row-versions ✗ | 0/1 | 10/11 |
| index-only-scan | 6/6 | 24/24 |
| index-only-bitmapscan | 1/1 | 9/9 |
| predicate-lock-hot-tuple | 1/1 | 8/8 |
| update-conflict-out | 2/2 | 16/16 |
| deadlock-simple | 1/1 | 7/7 |
| deadlock-hard | 1/1 | 32/32 |
| deadlock-soft | 1/1 | 14/14 |
| deadlock-soft-2 | 1/1 | 14/14 |
| deadlock-parallel ✗ | 0/1 | 12/14 |
| detach-partition-concurrently-1 ✗ | 0/13 | 57/105 |
| detach-partition-concurrently-2 ✗ | 1/5 | 23/32 |
| detach-partition-concurrently-3 ✗ | 0/18 | 55/204 |
| detach-partition-concurrently-4 ✗ | 4/20 | 106/183 |
| fk-contention | 3/3 | 9/9 |
| fk-deadlock | 14/14 | 90/90 |
| fk-deadlock2 | 11/11 | 72/72 |
| fk-partitioned-1 ✗ | 8/14 | 72/90 |
| fk-partitioned-2 ✗ | 0/6 | 24/42 |
| fk-snapshot ✗ | 4/7 | 51/59 |
| subxid-overflow ✗ | 0/4 | 25/31 |
| eval-plan-qual ✗ | 51/56 | 331/342 |
| eval-plan-qual-trigger ✗ | 37/39 | 501/503 |
| inplace-inval | 2/2 | 9/9 |
| intra-grant-inplace ✗ | 3/10 | 39/69 |
| intra-grant-inplace-db ✗ | 0/1 | 5/8 |
| lock-update-delete | 12/12 | 84/84 |
| lock-update-traversal | 3/3 | 26/26 |
| inherit-temp ✗ | 0/9 | 52/72 |
| temp-schema-cleanup ✗ | 0/2 | 6/9 |
| insert-conflict-do-nothing | 2/2 | 12/12 |
| insert-conflict-do-nothing-2 | 8/8 | 60/60 |
| insert-conflict-do-update | 2/2 | 12/12 |
| insert-conflict-do-update-2 | 2/2 | 12/12 |
| insert-conflict-do-update-3 | 1/1 | 6/6 |
| insert-conflict-do-update-4 | 3/3 | 21/21 |
| insert-conflict-specconflict ✗ | 0/5 | 65/97 |
| merge-insert-update | 8/8 | 46/46 |
| merge-delete | 19/19 | 100/100 |
| merge-update ✗ | 14/17 | 89/94 |
| merge-match-recheck ✗ | 22/24 | 148/150 |
| merge-join | 4/4 | 38/38 |
| delete-abort-savept | 7/7 | 54/54 |
| delete-abort-savept-2 | 4/4 | 31/31 |
| aborted-keyrevoke | 14/14 | 103/103 |
| multixact-no-deadlock | 1/1 | 9/9 |
| multixact-no-forget | 10/10 | 52/52 |
| lock-committed-update | 24/24 | 222/222 |
| lock-committed-keyupdate | 18/18 | 171/171 |
| update-locked-tuple | 6/6 | 42/42 |
| reindex-concurrently ✗ | 5/6 | 45/47 |
| reindex-concurrently-toast ✗ | 0/60 | 0/348 |
| reindex-schema | 2/2 | 12/12 |
| propagate-lock-delete | 8/8 | 80/80 |
| tuplelock-conflict | 32/32 | 164/164 |
| tuplelock-update | 1/1 | 17/17 |
| tuplelock-upgrade-no-deadlock | 9/9 | 90/90 |
| tuplelock-partition | 2/2 | 9/9 |
| freeze-the-dead | 1/1 | 14/14 |
| nowait | 6/6 | 24/24 |
| nowait-2 | 3/3 | 15/15 |
| nowait-3 | 1/1 | 7/7 |
| nowait-4 | 1/1 | 9/9 |
| nowait-5 | 1/1 | 7/7 |
| skip-locked | 20/20 | 120/120 |
| skip-locked-2 | 3/3 | 15/15 |
| skip-locked-3 | 1/1 | 7/7 |
| skip-locked-4 | 1/1 | 9/9 |
| drop-index-concurrently-1 | 1/1 | 16/16 |
| multiple-cic ✗ | 0/1 | 2/5 |
| alter-table-1 | 137/137 | 1407/1407 |
| alter-table-2 | 48/48 | 453/453 |
| alter-table-3 | 48/48 | 400/400 |
| alter-table-4 ✗ | 2/4 | 24/26 |
| create-trigger | 25/25 | 185/185 |
| sequence-ddl | 5/5 | 21/21 |
| async-notify | 6/6 | 35/35 |
| vacuum-no-cleanup-lock | 5/5 | 38/38 |
| timeouts | 8/8 | 32/32 |
| vacuum-concurrent-drop | 6/6 | 24/24 |
| vacuum-conflict | 16/16 | 108/108 |
| vacuum-skip-locked | 16/16 | 50/50 |
| stats ✗ | 31/56 | 573/604 |
| horizons ✗ | 1/5 | 44/51 |
| predicate-hash | 40/40 | 240/240 |
| predicate-gist ✗ | 22/40 | 222/240 |
| predicate-gin ✗ | 12/31 | 170/189 |
| partition-concurrent-attach ✗ | 0/3 | 12/24 |
| partition-drop-index-locking | 2/2 | 20/20 |
| partition-key-update-1 ✗ | 10/13 | 92/95 |
| partition-key-update-2 ✗ | 1/2 | 11/17 |
| partition-key-update-3 ✗ | 2/8 | 50/84 |
| partition-key-update-4 | 4/4 | 34/34 |
| plpgsql-toast | 7/7 | 31/31 |
| cluster-conflict | 2/2 | 14/14 |
| cluster-conflict-partition | 4/4 | 26/26 |
| truncate-conflict | 8/8 | 54/54 |
| serializable-parallel | 2/2 | 16/16 |
| serializable-parallel-2 | 1/1 | 5/5 |
| serializable-parallel-3 | 1/1 | 10/10 |
| matview-write-skew | 8/8 | 56/56 |
| lock-nowait | 1/1 | 6/6 |
| ddl-dependency-locking ✗ | 0/17 | 50/85 |
| pub-concurrent-drop ✗ | 0/1 | 2/4 |
| drop-owned-grant ✗ | 0/1 | 2/5 |

## What this round changed (round 17, agent 34)

Bugs the suite found, all of them invisible to the regression suite (single session):

- **No lock conflicts between statements and DDL.** Statements took no relation locks at all:
  an `ALTER TABLE`, `TRUNCATE`, `DROP`, `CLUSTER`, `VACUUM FULL`, `REINDEX`, `CREATE INDEX` ran
  under a reader's or writer's open transaction, and `LOCK TABLE` only recorded the lock for
  pg_locks. Now every statement locks the relations it names before it runs (AccessShare to
  read, RowShare under FOR UPDATE / SHARE, RowExclusive to change; inheritance members,
  partitions and indexes as the planner and executor take them), DDL takes Postgres's levels
  (`AlterTableGetLockLevel` and the rest), and a statement that waited for a lock binds again
  over the catalog other transactions committed meanwhile.
- **Only two lock modes, no queue, no deadlock search.** The lock manager had shared /
  exclusive and a polling waits-for check that reported the wrong DETAIL. It is now Postgres's
  lock manager (`txn/locks.rs`): the eight modes and conflict table, FIFO wait queues with
  JoinWaitQueue's rules, ProcLockWakeup, `deadlock_timeout`, DeadLockCheck with soft-deadlock
  queue reordering (deadlock.c ported), DeadLockReport's DETAIL (`Process N waits for
  ShareLock on transaction X; blocked by process M.`), fast-path flags, and pg_locks / 
  `pg_blocking_pids` / `pg_isolation_test_session_is_blocked` /
  `pg_safe_snapshot_blocking_pids` over it.
- **Row locks had two strengths.** FOR KEY SHARE conflicted with FOR NO KEY UPDATE, so foreign
  key checks blocked behind unrelated updates (and an UPDATE changing a key column didn't take
  FOR UPDATE). Rows are now locked in Postgres's four strengths by (sub)transaction, waiting for
  the holders' transactions (heap_lock_tuple's tuple lock then XactLockTableWait), released by
  ROLLBACK TO and at an error inside a block; a FOR KEY SHARE of a row whose key a concurrent
  update left alone keeps the version it saw.
- **SERIALIZABLE was REPEATABLE READ.** Write skew and every other SSI anomaly committed. Now
  `txn/ssi.rs` implements predicate.c: SIRead locks on relations, heap tuples and btree / hash
  index pages (with Postgres's lock promotion), rw-conflict detection on reads (conflict out)
  and writes (conflict in), dooming and the reason codes of
  OnConflict_CheckForSerializationFailure / PreCommit_CheckForSerializationFailure, read-only
  transactions' safe snapshots and DEFERRABLE's wait for one. All the SSI specs
  (simple-write-skew, receipt-report, two-ids, read-only-anomaly-*, read-write-unique-*, ...)
  match but those over GiST / GIN pages.
- **A REPEATABLE READ / SERIALIZABLE snapshot was taken at BEGIN** instead of at the first
  statement that needs one (`SET`, `LOCK`, `SHOW` don't): a transaction that began, waited and
  then read saw too little.
- **ON CONFLICT DO UPDATE / DO NOTHING under a concurrent change**: a READ COMMITTED upsert
  whose conflicting row changed while it waited now starts over (it updated a version it never
  saw); a snapshot transaction's DO NOTHING over a row its snapshot doesn't see fails.
- **Notices of a waiting statement** reached the client only when it finished; Postgres sends
  each one as it's raised. A statement now sends what it raised before it waits for a lock or
  sleeps (results of earlier statements of the query first).
- **Rows moved to another partition** by a concurrent UPDATE: a locker now gets "tuple to be
  locked was already moved to another partition due to concurrent update" (MERGE: "...to be
  merged...") instead of skipping the row silently.
- **Concurrent DDL** of two transactions on different objects failed the second commit with
  "concurrent schema change"; the second transaction's catalog changes are now merged onto the
  first's (`catalog/merge.rs`).
- Smaller: VACUUM / ANALYZE lock each relation in a transaction of its own (SKIP_LOCKED and
  "relation no longer exists" warnings); CONCURRENTLY index builds and drops wait for the table's
  lockers; `nextval` waits for `ALTER SEQUENCE`; heap order of a row another transaction is
  updating follows the version the reader sees; `pg_notification_queue_usage`; LOCK TABLE
  NOWAIT; a void built-in declared `RETURNS int` (LANGUAGE internal) returns 0, not NULL;
  `pg_get_publication_tables` is variadic and skips tables dropped while it waited.
