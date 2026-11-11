# An UPDATE or DELETE of one row by its integer primary key under READ COMMITTED, while another
# transaction changes the row: the statement waits for the row's lock, then rechecks its
# newest version (EvalPlanQual), or skips it when it was deleted or its key changed; a row
# inserted under the same key after the statement's snapshot isn't one it sees.
setup
{
  CREATE TABLE pu (id int PRIMARY KEY, v int);
  INSERT INTO pu VALUES (1, 0), (2, 0);
}
teardown
{
  DROP TABLE pu;
}
session a
step abegin { BEGIN; }
step aupd { UPDATE pu SET v = v + 10 WHERE id = 1; }
step akey { UPDATE pu SET id = 3 WHERE id = 1; }
step adel { DELETE FROM pu WHERE id = 1; }
step adelins { DELETE FROM pu WHERE id = 1; INSERT INTO pu VALUES (1, 100); }
step acommit { COMMIT; }
step aabort { ROLLBACK; }
session b
setup { DEALLOCATE ALL; PREPARE pu_u(int) AS UPDATE pu SET v = v + 1 WHERE id = $1 RETURNING id, v; }
step bupd { UPDATE pu SET v = v + 1 WHERE id = 1 RETURNING id, v; }
step bupdc { UPDATE pu SET v = v + 1 WHERE id = 1 AND v = 0 RETURNING id, v; }
step bdel { DELETE FROM pu WHERE id = 1 RETURNING id, v; }
step bexec { EXECUTE pu_u(1); }
step bsel { SELECT * FROM pu ORDER BY id; }

permutation abegin aupd bupd acommit bsel
permutation abegin aupd bupdc acommit bsel
permutation abegin aupd bupd aabort bsel
permutation abegin akey bupd acommit bsel
permutation abegin adel bupd acommit bsel
permutation abegin adelins bupd acommit bsel
permutation abegin adel bdel acommit bsel
permutation abegin aupd bdel acommit bsel
permutation bexec bexec bexec bexec bexec abegin aupd bexec acommit bexec bsel
permutation abegin adelins bexec acommit bsel
