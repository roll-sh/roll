# Concurrent CREATE of one name (Flyway's concurrent migrates): the second waits for the first
# transaction's uncommitted catalog row, then fails with the unique violation once the first
# commits (or goes ahead once it rolls back); IF NOT EXISTS skips with a NOTICE only when the
# first had committed before the check.

setup
{
  CREATE SCHEMA r_base;
}

teardown
{
  DROP SCHEMA IF EXISTS r_sch CASCADE;
  DROP SCHEMA r_base CASCADE;
}

session s1
step s1begin    { BEGIN; }
step s1schema   { CREATE SCHEMA r_sch; }
step s1schemane { CREATE SCHEMA IF NOT EXISTS r_sch; }
step s1table    { CREATE TABLE r_base.t (a int); }
step s1tablene  { CREATE TABLE IF NOT EXISTS r_base.t (a int); }
step s1seq      { CREATE SEQUENCE r_base.q; }
step s1view     { CREATE VIEW r_base.v AS SELECT 1 AS x; }
step s1commit   { COMMIT; }
step s1rollback { ROLLBACK; }

session s2
step s2begin    { BEGIN; }
step s2schema   { CREATE SCHEMA r_sch; }
step s2schemane { CREATE SCHEMA IF NOT EXISTS r_sch; }
step s2table    { CREATE TABLE r_base.t (b int); }
step s2tablene  { CREATE TABLE IF NOT EXISTS r_base.t (b int); }
step s2seq      { CREATE SEQUENCE r_base.q; }
step s2view     { CREATE VIEW r_base.v AS SELECT 2 AS y; }
step s2commit   { COMMIT; }
step s2check    { SELECT count(*) FROM pg_namespace WHERE nspname = 'r_sch'; SELECT relname, relkind FROM pg_class WHERE relnamespace = 'r_base'::regnamespace ORDER BY 1; }

permutation s1begin s1schema s2schema s1commit s2check
permutation s1begin s1schema s2schemane s1commit s2check
permutation s1begin s1schemane s2begin s2schemane s1commit s2commit s2check
permutation s1begin s1schema s2schema s1rollback s2check
permutation s1begin s1table s2table s1commit s2check
permutation s1begin s1tablene s2begin s2tablene s1commit s2commit s2check
permutation s1begin s1table s2tablene s1rollback s2check
permutation s1begin s1table s1commit s2tablene s2check
permutation s1begin s1seq s2seq s1commit s2check
permutation s1begin s1view s2view s1commit s2check
permutation s1begin s1table s2view s1commit s2check
