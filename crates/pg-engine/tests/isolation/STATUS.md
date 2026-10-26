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

| | before (round 17 start) | round 18 start | now |
|---|---:|---:|---:|
| specs matching | 30 of 122 (24.6%) | 94 of 122 (77.0%) | 120 of 122 (98.4%) |
| permutations matching | 921 of 1648 (55.9%) | 1452 of 1648 (88.1%) | 1646 of 1648 (99.9%) |
| steps matching | 10146 of 12238 (82.9%) | 11397 of 12238 (93.1%) | 12232 of 12238 (100.0%) |

A *step* is one `step ...` line of isolationtester's output with what follows it (its rows,
errors, notices, `<waiting ...>` and `<... completed>`); a permutation matches when all of its
output is identical. Re-measured at 9407d0ee (round 20, agent 73; a full run, debug build,
private reference): the same 120 / 122 specs and 1,646 / 1,648 permutations, the same two
differing. Before this round the engine had no `pg_isolation_test_session_is_blocked`
at all (isolationtester can't run without it: 0 specs); the 'before' column is the first run with
it implemented over the old lock manager.

## Remaining differences, classified

| class | specs | permutations differing |
|---|---|---:|
| L2 | intra-grant-inplace | 1 |
| L4 | deadlock-parallel | 1 |

- **D1** (done, round 18, agent 56: all 56 permutations of detach-partition-concurrently-1..4
  match; see "DETACH PARTITION CONCURRENTLY" below).
- (D2, reindex-concurrently-toast, matches since round 18, agent 56: a table's TOAST table and
  index are relations a statement can name. `pg_toast.pg_toast_N[_index]` (or the names an
  `ALTER TABLE / INDEX ... RENAME` under allow_system_table_mods gave them, `Table::
  toast_relname`) resolve for regclass, ALTER ... RENAME (owner, then "permission denied:
  \"x\" is a system catalog" without allow_system_table_mods) and REINDEX TABLE / INDEX
  [CONCURRENTLY] (`Ddl::reindex_toast`: SHARE / ACCESS EXCLUSIVE or SHARE UPDATE EXCLUSIVE on
  the TOAST table and index, CONCURRENTLY's waits for the TOAST table's lockers, the name
  looked up again after the wait). Writing a value out of line, or deleting one
  (toast_save_datum / toast_delete_datum, as the size model lays the row out), takes
  RowExclusiveLock on the TOAST table until the transaction ends; DROP TABLE and TRUNCATE take
  AccessExclusiveLock on the TOAST table and index; REINDEX of a table locks its TOAST table
  and index too. Not modelled: detoasting's AccessShareLock on the TOAST table when a read
  fetches an out-of-line value, and deletes from tables too big for the size model.)
- (E1–E4, L1, L3, L5, L6, T1 and T2 match since round 18, agent 56: "What round 18 changed"
  below. I1, stats, and I2, horizons, too: see "Which differences are engine internals".)
- **L2**: intra-grant-inplace's last permutation deletes the table's pg_class row with
  `DELETE FROM pg_class` (a catalog corruption Postgres permits a superuser); the engine
  refuses DML deleting catalog rows ("permission denied for table pg_class"), so the
  waits on that row and REVOKE's "cache lookup failed" don't happen. Not planned.
- **L4**: A parallel worker's own advisory locks and the deadlock checker's queue reordering across a lock group: the engine runs the worker's part in the leader, so the order the waiters are woken in differs once.

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
| multiple-row-versions | 1/1 | 11/11 |
| index-only-scan | 6/6 | 24/24 |
| index-only-bitmapscan | 1/1 | 9/9 |
| predicate-lock-hot-tuple | 1/1 | 8/8 |
| update-conflict-out | 2/2 | 16/16 |
| deadlock-simple | 1/1 | 7/7 |
| deadlock-hard | 1/1 | 32/32 |
| deadlock-soft | 1/1 | 14/14 |
| deadlock-soft-2 | 1/1 | 14/14 |
| deadlock-parallel ✗ | 0/1 | 12/14 |
| detach-partition-concurrently-1 | 13/13 | 105/105 |
| detach-partition-concurrently-2 | 5/5 | 32/32 |
| detach-partition-concurrently-3 | 18/18 | 204/204 |
| detach-partition-concurrently-4 | 20/20 | 183/183 |
| fk-contention | 3/3 | 9/9 |
| fk-deadlock | 14/14 | 90/90 |
| fk-deadlock2 | 11/11 | 72/72 |
| fk-partitioned-1 | 14/14 | 90/90 |
| fk-partitioned-2 | 6/6 | 42/42 |
| fk-snapshot | 7/7 | 59/59 |
| subxid-overflow | 4/4 | 31/31 |
| eval-plan-qual | 56/56 | 342/342 |
| eval-plan-qual-trigger | 39/39 | 503/503 |
| inplace-inval | 2/2 | 9/9 |
| intra-grant-inplace ✗ | 9/10 | 65/69 |
| intra-grant-inplace-db | 1/1 | 8/8 |
| lock-update-delete | 12/12 | 84/84 |
| lock-update-traversal | 3/3 | 26/26 |
| inherit-temp | 9/9 | 72/72 |
| temp-schema-cleanup | 2/2 | 9/9 |
| insert-conflict-do-nothing | 2/2 | 12/12 |
| insert-conflict-do-nothing-2 | 8/8 | 60/60 |
| insert-conflict-do-update | 2/2 | 12/12 |
| insert-conflict-do-update-2 | 2/2 | 12/12 |
| insert-conflict-do-update-3 | 1/1 | 6/6 |
| insert-conflict-do-update-4 | 3/3 | 21/21 |
| insert-conflict-specconflict | 5/5 | 97/97 |
| merge-insert-update | 8/8 | 46/46 |
| merge-delete | 19/19 | 100/100 |
| merge-update | 17/17 | 94/94 |
| merge-match-recheck | 24/24 | 150/150 |
| merge-join | 4/4 | 38/38 |
| delete-abort-savept | 7/7 | 54/54 |
| delete-abort-savept-2 | 4/4 | 31/31 |
| aborted-keyrevoke | 14/14 | 103/103 |
| multixact-no-deadlock | 1/1 | 9/9 |
| multixact-no-forget | 10/10 | 52/52 |
| lock-committed-update | 24/24 | 222/222 |
| lock-committed-keyupdate | 18/18 | 171/171 |
| update-locked-tuple | 6/6 | 42/42 |
| reindex-concurrently | 6/6 | 47/47 |
| reindex-concurrently-toast | 60/60 | 348/348 |
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
| multiple-cic | 1/1 | 5/5 |
| alter-table-1 | 137/137 | 1407/1407 |
| alter-table-2 | 48/48 | 453/453 |
| alter-table-3 | 48/48 | 400/400 |
| alter-table-4 | 4/4 | 26/26 |
| create-trigger | 25/25 | 185/185 |
| sequence-ddl | 5/5 | 21/21 |
| async-notify | 6/6 | 35/35 |
| vacuum-no-cleanup-lock | 5/5 | 38/38 |
| timeouts | 8/8 | 32/32 |
| vacuum-concurrent-drop | 6/6 | 24/24 |
| vacuum-conflict | 16/16 | 108/108 |
| vacuum-skip-locked | 16/16 | 50/50 |
| stats | 56/56 | 604/604 |
| horizons | 5/5 | 51/51 |
| predicate-hash | 40/40 | 240/240 |
| predicate-gist | 40/40 | 240/240 |
| predicate-gin | 31/31 | 189/189 |
| partition-concurrent-attach | 3/3 | 24/24 |
| partition-drop-index-locking | 2/2 | 20/20 |
| partition-key-update-1 | 13/13 | 95/95 |
| partition-key-update-2 | 2/2 | 17/17 |
| partition-key-update-3 | 8/8 | 84/84 |
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
| ddl-dependency-locking | 17/17 | 85/85 |
| pub-concurrent-drop | 1/1 | 4/4 |
| drop-owned-grant | 1/1 | 5/5 |

## Which differences are engine internals

- **Statistics and pruning horizons are client-visible, not internals** (round 18, agent 56; the
  stats and horizons specs, 29 permutations, used to be classed as internals here). What a
  client sees through pg_stat_* and EXPLAIN ANALYZE / VACUUM now follows Postgres's semantics,
  computed the engine's way:
  - *When counts reach other sessions* (`exec/pgstat.rs` `report`): a backend's counts are
    pending until it goes idle; then pgstat_report_stat's rules apply: flushed unless the last
    flush was less than PGSTAT_MIN_INTERVAL (1 s) ago, at once after
    `pg_stat_force_next_flush()`, and counts kept go when the backend has been idle for
    PGSTAT_IDLE_INTERVAL (10 s) (flushed by the next reader of the statistics: no timer
    thread), when they have waited PGSTAT_MAX_INTERVAL, or when the session ends. The
    backend's own pg_stat_get_xact_* include what it kept. Dropped objects' entries go at
    the dropping commit. (Before: every transaction end flushed.)
  - Resets (`pg_stat_reset`, `pg_stat_reset_single_*_counters`) keep the entries, zeroed (a
    reset function's calls read 0, not NULL); `pg_stat_reset` leaves shared catalogs' alone.
  - pg_stat_slru counts the notify SLRU's zeroed pages as committed notifications move the
    queue head over pages (asyncQueueAddEntries), and is part of a stats_fetch_consistency
    snapshot / cache like the other statistics.
  - `COMMIT PREPARED` / `ROLLBACK PREPARED` of a name never prepared fail as Postgres's
    LockGXact does ("prepared transaction with identifier ... does not exist"), not with
    "prepared transactions are disabled".
  - Two-phase commit (round 19, agent 71): with `PGISOLATION_MAX_PREPARED=5` and a reference
    started with `-c max_prepared_transactions=5`, the 2PC specs' prepared paths run:
    prepared-transactions (1,432 permutations), prepared-transactions-cic (outside the
    schedule; `PGISOLATION_ONLY` names them), stats and two-ids match, 1,579 / 1,579. The
    schedule's runs keep the default (2PC off) on both sides.
  - *Pruning horizons* (`txn/xids.rs` `horizon`, `catalog/heapsim.rs` `Vis`): a transaction's
    snapshot registers its xmin (PGPROC.xmin; READ COMMITTED: the statement's, kept until the
    next statement), and what pruning, index-entry killing (kill_prior_tuple) and VACUUM may
    remove is what no running transaction or held snapshot can see; a temporary table's
    horizon is its own session's transaction only (GlobalVisTempRels). An index-only scan's
    Heap Fetches count every index entry it passes that isn't LP_DEAD on a page not
    all-visible, dead rows' included (`exec/scan.rs` `IosEntries`). VACUUM reports the dead
    tuples it had to keep (recently dead) as the table's n_dead_tup. Not modelled: horizons
    per database (snapshots in other databases hold back this one's), and a READ COMMITTED
    transaction idle between statements still holds its last statement's xmin.
- (T2 was a fixed cap on PL/pgSQL nesting, not the stack: gone in round 18. D2's TOAST
  relations are modelled since round 18: the engine still stores out-of-line values in its
  rows' keyspace, and the TOAST table is a name, locks and its size model's chunks.)
- What's left is two corner cases: L2's catalog-corrupting DELETE and L4's parallel lock
  group.
- **SSI** is implemented (`txn/ssi.rs`): no anomaly the suite checks commits, and the failures
  come where Postgres raises them. Not modelled: summarizing old serializable transactions into
  the SLRU when shared memory runs out (the engine keeps them until no snapshot needs them, the
  same conflicts), two-phase commit and parallel-worker sharing of a serializable transaction.

## Waits a client ends: timeouts and cancel (round 20, agent 73)

isolationtester cancels steps with the simple protocol and prints errors tersely, so the suite
doesn't see how drivers end waits. `tests/wire_waits.rs` does: every kind of wait (row locks
FOR UPDATE / SHARE, unique-key waits, relation locks, advisory session / transaction locks, a
SERIALIZABLE READ ONLY DEFERRABLE transaction's wait for a safe snapshot, pg_sleep, a long
scan, a FOR UPDATE portal fetched one row per Execute) ended by statement_timeout,
lock_timeout, SET LOCAL lock_timeout in a block, and a CancelRequest from another connection,
over the simple and the extended protocol (tokio-postgres: Parse / Bind / Execute); then the
connection's next statement and the waiter's retry once the hold ends. The transcript is
PostgreSQL 18.6's (`wire_waits.expected`, recorded from it: `WIRE_WAITS_REF`; tuple positions
masked). At 9407d0ee lock waits already ended right in both protocols (agent 51's driver
failures predate round 17's lock manager); what differed:

- **A safe-snapshot wait ignored statement_timeout**, and over the extended protocol a cancel
  of it was lost: Parse binds the query (exec_parse_message takes the snapshot, so the wait is
  there), the canceled bind fell back to binding again for its parameter types, and that bind
  found the snapshot already set and skipped the wait. The statement's waits (lock_timeout,
  deadlock_timeout, statement_timeout's deadline) are armed once per statement, before its
  snapshot (`exec::arm_statement_waits`, `Session::waits_armed`: each statement of a query
  string and each extended-protocol message starts afresh; a statement's later phases no longer
  restart its timeout), the wait takes them, and a failed wait leaves no snapshot behind.
- **A cancel arriving while the session was idle in a transaction block** stayed pending and
  canceled a later statement (ProcessInterrupts ignores it while DoingCommandRead): cleared
  when a message arrives.
- **XactLockTableWait's CONTEXT**: a wait for a row's locker or writer that ends in a timeout,
  cancel or deadlock says `while updating / deleting / locking tuple (b,o) in relation "t"`
  (`scan::Xltw`, `xltw_context`: the version the waiter's snapshot sees), a unique check's
  `while inserting index tuple (b,o) in relation "t_pkey"` (its own new tuple; the stored TID
  when it has one, else the key's stand-in, as for ctid). The tuple lock's wait has none, as
  in Postgres.
- **idle_in_transaction_session_timeout, transaction_timeout and idle_session_timeout** were
  accepted and ignored. Now: waiting for the next query after ReadyForQuery (not between an
  extended query's messages), a session past idle_in_transaction_session_timeout (in a block,
  failed or not) or idle_session_timeout (outside one) ends with Postgres's FATAL (25P03 /
  57P05); transaction_timeout (`Txn::txn_deadline`, from the transaction's start, or from the
  first statement after a SET turned it on) ends it idle or running, in lock and safe-snapshot
  waits, pg_sleep and scans (25P04; a statement_timeout not shorter than it doesn't arm, as
  enable_statement_timeout). `server.rs` `IdleRead::deadline`, `Session::idle_timeout`.

## What round 18 changed (agent 56)

What UPDATE, DELETE, MERGE, INSERT ... ON CONFLICT and foreign key checks see and change
under concurrency (E1–E4; wire tests `unique_checks_wait_for_writers_in_progress`,
`rechecks_after_concurrent_updates`):

- **Unique checks see writes in progress as Postgres's dirty snapshot does** (`exec/dml.rs`
  `find_conflict`). A unique key's lock (`locks::UNIQUE_KEY`) stands for an index entry in
  progress and records its (sub)transaction, speculative token and row (`locks::KeyHolder`);
  another writer of the key waits for that transaction (XactLockTableWait: pg_locks shows
  `transactionid ShareLock`) or its speculative token, then looks again. A row with the key
  being updated or deleted by a transaction in progress is waited for too (row locks record
  whether their xmax is lock-only, `mark_row_modified`). Before, an INSERT of a key whose row
  another transaction was deleting failed at once with a unique violation (Postgres waits and
  the insert succeeds), and ON CONFLICT DO NOTHING did nothing at once instead of waiting
  (partition-key-update-2/3).
- **INSERT ... ON CONFLICT is Postgres's speculative insertion**: the arbiters' pre-check
  (waiting; FormIndexDatum over every row it meets, so index expressions run as often as
  Postgres's), the row inserted under a speculative token (`spectoken` in pg_locks), its arbiter
  entries checked without waiting, and on a conflict the row killed (counted inserted and
  deleted, left dead in the size model) and the insertion started over at the pre-check
  (insert-conflict-specconflict). Index expressions run once per index per row (they ran two
  or three times); an UPDATE leaves an index none of whose columns changed alone without
  evaluating it (heap_update's HOT test).
- An ON CONFLICT DO UPDATE whose row changed while it waited starts over at its unique
  checks: BEFORE INSERT triggers no longer fire twice (eval-plan-qual-trigger).
- **MERGE**: a matched row a concurrent update made no longer match goes to WHEN NOT MATCHED
  BY SOURCE, and its source row then to WHEN NOT MATCHED (both act; EXPLAIN ANALYZE counts
  the join's row twice); a row moved to another partition fails with "tuple to be locked was
  already moved ..." (merge-update, merge-match-recheck).
- **Foreign key checks** (RI_FKey_check) read over the transaction snapshot in REPEATABLE READ
  unless the referenced table is partitioned (fk-snapshot: a referenced row committed after
  the snapshot isn't there); in READ COMMITTED a referenced row moved to another partition by
  the update the check waited for fails ("tuple to be locked was already moved ...") where it
  passed as a violation, and a row whose key that update changed is no match
  (partition-key-update-1).
- An UPDATE / DELETE by TID range rechecks the new version's TID (eval-plan-qual).
- pg_stat_activity shows application_name as `SET` left it (isolationtester names its
  sessions so; specs read pg_locks joined with it).

Locks on catalog rows, and what a statement finds after waiting for DDL (L1–L6, T1, T2;
wire tests `catalog_row_locks`, `after_waits_for_ddl`,
`concurrent_index_builds_wait_for_old_snapshots`):

- **Catalog rows are locked as Postgres's catalog tuples are** (`locks::catalog_row_key`,
  `Txn::lock_catalog_row`, `inplace_lock_catalog_row`). GRANT / REVOKE on a relation or a
  database updates its pg_class / pg_database row (waits for a transaction updating it or
  locking it FOR UPDATE / NO KEY UPDATE, then works over what it committed); an index build's
  in-place update of relhasindex, VACUUM's of pg_class and of datfrozenxid wait for a
  transaction updating the row (heap_inplace_lock, holding the tuple lock meanwhile, so
  deadlocks are detected); `SELECT ... FOR UPDATE / SHARE` over a catalog with an oid column
  locks its rows; `UPDATE pg_class` / `MERGE INTO pg_class` of the same row serialize, the
  second applying to the first's result (it failed with "concurrent schema change").
  DROP OWNED locks each role membership it deletes (AcquireDeletionLock), so a second one
  waits and then finds nothing left (it failed at commit). (intra-grant-inplace,
  intra-grant-inplace-db, drop-owned-grant, eval-plan-qual)
- **After a lock wait**: an inheritance child dropped while a query waited for it is left
  out (it was read: rows of a dropped table), one whose column changed type fails "does not
  match parent's type" (alter-table-4); a row routed to a default partition is checked
  against the constraint an ATTACH it waited for gave the partition (it went in, duplicating
  rows another partition then held), and ATTACH validates the default partition's rows over
  the latest snapshot (partition-concurrent-attach); a foreign key ATTACH or ADD CONSTRAINT
  adds locks the referenced table and its partitions SHARE ROW EXCLUSIVE and checks over the
  latest snapshot; a REPEATABLE READ RI check whose referenced row a transaction it waited for
  deleted fails "could not serialize access" (fk-partitioned-1/2).
- **Concurrent index builds**: CREATE INDEX CONCURRENTLY waits for the table's writers again
  after the build and then for every transaction holding a snapshot as old as its own
  (WaitForOlderSnapshots: snapshots are registered by transaction, `Xids::register_snapshot_of`;
  builds on plain columns are skipped, PROC_IN_SAFE_IC); its predicate's immutable functions
  run over constants when the build opens the index (eval_const_expressions); REINDEX
  CONCURRENTLY waits in all five of its phases (reindex-concurrently, multiple-cic).
- **Temporary objects**: DISCARD TEMP and the end of a session drop the session's temporary
  functions and types too, quietly (no "drop cascades" notices); a session's objects go when
  it ends, not with the database's last session (temp-schema-cleanup).
- PL/pgSQL calls nested more than 64 deep failed "stack depth limit exceeded" whatever the
  stack (a fixed cap from the engine's first version); only the stack bounds them now, as
  check_stack_depth does. An unoptimized build's connection threads get a 64 MB stack (16 MB
  optimized): its frames are ~10x bigger (~250 kB per PL/pgSQL level), and subxid-overflow's
  100 levels with exception blocks ran out.

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
  index pages, hash buckets, GiST pages and GIN entry pages / posting trees / metapage (with
  Postgres's lock promotion; bitmap index scans too), rw-conflict detection on reads (conflict out)
  and writes (conflict in), dooming and the reason codes of
  OnConflict_CheckForSerializationFailure / PreCommit_CheckForSerializationFailure, read-only
  transactions' safe snapshots and DEFERRABLE's wait for one. All the SSI specs
  (simple-write-skew, receipt-report, two-ids, read-only-anomaly-*, read-write-unique-*,
  predicate-hash / -gist / -gin, ...) match but multiple-row-versions (a plan choice, below).
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
- **Dependencies weren't locked**: a CREATE FUNCTION in a schema another session was dropping
  (or a DROP SCHEMA under a function being created in it) both committed, leaving a function in
  no schema. recordDependencyOn's object locks are taken now (schemas, types and domains,
  functions a SQL body calls, owner roles, foreign-data wrappers), DROP takes AccessExclusive
  on each object and sees what committed meanwhile: "referenced schema was concurrently
  dropped", "cannot drop schema ... because other objects depend on it".
- **Other sessions' temporary tables** under a permanent inheritance parent were scanned,
  updated, deleted and truncated through the parent; Postgres skips them.
- **A foreign key check against a partitioned referenced table** didn't lock the referenced
  row when it found it by scanning, so a concurrent DELETE of it committed under the new
  reference.
- Smaller: VACUUM / ANALYZE lock each relation in a transaction of its own (SKIP_LOCKED and
  "relation no longer exists" warnings); CONCURRENTLY index builds and drops wait for the table's
  lockers; `nextval` waits for `ALTER SEQUENCE`; heap order of a row another transaction is
  updating follows the version the reader sees; `pg_notification_queue_usage`; LOCK TABLE
  NOWAIT; a void built-in declared `RETURNS int` (LANGUAGE internal) returns 0, not NULL;
  `pg_get_publication_tables` is variadic and skips tables dropped while it waited; ATTACH
  PARTITION locks the default partition and a routed insert its leaf; an UPDATE / DELETE by
  TID rechecks the TID on the row's new version; a parallel worker's own locks end with it.

Client libraries' own test suites (`tests/clients`, agent 38) hit three of these from outside:
psycopg2's test_deadlock (two sessions' `LOCK ... IN ACCESS EXCLUSIVE MODE` neither blocked nor
deadlocked), pgx's TestTxCommitSerializationFailure (write skew between two SERIALIZABLE
transactions committed) and pgjdbc's closeInProgressStatement (a waiting statement's notice).
All three pass against this engine; `tests/wire.rs` heavyweight_locks_and_deadlocks and
serializable_write_skew keep the first two.

## DETACH PARTITION CONCURRENTLY (round 18, agent 56)

detach-partition-concurrently-1..4 went from 5 to 56 of 56 permutations. What changed:

- **Two transactions.** `ALTER TABLE ... DETACH PARTITION ... CONCURRENTLY`
  (`session/ddl/partition.rs` `detach_partition`) runs ATExecDetachPartition's: the first
  locks the parent and the partition ShareUpdateExclusive, marks the partition detach pending
  (`Table::detach_pending`, pg_inherits.inhdetachpending), checks foreign keys referencing it
  (ATDetachCheckNoForeignKeyRefs, ShareLock on the referencing tables, a fresh READ COMMITTED
  snapshot once that lock is held), adds the CHECK constraint standing for the partition
  constraint (as eval_const_expressions leaves it: a list bound's values one array constant)
  and commits (`session::end_txn_midway`); the second waits for every transaction holding a
  lock on the parent (WaitForLockersMultiple), locks the parent ShareUpdateExclusive and the
  partition AccessExclusive again ("... was removed concurrently") and finishes
  (DetachPartitionFinalize: dropping the derived foreign keys locks their tables
  AccessExclusive, as RemoveConstraintById does).
- **The pending state.** A query whose snapshot sees the mark's commit leaves the partition
  out of its parent (find_inheritance_children_extended's omit_detached): `Txn::catalog`
  hands such a statement a view of the catalog without it (`Catalog::detach_view`, cached per
  catalog version in `Database::detach_views`; the mark's commit time is in
  `Database::detach_marks`), with a version of its own, so prepared statements and other
  caches keyed by the version tell the two apart. A REPEATABLE READ snapshot older than the
  mark keeps the partition; insert routing in a snapshot transaction keeps it always
  (`Catalog::routing_partitions`); a foreign key check against a partitioned table plans with
  a fresh snapshot's view (`Txn::catalog_latest`, ri_PerformCheck's detectNewRows); statements
  without a snapshot and DROP (dependencies) see everything. DDL storing a catalog stores the
  full one (`Catalog::unview`).
- **After a cancel**: the mark stays; another DETACH CONCURRENTLY of the parent fails
  ("partition ... already pending detach"), a plain DETACH of the partition fails ("cannot
  detach partition"), any other ALTER TABLE of it fails ("cannot alter partition ... with an
  incomplete detach"), pg_partition_tree / pg_partition_ancestors stop at it
  (get_partition_ancestors), and `ALTER TABLE ... DETACH PARTITION ... FINALIZE` finishes it
  (it used to always fail "no pending concurrent detach").
- Also: PREPARE locks the relations its statement names (parse analysis), and DROP of a
  partition locks its parent AccessExclusive first (RangeVarCallbackForDropRelation), so
  queries planned with the partition wait for the drop.
- Not modelled: FINALIZE's WaitForOlderSnapshots (transactions holding a snapshot but no lock
  on the partition; no spec observes it, see L6).

Tests: `tests/wire.rs` `detach_partition_concurrently`, golden `detach_concurrently`.

