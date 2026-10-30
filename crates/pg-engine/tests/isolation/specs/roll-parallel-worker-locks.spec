# Locks a real parallel plan's lower part takes (a PARALLEL SAFE function's advisory locks,
# at transaction and session level) belong to the parallel workers that run it, and go when
# they exit at the Gather's end. With parallel_leader_participation off only the workers run
# that part, so none of those locks is left to the leader, and another session can take them
# while the leader's transaction is still open.

setup
{
  CREATE TABLE pw_t (a int);
  INSERT INTO pw_t SELECT generate_series(1, 1000);
  ANALYZE pw_t;
  CREATE FUNCTION pw_xact(int) RETURNS int LANGUAGE plpgsql PARALLEL SAFE
    AS $$ BEGIN PERFORM pg_advisory_xact_lock(42, $1); RETURN 1; END $$;
  CREATE FUNCTION pw_session(int) RETURNS int LANGUAGE plpgsql PARALLEL SAFE
    AS $$ BEGIN PERFORM pg_advisory_lock(43, $1); RETURN 1; END $$;
}

teardown
{
  DROP TABLE pw_t;
  DROP FUNCTION pw_xact(int), pw_session(int);
}

session s1
setup
{
  SET parallel_setup_cost = 0;
  SET parallel_tuple_cost = 0;
  SET min_parallel_table_scan_size = 0;
  SET max_parallel_workers_per_gather = 2;
  SET parallel_leader_participation = off;
}
step s1plan    { EXPLAIN (COSTS OFF) SELECT count(*) FROM pw_t WHERE pw_xact(a) = 1; }
step s1begin   { BEGIN; }
step s1xact    { SELECT count(*) FROM pw_t WHERE pw_xact(a) = 1; }
step s1session { SELECT count(*) FROM pw_t WHERE pw_session(a) = 1; }
step s1locks   { SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid(); }
step s1lockall { SELECT pg_advisory_xact_lock(42, 5); }
step s1commit  { COMMIT; }
step s1serial  { SET parallel_setup_cost = 1000000; }
teardown       { SELECT pg_advisory_unlock_all(); }

session s2
step s2xact    { SELECT pg_try_advisory_xact_lock(42, 1), pg_try_advisory_xact_lock(42, 1000); }
step s2session { SELECT pg_try_advisory_lock(43, 7), pg_advisory_unlock(43, 7); }
step s2five    { SELECT pg_try_advisory_xact_lock(42, 5); }

# the workers' transaction-level locks went with them; the leader's own stays
permutation s1plan s1begin s1xact s1locks s2xact s1lockall s1locks s2five s1commit
# so did their session-level ones
permutation s1begin s1session s1locks s2session s1commit s1locks
# without a Gather the leader runs the function and keeps its locks
permutation s1serial s1begin s1xact s1locks s2xact s1commit s2xact
