# MultiXact: a second locker of a row makes a MultiXactId of both (and a third expands it); an
# updater joining lockers is the multi's update member; a FOR KEY SHARE lock past a running
# update that left the key alone locks the new version too; the xmax stays once they end.
# Ids are printed relative to each other, so the output doesn't depend on the cluster's counters.

setup
{
  CREATE EXTENSION IF NOT EXISTS pgrowlocks;
  CREATE TABLE mxs (id int PRIMARY KEY, v int);
  INSERT INTO mxs SELECT g, g FROM generate_series(1, 3) g;
}

teardown
{
  DROP TABLE mxs;
}

session s1
step s1_begin { BEGIN; }
step s1_keyshare { SELECT id FROM mxs WHERE id = 1 FOR KEY SHARE; }
step s1_share_all { SELECT id FROM mxs FOR KEY SHARE; }
step s1_update { UPDATE mxs SET v = v + 1 WHERE id = 1; }
step s1_commit { COMMIT; }

session s2
step s2_begin { BEGIN; }
step s2_share { SELECT id FROM mxs WHERE id = 1 FOR SHARE; }
step s2_share_all { SELECT id FROM mxs FOR KEY SHARE; }
step s2_keyshare { SELECT id FROM mxs WHERE id = 1 FOR KEY SHARE; }
step s2_update { UPDATE mxs SET v = v + 1 WHERE id = 1; }
step s2_commit { COMMIT; }

session s3
step s3_begin { BEGIN; }
step s3_keyshare { SELECT id FROM mxs WHERE id = 1 FOR KEY SHARE; }
step s3_commit { COMMIT; }

session ctl
step c_locks {
  SELECT locked_row, multi, modes, array_length(xids, 1) AS n FROM pgrowlocks('mxs') ORDER BY 1;
}
step c_rows { SELECT id, v, xmax = '0' AS none FROM mxs ORDER BY id; }
step c_members {
  SELECT array_agg(mode ORDER BY mode) FROM pg_get_multixact_members((SELECT xmax FROM mxs WHERE id = 1));
}
step c_same { SELECT count(DISTINCT xmax::text) AS xmaxes FROM mxs; }

# Two lockers, then a third: a multi, then a bigger one; the xmax stays after they end.
permutation s1_begin s2_begin s3_begin s1_keyshare c_locks s2_share c_locks c_members s3_keyshare c_locks c_members c_rows s1_commit c_locks s2_commit s3_commit c_locks c_rows c_members
# A locker and an updater: the old version's xmax is a multi with an update member.
permutation s1_begin s2_begin s1_keyshare s2_update c_locks s1_commit s2_commit c_rows
# An update, then a FOR KEY SHARE lock that doesn't wait for it: both versions locked.
permutation s1_begin s2_begin s1_update s2_keyshare c_locks s1_commit c_locks c_rows s2_commit
# Two transactions locking the same rows: one multi for all of them.
permutation s1_begin s2_begin s1_share_all s2_share_all c_locks c_same s1_commit s2_commit
