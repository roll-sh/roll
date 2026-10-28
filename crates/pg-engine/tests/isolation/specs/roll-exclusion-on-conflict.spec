# INSERT ... ON CONFLICT DO NOTHING with an exclusion constraint as its arbiter.
#
# Speculative insertions whose rows conflict never wait for each other: an arbiter's check
# waits only for an older speculative insertion of a newer transaction
# (CEOUC_LIVELOCK_PREVENTING_WAIT); otherwise the insertion kills its row and starts over at
# the pre-check. Index expressions take advisory locks the controller holds, so the
# insertions stop between their indexes: the plain index r_spec_c comes before the
# exclusion constraint's, r_spec_b after it (indexes are visited in oid order).
#
# And a deferred exclusion constraint's check waits at its recheck, not at the insert.

setup
{
  CREATE FUNCTION oc_gate(who int, n int) RETURNS int IMMUTABLE LANGUAGE plpgsql AS $$
  BEGIN
    PERFORM pg_advisory_xact_lock(who, n);
    RETURN who;
  END; $$;
  CREATE TABLE r_spec (r int4range, who int);
  CREATE INDEX r_spec_c ON r_spec (oc_gate(who, 3));
  ALTER TABLE r_spec ADD CONSTRAINT r_spec_x EXCLUDE USING gist (r WITH &&);
  CREATE INDEX r_spec_b ON r_spec (oc_gate(who, 4));
  CREATE TABLE r_oc (r int4range, EXCLUDE USING gist (r WITH &&));
  CREATE TABLE r_def (r int4range, EXCLUDE USING gist (r WITH &&) DEFERRABLE INITIALLY DEFERRED);
}

teardown
{
  DROP TABLE r_spec, r_oc, r_def;
  DROP FUNCTION oc_gate(int, int);
}

session c
step c_lock     { SELECT pg_advisory_lock(1, 3), pg_advisory_lock(2, 3), pg_advisory_lock(3, 4); }
step c_u13      { SELECT pg_advisory_unlock(1, 3); }
step c_u23      { SELECT pg_advisory_unlock(2, 3); }
step c_u34      { SELECT pg_advisory_unlock(3, 4); }
step c_waits    {
  SELECT regexp_replace(a.application_name, '.*/', '') AS who, l.locktype, l.mode
  FROM pg_locks l JOIN pg_stat_activity a USING (pid)
  WHERE l.locktype IN ('spectoken', 'transactionid') AND NOT l.granted
    AND a.datname = current_database()
  ORDER BY 1, 2;
}
step c_show     { SELECT r, who FROM r_spec ORDER BY r; }
step c_count    { SELECT count(*) FROM r_oc; }
step c_def      { SELECT r FROM r_def ORDER BY r; }

# Transactions get their ids in this order: old < new < t.
session old
step old_b      { BEGIN; SELECT pg_current_xact_id() IS NOT NULL AS x; }
step old_ins    { INSERT INTO r_spec VALUES ('[5,15)', 1) ON CONFLICT DO NOTHING; }
step old_c      { COMMIT; }

session new
step new_b      { BEGIN; SELECT pg_current_xact_id() IS NOT NULL AS x; }
step new_ins    { INSERT INTO r_spec VALUES ('[12,20)', 2) ON CONFLICT DO NOTHING; }
step new_c      { COMMIT; }

session t
step t_b        { BEGIN; SELECT pg_current_xact_id() IS NOT NULL AS x; }
step t_ins      { INSERT INTO r_spec VALUES ('[1,10)', 3) ON CONFLICT DO NOTHING; }
step t_c        { COMMIT; }
step t_r        { ROLLBACK; }

session s1
step s1b        { BEGIN; }
step s1hold     { INSERT INTO r_oc VALUES ('[1,100)'); }
step s1def      { INSERT INTO r_def VALUES ('[1,10)'); }
step s1c        { COMMIT; }
step s1r        { ROLLBACK; }

session s2
step s2b        { BEGIN; }
step s2oc       { INSERT INTO r_oc VALUES ('[5,15)') ON CONFLICT DO NOTHING; }
step s2def      { INSERT INTO r_def VALUES ('[5,15)'); }
step s2c        { COMMIT; }

# old and new pass their pre-checks and stop before the arbiter; t's speculative row stops
# after it. old (older than t) then waits for t's speculative insertion; new sees old's
# speculative row, gives its own up and waits for old's at the pre-check. t finishing lets
# new in and sends old back to the pre-check, behind t.
permutation c_lock old_b new_b t_b old_ins new_ins t_ins c_u13 c_u23 c_waits c_u34 c_waits t_c old_c new_c c_show
permutation c_lock old_b new_b t_b old_ins new_ins t_ins c_u13 c_u23 c_u34 t_r c_waits new_c old_c c_show

# A conflicting committed row: nothing to insert, at once.
permutation s1hold c_count s2oc c_count
# One in progress: the insert waits for it, then does nothing (it committed) or goes in.
permutation s1b s1hold s2oc s1c c_count
permutation s1b s1hold s2oc s1r c_count
# A deferred exclusion constraint: the second insert goes in at once, its COMMIT waits.
permutation s1b s1def s2b s2def s2c s1c c_def
permutation s1b s1def s2b s2def s2c s1r c_def
