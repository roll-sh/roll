# DDL whose table lock waited for a writer's transaction reads the rows that transaction
# committed meanwhile, as PostgreSQL's index build (SnapshotAny) and table rewrite
# (GetLatestSnapshot) do: the statement's snapshot, taken before the wait, is not the one the
# command reads. With it, CREATE INDEX (and a unique one, ADD PRIMARY KEY) left the writer's row
# out of the index, and CLUSTER and ALTER TABLE rewrites lost the row itself (found by
# tools/crash-binary.py's create-index round, agent 307).

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
step s1ins    { BEGIN; INSERT INTO w_t VALUES (11, 11); }
step s1commit { COMMIT; }

session s2
step s2index    { CREATE INDEX w_t_b ON w_t (b); }
step s2unique   { CREATE UNIQUE INDEX w_t_b ON w_t (b); }
step s2pkey     { ALTER TABLE w_t ADD PRIMARY KEY (b); }
step s2cluster  { CLUSTER w_t USING w_t_a; }
step s2rewrite  { ALTER TABLE w_t ALTER COLUMN b TYPE bigint; }
step s2volatile { ALTER TABLE w_t ADD COLUMN c float8 DEFAULT random(); }
step s2check    {
  SET enable_seqscan = off;
  SELECT count(*) AS through_b, sum(a) FROM w_t WHERE b >= 0;
  SELECT count(*) AS through_a, sum(b) FROM w_t WHERE a >= 0;
  RESET enable_seqscan;
}

permutation s1ins s2index s1commit s2check
permutation s1ins s2unique s1commit s2check
permutation s1ins s2pkey s1commit s2check
permutation s1ins s2cluster s1commit s2check
permutation s1ins s2rewrite s1commit s2check
permutation s1ins s2volatile s1commit s2check
