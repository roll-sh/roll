# DDL that waits for a relation lock sees what the lock holder (and others) committed meanwhile,
# as PostgreSQL's AcceptInvalidationMessages after LockRelationOid: an index locked while another
# transaction's DDL commits isn't "relation <oid> does not exist" (pg_restore -j's ALTER TABLE
# ... ADD FOREIGN KEY, about one run in ten), and REINDEX, CLUSTER, VACUUM FULL and ALTER TABLE
# rewrites that waited neither fail as a concurrent schema change nor undo the other's change.

setup
{
  CREATE TABLE r_p (id int PRIMARY KEY, v int);
  CREATE TABLE r_c (id int PRIMARY KEY, pid int);
  CREATE INDEX r_c_pid ON r_c (pid);
  INSERT INTO r_p SELECT g, g FROM generate_series(1, 10) g;
  INSERT INTO r_c SELECT g, g FROM generate_series(1, 10) g;
}

teardown
{
  DROP TABLE r_c, r_p;
}

session s1
step s1begin   { BEGIN; }
step s1ixopt   { ALTER INDEX r_c_pid SET (fillfactor = 70); }
step s1stats   { ALTER TABLE r_p ALTER COLUMN v SET STATISTICS 50; }
step s1cstats  { ALTER TABLE r_c ALTER COLUMN pid SET STATISTICS 40; }
step s1comment { COMMENT ON TABLE r_p IS 'p'; }
step s1commit  { COMMIT; }

session s2
step s2rename  { ALTER INDEX r_c_pid RENAME TO r_c_pid2; }
step s2fk      { ALTER TABLE ONLY r_c ADD CONSTRAINT r_c_fk FOREIGN KEY (pid) REFERENCES r_p (id); }
step s2reindex { REINDEX TABLE r_p; }
step s2cluster { CLUSTER r_p USING r_p_pkey; }
step s2vacfull { VACUUM FULL r_p; }
step s2rewrite { ALTER TABLE r_p ALTER COLUMN v TYPE bigint; }
step s2addcol  { ALTER TABLE r_p ADD COLUMN w int DEFAULT 1; }
step s2check   {
  SELECT c.relname, c.reloptions, a.attname, a.attstattarget, obj_description(c.oid, 'pg_class') AS comment
    FROM pg_class c LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname IN ('v', 'pid')
   WHERE c.relname LIKE 'r\_%' ORDER BY 1, 3;
  SELECT conname, contype FROM pg_constraint WHERE conrelid IN ('r_p'::regclass, 'r_c'::regclass) ORDER BY 1;
}

session s3
step s3ddl     { CREATE TABLE r_other (a int); DROP TABLE r_other; }

# An index lock waited for while the holder's DDL commits.
permutation s1begin s1ixopt s2rename s1commit s2check
# A foreign key whose tables' locks are waited for; other DDL commits meanwhile.
permutation s1begin s1stats s2fk s3ddl s1commit s2check
permutation s1begin s1cstats s1comment s2fk s1commit s2check
# Rewrites that waited for a table whose statistics target, comment changed meanwhile.
permutation s1begin s1stats s1comment s2reindex s1commit s2check
permutation s1begin s1stats s1comment s2cluster s1commit s2check
permutation s1begin s1stats s1comment s2vacfull s1commit s2check
permutation s1begin s1comment s2rewrite s1commit s2check
permutation s1begin s1stats s2addcol s1commit s2check
