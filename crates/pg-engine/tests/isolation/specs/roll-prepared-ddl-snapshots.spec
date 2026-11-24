# DDL in a prepared transaction, seen from snapshots older than its COMMIT PREPARED: such a
# snapshot reads the table's rows through an index the prepared transaction built or rebuilt
# (roll's index entries are versions of their own, made by the COMMIT PREPARED: a snapshot
# older than it plans without the index), and a REPEATABLE READ transaction that rewrote a
# table changed after its snapshot prepares and commits without a serialization failure,
# keeping the rows committed after its snapshot. The same after a restart between PREPARE and
# COMMIT PREPARED: tests/twophase.rs (agent 312).

setup
{
  CREATE TABLE p_t (a int, b int);
  INSERT INTO p_t SELECT g, g FROM generate_series(1, 10) g;
  CREATE INDEX p_t_a ON p_t (a);
}

teardown
{
  DROP TABLE p_t;
}

session s1
step s1index    { BEGIN; CREATE INDEX p_t_b ON p_t (b); PREPARE TRANSACTION 'p_t_g'; }
step s1reindex  { BEGIN; REINDEX TABLE p_t; PREPARE TRANSACTION 'p_t_g'; }
step s1rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM p_t; }
step s1rewrite  {
  ALTER TABLE p_t ALTER COLUMN a TYPE bigint;
  UPDATE p_t SET a = a + 1 WHERE b = 7;
  PREPARE TRANSACTION 'p_t_g';
}
step s1commitp   { COMMIT PREPARED 'p_t_g'; }
step s1rollbackp { ROLLBACK PREPARED 'p_t_g'; }

session s2
step s2ins   { INSERT INTO p_t VALUES (11, 11); }
step s2upd   { UPDATE p_t SET a = a + 100 WHERE b = 7; }
step s2check {
  SET enable_seqscan = off;
  SELECT count(*) AS through_a, sum(b) FROM p_t WHERE a >= 0;
  RESET enable_seqscan;
  SELECT count(*) AS seq, sum(a), sum(b) FROM p_t;
  SELECT a FROM p_t WHERE b = 7;
}

session s3
step s3rr     { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1; }
step s3check  {
  SET enable_seqscan = off;
  SELECT count(*) AS through_a, sum(b) FROM p_t WHERE a >= 0;
  SELECT a AS a_of_b7 FROM p_t WHERE b = 7;
  SELECT count(*) AS b_below_6 FROM p_t WHERE b < 6;
  RESET enable_seqscan;
}
step s3commit { COMMIT; }

# An index built or rebuilt by a prepared transaction, read by a snapshot taken before its
# COMMIT PREPARED (and after it, once that transaction ends).
permutation s1index s3rr s1commitp s3check s3commit s3check
permutation s1index s3rr s1rollbackp s3check s3commit
permutation s1reindex s3rr s1commitp s3check s3commit s3check
# A REPEATABLE READ rewrite of a table changed after the snapshot, prepared.
permutation s1rr s2ins s2upd s1rewrite s1commitp s2check
permutation s1rr s2ins s1rewrite s3rr s1rollbackp s3check s3commit s2check
