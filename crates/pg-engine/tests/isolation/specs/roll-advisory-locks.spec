# Advisory locks: session and transaction level, shared and exclusive, try variants,
# stacking, unlock warnings, the int4 pair keys, and what pg_locks shows.

setup
{
  CREATE TABLE r_dummy (i int);
}

teardown
{
  DROP TABLE r_dummy;
}

session s1
step s1lock     { SELECT pg_advisory_lock(1); }
step s1lock2    { SELECT pg_advisory_lock(1); }
step s1share    { SELECT pg_advisory_lock_shared(2); }
step s1pair     { SELECT pg_advisory_lock(7, 8); }
step s1unlock   { SELECT pg_advisory_unlock(1); }
step s1unlockx  { SELECT pg_advisory_unlock(99); }
step s1unlocks  { SELECT pg_advisory_unlock_shared(1); }
step s1unlockall { SELECT pg_advisory_unlock_all(); }
step s1begin    { BEGIN; }
step s1xact     { SELECT pg_advisory_xact_lock(3); }
step s1xacts    { SELECT pg_advisory_xact_lock_shared(4); }
step s1commit   { COMMIT; }
step s1rollback { ROLLBACK; }
step s1locks    { SELECT locktype, classid, objid, objsubid, mode, granted FROM pg_locks
                  WHERE locktype = 'advisory' AND pid = pg_backend_pid() ORDER BY objid, objsubid, mode; }

session s2
step s2try      { SELECT pg_try_advisory_lock(1); }
step s2trys     { SELECT pg_try_advisory_lock_shared(2); }
step s2tryx     { SELECT pg_try_advisory_lock(2); }
step s2tryxact  { SELECT pg_try_advisory_xact_lock(3); }
step s2tryxacts { SELECT pg_try_advisory_xact_lock_shared(4); }
step s2trypair  { SELECT pg_try_advisory_lock(7, 8), pg_try_advisory_lock(8, 7); }
step s2lock     { SELECT pg_advisory_lock(1); }
step s2xact     { SELECT pg_advisory_xact_lock(3); }
step s2unlockall { SELECT pg_advisory_unlock_all(); }
step s2locks    { SELECT locktype, classid, objid, objsubid, mode, granted FROM pg_locks
                  WHERE locktype = 'advisory' AND pid = pg_backend_pid() ORDER BY objid, objsubid, mode; }

# stacking: two locks need two unlocks; a third unlock warns
permutation s1lock s1lock2 s2try s1unlock s2try s1unlock s2try s1unlock s2unlockall
# session locks survive ROLLBACK; xact locks end with it
permutation s1begin s1lock s1xact s1xacts s1locks s1rollback s1locks s2tryxact s2try s1unlockall s2unlockall
# a waiter gets the lock when the holder unlocks; shared vs exclusive
permutation s1lock s1share s2trys s2tryx s2lock s1unlock s2locks s1unlockall s2unlockall
# xact lock held until commit; the waiter then gets it
permutation s1begin s1xact s2xact s1locks s1commit s2locks
# unlocking what isn't held, or with the wrong mode
permutation s1unlockx s1lock s1unlocks s1unlock
# int4 pair keys are separate from bigint keys
permutation s1pair s1locks s2trypair s2locks s1unlockall s2unlockall
# pg_advisory_unlock_all leaves transaction-level locks
permutation s1begin s1xact s1lock s1unlockall s1locks s2tryxact s1commit s2tryxact
