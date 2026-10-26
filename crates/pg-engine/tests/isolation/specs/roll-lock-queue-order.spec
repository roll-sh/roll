# Lock queue fairness: a waiting strong lock request blocks later weaker requests that
# conflict with it (JoinWaitQueue), for relation locks and row locks; LOCK ... NOWAIT behind
# a queue; a holder's own stronger request jumps the queue.

setup
{
  CREATE TABLE r_fair (id int PRIMARY KEY, v int);
  INSERT INTO r_fair VALUES (1, 0), (2, 0);
}

teardown
{
  DROP TABLE r_fair;
}

session s1
setup           { BEGIN; }
step s1read     { SELECT count(*) FROM r_fair; }
step s1rowshare { SELECT v FROM r_fair WHERE id = 1 FOR SHARE; }
step s1excl     { LOCK TABLE r_fair IN ACCESS EXCLUSIVE MODE; }
step s1commit   { COMMIT; }

session s2
setup           { BEGIN; }
step s2excl     { LOCK TABLE r_fair IN ACCESS EXCLUSIVE MODE; }
step s2rowupd   { SELECT v FROM r_fair WHERE id = 1 FOR UPDATE; }
step s2commit   { COMMIT; }

session s3
setup           { BEGIN; }
step s3read     { SELECT count(*) FROM r_fair; }
step s3readnw   { LOCK TABLE r_fair IN ACCESS SHARE MODE NOWAIT; }
step s3rowshare { SELECT v FROM r_fair WHERE id = 1 FOR SHARE; }
step s3rowsharenw { SELECT v FROM r_fair WHERE id = 1 FOR SHARE NOWAIT; }
step s3commit   { COMMIT; }

session s4
step s4locks    { SELECT l.mode, l.granted, a.application_name FROM pg_locks l
                  JOIN pg_stat_activity a USING (pid)
                  WHERE l.locktype = 'relation' AND l.relation = 'r_fair'::regclass
                  ORDER BY a.application_name, l.mode; }

# AccessShare held; AccessExclusive waits; a later AccessShare queues behind it
permutation s1read s2excl s3read s4locks s1commit s2commit s3commit
# NOWAIT behind the queued AccessExclusive fails at once
permutation s1read s2excl s3readnw s1commit s2commit s3commit
# the holder's own upgrade goes ahead of the queue (deadlock-free: it holds a conflicting lock)
permutation s1read s3read s2excl s1excl s3commit s1commit s2commit
# row locks: FOR SHARE held, FOR UPDATE waits; a later FOR SHARE doesn't queue behind it but
# joins the share lockers (heap_lock_tuple takes the tuple lock only to wait), and the FOR
# UPDATE then waits for it too
permutation s1rowshare s2rowupd s3rowshare s1commit s3commit s2commit
permutation s1rowshare s2rowupd s3rowsharenw s1commit s3commit s2commit
