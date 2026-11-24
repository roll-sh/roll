# Index builds and table rewrites in a REPEATABLE READ or SERIALIZABLE transaction read the
# table's newest committed rows, not the transaction's snapshot, as PostgreSQL's index build
# (SnapshotAny) and rewrite (GetLatestSnapshot) do: an index built there had no entry for a
# row committed after the snapshot, a rewrite (ALTER COLUMN TYPE, ADD COLUMN with a volatile
# default, CLUSTER) lost that row and gave back a row deleted after it, and SET UNLOGGED
# failed its commit on a row updated after it. A unique index sees a duplicate committed
# after the snapshot. And a snapshot older than another session's CREATE INDEX reads the
# table's rows through that index (agent 310).

setup
{
  CREATE TABLE w_t (a int, b int);
  INSERT INTO w_t SELECT g, g FROM generate_series(1, 10) g;
  CREATE INDEX w_t_a ON w_t (a);
}

teardown
{
  DROP TABLE w_t;
}

session s1
step s1rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM w_t; }
step s1ser      { BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM w_t; }
step s1index    { CREATE INDEX w_t_b ON w_t (b); }
step s1unique   { CREATE UNIQUE INDEX w_t_b ON w_t (b); }
step s1pkey     { ALTER TABLE w_t ADD PRIMARY KEY (b); }
step s1cluster  { CLUSTER w_t USING w_t_a; }
step s1rewrite  { ALTER TABLE w_t ALTER COLUMN b TYPE bigint; }
step s1volatile { ALTER TABLE w_t ADD COLUMN c float8 DEFAULT random(); }
step s1unlogged { ALTER TABLE w_t SET UNLOGGED; }
step s1through  {
  SET enable_seqscan = off;
  SELECT count(*) AS through_b, sum(a) FROM w_t WHERE b >= 0;
  RESET enable_seqscan;
}
step s1seq      { SELECT count(*) AS seq, sum(a), sum(b) FROM w_t; }
step s1commit   { COMMIT; }

session s2
step s2ins   { INSERT INTO w_t VALUES (11, 11); }
step s2del   { DELETE FROM w_t WHERE a = 5; }
step s2upd   { UPDATE w_t SET b = b + 100 WHERE a = 7; }
step s2dup   { INSERT INTO w_t VALUES (12, 3); }
step s2check {
  SET enable_seqscan = off;
  SELECT count(*) AS through_b, sum(a) FROM w_t WHERE b >= 0;
  SELECT count(*) AS through_a, sum(b) FROM w_t WHERE a >= 0;
  RESET enable_seqscan;
  SET enable_indexscan = off;
  SET enable_bitmapscan = off;
  SET enable_indexonlyscan = off;
  SELECT count(*) AS seq, sum(a), sum(b) FROM w_t;
  RESET enable_indexscan;
  RESET enable_bitmapscan;
  RESET enable_indexonlyscan;
}

session s3
step s3rr     { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1; }
step s3check  {
  SET enable_seqscan = off;
  SELECT count(*) AS through_b, sum(a) FROM w_t WHERE b >= 0;
  SELECT count(*) AS through_b5 FROM w_t WHERE b = 5;
  RESET enable_seqscan;
}
step s3commit { COMMIT; }

# Index builds in the transaction: the new index has the rows committed after its snapshot,
# and the snapshot still reads its own rows through it.
permutation s1rr s2ins s1index s1through s1commit s2check
permutation s1rr s2del s1index s1through s1commit s2check
permutation s1rr s2upd s1index s1through s1commit s2check
permutation s1ser s2ins s1unique s1through s1commit s2check
permutation s1rr s2dup s1unique s1commit s2check
permutation s1rr s2ins s1pkey s1commit s2check
permutation s1ser s2del s1pkey s1commit s2check
# Rewrites in the transaction: no committed row lost, no deleted one back, no failed commit.
permutation s1rr s2ins s1cluster s1commit s2check
permutation s1rr s2del s1cluster s1commit s2check
permutation s1rr s2ins s1rewrite s1seq s1commit s2check
permutation s1rr s2del s1rewrite s1seq s1commit s2check
permutation s1ser s2upd s1volatile s1seq s1commit s2check
permutation s1rr s2upd s1unlogged s1seq s1commit s2check
# A snapshot older than the index.
permutation s3rr s2del s1index s3check s3commit
permutation s3rr s2upd s1unique s3check s3commit
