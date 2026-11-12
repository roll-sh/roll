# A prepared SELECT of one row by its integer primary key, executed again and again (a kept
# plan, its rows sent from their stored values), while another session changes the row: READ
# COMMITTED sees each commit at its next execution, REPEATABLE READ keeps its snapshot,
# SERIALIZABLE takes the predicate locks that make write skew fail; a deleted and re-inserted
# key, a NULL key, an int8 key, DDL between executions (the cached plan's result type), row
# level security and column privileges as they apply to the same statement.
setup
{
  CREATE TABLE pt (id int PRIMARY KEY, v int, t text, d date);
  INSERT INTO pt SELECT g, 0, 'r' || g, date '2024-01-01' + g FROM generate_series(1, 5000) g;
  ANALYZE pt;
  CREATE TABLE pr (id int PRIMARY KEY, owner text, v int);
  INSERT INTO pr VALUES (1, 'roll_point_reader', 1), (2, 'someone', 2);
  ALTER TABLE pr ENABLE ROW LEVEL SECURITY;
  CREATE POLICY mine ON pr USING (owner = current_user);
  CREATE ROLE roll_point_reader;
  GRANT SELECT ON pr TO roll_point_reader;
  GRANT SELECT (id, v) ON pt TO roll_point_reader;
}
teardown
{
  DROP TABLE pt;
  DROP TABLE pr;
  DROP ROLE roll_point_reader;
}
session a
step abegin { BEGIN; }
step aser { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step aread2 { SELECT id, v FROM pt WHERE id = 2; }
step aupd { UPDATE pt SET v = v + 10, t = 'changed' WHERE id = 1; }
step adelins { DELETE FROM pt WHERE id = 1; INSERT INTO pt VALUES (1, 100, 'again', NULL); }
step aaddcol { ALTER TABLE pt ADD COLUMN w int DEFAULT 5; }
step acommit { COMMIT; }
step aabort { ROLLBACK; }
session b
setup { DEALLOCATE ALL; PREPARE ps(int) AS SELECT * FROM pt WHERE id = $1; PREPARE pv(int) AS SELECT id, v FROM pt WHERE id = $1; PREPARE p8(int8) AS SELECT id, t FROM pt WHERE id = $1; PREPARE pp(int) AS SELECT * FROM pr WHERE id = $1; }
step bsel { EXECUTE ps(1); }
step bv { EXECUTE pv(1); }
step bnull { EXECUTE ps(NULL); }
step b8 { EXECUTE p8(1); }
step brr { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step bser { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step bupd2 { UPDATE pt SET v = v + 1 WHERE id = 2; }
step bcommit { COMMIT; }
step brole { SET ROLE roll_point_reader; }
step bpp1 { EXECUTE pp(1); }
step bpp2 { EXECUTE pp(2); }
step bunrole { RESET ROLE; }

permutation bsel bsel bsel abegin aupd bsel acommit bsel
permutation bsel abegin aupd bsel aabort bsel
permutation bsel bsel brr bsel abegin aupd acommit bsel bcommit bsel
permutation bsel bsel abegin adelins bsel acommit bsel bsel
permutation bsel bsel bnull b8 b8 bsel
permutation bsel bsel aaddcol bsel bv bv
permutation bv bv aser aread2 bser bv aupd bupd2 acommit bcommit
permutation bpp1 bpp2 brole bpp1 bpp1 bpp2 bv bv bsel bunrole bpp2
