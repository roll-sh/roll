# Exclusion constraints: concurrent inserts of non-conflicting rows don't wait for each other;
# a conflicting one waits for the inserter, then fails or goes in.

setup
{
  CREATE TABLE r_ex (r int4range, EXCLUDE USING gist (r WITH &&));
}

teardown
{
  DROP TABLE r_ex;
}

session s1
setup           { BEGIN; }
step s1ins      { INSERT INTO r_ex VALUES ('[1,10)'); }
step s1c        { COMMIT; }
step s1r        { ROLLBACK; }

session s2
setup           { BEGIN; }
step s2far      { INSERT INTO r_ex VALUES ('[20,30)'); }
step s2near     { INSERT INTO r_ex VALUES ('[5,15)'); }
step s2c        { COMMIT; }

permutation s1ins s2far s1c s2c
permutation s1ins s2near s1c s2c
permutation s1ins s2near s1r s2c
