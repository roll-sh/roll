# DETACH PARTITION ... FINALIZE waits for transactions holding snapshots older than its own
# (WaitForOlderSnapshots), even ones holding no lock on the partition.

setup
{
  CREATE TABLE r_d (a int) PARTITION BY LIST (a);
  CREATE TABLE r_d1 PARTITION OF r_d FOR VALUES IN (1);
  CREATE TABLE r_d2 PARTITION OF r_d FOR VALUES IN (2);
  INSERT INTO r_d VALUES (1), (2);
  CREATE TABLE r_other (i int);
}

teardown
{
  DROP TABLE IF EXISTS r_d, r_d1, r_d2, r_other;
}

session s1
step s1b        { BEGIN; }
step s1s        { SELECT * FROM r_d ORDER BY a; }
step s1c        { COMMIT; }

session s2
step s2detach   { ALTER TABLE r_d DETACH PARTITION r_d1 CONCURRENTLY; }
step s2final    { ALTER TABLE r_d DETACH PARTITION r_d1 FINALIZE; }
step s2noparent { ALTER TABLE r_d DETACH PARTITION r_other FINALIZE; }
step s2show     { SELECT relname, relispartition FROM pg_class WHERE relname LIKE 'r_d_' ORDER BY 1; }

session s3
step s3cancel   { SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE query LIKE 'ALTER TABLE r_d DETACH%CONCURRENTLY%' AND pid <> pg_backend_pid(); }
step s3rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s3snap     { SELECT count(*) FROM r_other; }
step s3c        { COMMIT; }

# a reader holds the parent; the concurrent detach waits for it and is canceled; a snapshot
# taken before FINALIZE (with no lock on the partition) makes FINALIZE wait for it
permutation s1b s1s s2detach s3cancel s1c s3rr s3snap s2final s3c s2show
# with no older snapshot FINALIZE doesn't wait
permutation s1b s1s s2detach s3cancel s1c s2final s2show
# FINALIZE of a table that isn't a partition waits too, then fails
permutation s3rr s3snap s2noparent s3c
