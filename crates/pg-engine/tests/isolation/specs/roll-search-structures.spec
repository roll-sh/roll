# Search structures of GiST and BRIN indexes (`exec::search`) under concurrency: a snapshot
# older than the structure reads every row; rows committed after it was built, updated to
# move, or deleted are found as each snapshot sees them; an exclusion check finds a
# conflicting row committed after the structure was built.

setup
{
  CREATE TABLE s_r (id int, r int4range, ts int);
  INSERT INTO s_r SELECT g, int4range(g * 10, g * 10 + 5), g FROM generate_series(1, 200) g;
  CREATE INDEX s_r_r ON s_r USING gist (r);
  CREATE INDEX s_r_ts ON s_r USING brin (ts) WITH (pages_per_range = 1);
  CREATE TABLE s_ex (id int, r int4range, EXCLUDE USING gist (r WITH &&));
  INSERT INTO s_ex SELECT g, int4range(g * 10, g * 10 + 5) FROM generate_series(1, 100) g;
}

teardown
{
  DROP TABLE s_r, s_ex;
}

session s1
setup           { SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s1rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM s_r; }
step s1find     { SELECT id, r FROM s_r WHERE r && int4range(50, 56) ORDER BY id; }
step s1far      { SELECT id, r FROM s_r WHERE r && int4range(5000, 5006) ORDER BY id; }
step s1brin     { SET enable_bitmapscan = on; SET enable_indexscan = off; SELECT id FROM s_r WHERE ts >= 4 AND ts <= 6 ORDER BY id; RESET enable_indexscan; SET enable_bitmapscan = off; }
step s1c        { COMMIT; }
step s1ins      { INSERT INTO s_ex VALUES (900, '[2000,2010)'); }

session s2
setup           { SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2warm     { SELECT count(*) FROM s_r WHERE r && int4range(1, 2); SELECT count(*) FROM s_ex WHERE r && int4range(1, 2); }
step s2move     { UPDATE s_r SET r = int4range(5000, 5005), ts = 5 WHERE id = 5; }
step s2new      { INSERT INTO s_r VALUES (300, int4range(52, 53), 4); }
step s2del      { DELETE FROM s_r WHERE id = 6; }
step s2ex       { INSERT INTO s_ex VALUES (901, '[2005,2006)'); }

permutation s1rr s2warm s2move s2new s2del s1find s1far s1brin s1c s1find s1far s1brin
permutation s2warm s1rr s2move s2new s1find s1far s1c s1find s1far s1brin
permutation s2warm s2ex s1ins s1find
permutation s1ins s2warm s2ex s1find
