# SERIALIZABLE / REPEATABLE READ failures applications retry on (SQLSTATE 40001) and
# succeed with on retry; read-only deferrable; mixed isolation levels.

setup
{
  CREATE TABLE r_acct (id int PRIMARY KEY, bal int NOT NULL);
  INSERT INTO r_acct VALUES (1, 100), (2, 100);
  CREATE TABLE r_log (id serial PRIMARY KEY, note text);
}

teardown
{
  DROP TABLE r_acct, r_log;
}

session s1
step s1ser      { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1rc       { BEGIN; }
step s1sum      { SELECT sum(bal) FROM r_acct; }
step s1w1       { UPDATE r_acct SET bal = bal - 150 WHERE id = 1; }
step s1upd2     { UPDATE r_acct SET bal = bal + 1 WHERE id = 2; }
step s1del2     { DELETE FROM r_acct WHERE id = 2; }
step s1ins      { INSERT INTO r_log (note) SELECT 'sum ' || sum(bal) FROM r_acct; }
step s1lock2    { SELECT bal FROM r_acct WHERE id = 2 FOR UPDATE; }
step s1upsert   { INSERT INTO r_acct VALUES (2, 0) ON CONFLICT (id) DO UPDATE SET bal = 0; }
step s1commit   { COMMIT; }
step s1rollback { ROLLBACK; }
step s1show     { SELECT id, bal FROM r_acct ORDER BY id; }

session s2
step s2ser      { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s2rodef    { BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE; }
step s2ro       { BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY; }
step s2sum      { SELECT sum(bal) FROM r_acct; }
step s2w2       { UPDATE r_acct SET bal = bal - 150 WHERE id = 2; }
step s2upd2     { UPDATE r_acct SET bal = bal + 10 WHERE id = 2; }
step s2ins      { INSERT INTO r_log (note) SELECT 'sum ' || sum(bal) FROM r_acct; }
step s2commit   { COMMIT; }
step s2rollback { ROLLBACK; }

# write skew: one of them fails 40001 at its write or commit
permutation s1ser s2ser s1sum s2sum s1w1 s2w2 s1commit s2commit s1show
# the retry of the failed one then succeeds (or rightly refuses)
permutation s1ser s2ser s1sum s2sum s1w1 s2w2 s1commit s2commit s2ser s2sum s2w2 s2commit s1show
# REPEATABLE READ: concurrent update, delete, lock, and ON CONFLICT
permutation s1rr s1sum s2upd2 s1upd2 s1rollback
permutation s2ser s2upd2 s1rr s1sum s1upd2 s2commit s1rollback
permutation s2ser s2upd2 s1rr s1sum s1del2 s2commit s1rollback
permutation s2ser s2upd2 s1rr s1sum s1lock2 s2commit s1rollback
permutation s2ser s2upd2 s1rr s1sum s1upsert s2commit s1rollback
# READ COMMITTED sees the update after waiting
permutation s2ser s2upd2 s1rc s1upd2 s2commit s1commit s1show
# insert-from-aggregate (a dangerous structure) in SERIALIZABLE
permutation s1ser s2ser s1ins s2ins s1commit s2commit
# read only: a read-only transaction may fail; deferrable waits for a safe snapshot instead
permutation s1ser s1sum s1w1 s2ro s2sum s1commit s2sum s2commit
permutation s1ser s1sum s1w1 s2rodef s2sum s1commit s2commit
