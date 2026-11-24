# pg_index.indcheckxmin as PostgreSQL sets it: a CREATE INDEX whose build finds a broken HOT
# chain (a HOT-updated row version still visible to a snapshot, or updated by the building
# transaction itself) marks the index unusable for snapshots whose xmin isn't past the index's
# pg_index row; REINDEX, VACUUM FULL and CLUSTER clear it once no chain is broken. EXPLAIN
# shows where the planner leaves the index out (agent 312).

setup
{
  CREATE TABLE h_t (a int, b int) WITH (fillfactor = 50);
  INSERT INTO h_t SELECT g, g FROM generate_series(1, 10) g;
}

teardown
{
  DROP TABLE h_t;
}

session s1
step s1rr      { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM h_t; }
step s1xid     { BEGIN; INSERT INTO h_t VALUES (100, 100); }
step s1explain {
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  EXPLAIN (COSTS OFF) SELECT a FROM h_t WHERE b = 5;
  RESET enable_seqscan; RESET enable_bitmapscan;
}
step s1commit  { COMMIT; }

session s2
step s2upd      { UPDATE h_t SET a = a + 100 WHERE b = 5; }
step s2index    { CREATE INDEX h_t_b ON h_t (b); }
step s2txnindex { BEGIN; UPDATE h_t SET a = a + 1 WHERE b = 6; CREATE INDEX h_t_b ON h_t (b); }
step s2explain  {
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  EXPLAIN (COSTS OFF) SELECT a FROM h_t WHERE b = 5;
  RESET enable_seqscan; RESET enable_bitmapscan;
}
step s2commit   { COMMIT; }
step s2reindex  { REINDEX INDEX h_t_b; }
step s2vacfull  { VACUUM FULL h_t; }
step s2checkxmin { SELECT indcheckxmin FROM pg_index WHERE indexrelid = 'h_t_b'::regclass; }

session s3
step s3explain {
  SET enable_seqscan = off; SET enable_bitmapscan = off;
  EXPLAIN (COSTS OFF) SELECT a FROM h_t WHERE b = 5;
  RESET enable_seqscan; RESET enable_bitmapscan;
}

# A snapshot older than the HOT update: set; that snapshot plans without the index (roll's
# reads nothing of an index committed after it either); REINDEX clears it once it ends.
permutation s1rr s2upd s2index s2checkxmin s1explain s2explain s1commit s2reindex s2checkxmin
# A transaction with an id older than the index: every snapshot whose xmin it holds back
# plans without the index until it ends.
permutation s1xid s2upd s2index s2checkxmin s3explain s1commit s3explain
# Its own HOT update: set, and the transaction itself plans without the index.
permutation s2txnindex s2explain s2commit s2checkxmin s2explain
# Nothing still sees the old version: not set.
permutation s2upd s2index s2checkxmin
# VACUUM FULL clears it.
permutation s1rr s2upd s2index s1commit s2checkxmin s2vacfull s2checkxmin
