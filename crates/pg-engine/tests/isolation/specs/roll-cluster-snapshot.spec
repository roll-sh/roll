# CLUSTER inside a REPEATABLE READ or SERIALIZABLE transaction whose snapshot misses another
# session's commit: PostgreSQL's copy keeps every tuple with its xmin and xmax, so the
# transaction goes on seeing its snapshot's rows (in the index's order, through the rebuilt
# indexes too), its UPDATE or row lock of a row changed since fails with 40001, a unique key
# committed since conflicts and a deleted one doesn't; the commit leaves the newest rows. An
# ALTER TABLE rewrite after it reads the newest rows (agent 324).

setup
{
  CREATE TABLE c_t (a int, b int);
  INSERT INTO c_t SELECT g, g FROM generate_series(10, 1, -1) g;
  CREATE UNIQUE INDEX c_t_a ON c_t (a);
}

teardown
{
  DROP TABLE c_t;
}

session s1
step s1rr      { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM c_t; }
step s1ser     { BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM c_t; }
step s1cluster { CLUSTER c_t USING c_t_a; }
step s1read    { SELECT string_agg(a || ':' || b, ' ') FROM c_t; }
step s1idx     {
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  SELECT count(*), sum(b) FROM c_t WHERE a > 0;
  RESET enable_seqscan; RESET enable_bitmapscan;
}
step s1upd3    { UPDATE c_t SET b = b + 100 WHERE a = 3; }
step s1lock3   { SELECT a FROM c_t WHERE a = 3 FOR UPDATE; }
step s1upd5    { UPDATE c_t SET b = b + 100 WHERE a = 5; }
step s1ins11   { INSERT INTO c_t VALUES (11, 11); }
step s1ins3    { INSERT INTO c_t VALUES (3, 33); }
step s1ci      { CREATE INDEX c_t_b ON c_t (b); }
step s1idxb    {
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  SELECT count(*) FROM c_t WHERE b > -1000;
  RESET enable_seqscan; RESET enable_bitmapscan;
}
step s1alter   { ALTER TABLE c_t ALTER COLUMN b TYPE bigint; }
step s1sp      { SAVEPOINT s; }
step s1rb      { ROLLBACK TO s; }
step s1prep    { PREPARE TRANSACTION 'c_t_cluster'; }
step s1commit  { COMMIT; }

session s2
step s2ins     { INSERT INTO c_t VALUES (11, 11); }
step s2del     { DELETE FROM c_t WHERE a = 3; }
step s2upd     { UPDATE c_t SET b = -b WHERE a = 3; }
step s2updk    { UPDATE c_t SET a = 30 WHERE a = 3; }
step s2cprep   { COMMIT PREPARED 'c_t_cluster'; }
step s2read    {
  SELECT string_agg(a || ':' || b, ' ') FROM c_t;
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  SELECT count(*), sum(b) FROM c_t WHERE a > 0;
  RESET enable_seqscan; RESET enable_bitmapscan;
}

# The snapshot's rows, in the index's order, by a scan and through the index; the commit
# leaves the newest.
permutation s1rr s2ins s2del s1cluster s1read s1idx s1commit s2read
# A row updated since: its UPDATE, or its row lock, fails.
permutation s1rr s2upd s1cluster s1read s1upd3 s1commit s2read
permutation s1rr s2del s1cluster s1lock3 s1commit s2read
permutation s1rr s2del s1cluster s1upd3 s1commit s2read
# Without CLUSTER: a row lock of a row deleted since says "update" (ExecLockRows), UPDATE "delete".
permutation s1rr s2del s1lock3 s1commit
# A row nobody changed: updated as usual; a key updated since stays out of sight.
permutation s1rr s2updk s1cluster s1upd5 s1read s1commit s2read
# A unique key committed since conflicts; one deleted since doesn't.
permutation s1rr s2ins s1cluster s1ins11 s1commit s2read
permutation s1rr s2del s1cluster s1ins3 s1read s1commit s2read
# An index built after it, CLUSTER again, a savepoint rolled back around it.
permutation s1rr s2ins s2del s1cluster s1ci s1idxb s1read s1commit s2read
permutation s1rr s2ins s2del s1cluster s1cluster s1read s1idx s1commit s2read
permutation s1rr s2ins s1sp s1cluster s1rb s1read s1commit s2read
# An ALTER TABLE rewrite after it reads the newest rows.
permutation s1rr s2ins s2del s1cluster s1alter s1read s1commit s2read
# SERIALIZABLE; a prepared transaction.
permutation s1ser s2ins s2upd s1cluster s1read s1upd3 s1commit s2read
permutation s1rr s2ins s2del s1cluster s1prep s2cprep s2read
