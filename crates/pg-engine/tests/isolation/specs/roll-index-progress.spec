# Observe a build from another backend while its expression waits, then finish or cancel it.
setup
{
  CREATE TABLE ip (a int);
  INSERT INTO ip SELECT g FROM generate_series(1, 100) g;
  CREATE FUNCTION ip_key(int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
  BEGIN
    IF $1 = 1 THEN
      PERFORM pg_advisory_xact_lock(148, 1);
    END IF;
    RETURN $1;
  END $$;
  CREATE INDEX ip_existing ON ip (ip_key(a));
}

teardown
{
  DROP TABLE ip;
  DROP FUNCTION ip_key(int);
}

session s1
step hold { BEGIN; SELECT pg_advisory_xact_lock(148, 1); }
step release { COMMIT; }

session s2
step build { CREATE INDEX ip_new ON ip (ip_key(a)); }
step concurrent { CREATE INDEX CONCURRENTLY ip_new ON ip (ip_key(a)); }
step rebuild { REINDEX INDEX ip_existing; }
step concurrent_rebuild { REINDEX INDEX CONCURRENTLY ip_existing; }
step usable { SELECT count(*), sum(a) FROM ip; }

session s3
step progress
{
  SELECT command, phase, relid = 'ip'::regclass AS right_table,
         index_relid <> 0 AS named_index, blocks_total > 0 AS has_blocks,
         tuples_total, tuples_done
    FROM pg_stat_progress_create_index WHERE relid = 'ip'::regclass;
}
step cancel
{
  SELECT pg_cancel_backend(pid) FROM pg_stat_progress_create_index
    WHERE relid = 'ip'::regclass;
}
step finished
{
  SELECT count(*) FROM pg_stat_progress_create_index WHERE relid = 'ip'::regclass;
}

permutation hold build progress release finished usable
permutation hold concurrent progress release finished usable
permutation hold rebuild progress release finished usable
permutation hold concurrent_rebuild progress release finished usable
permutation hold build progress cancel release finished usable
permutation hold concurrent progress cancel release finished usable
permutation hold rebuild progress cancel release finished usable
permutation hold concurrent_rebuild progress cancel release finished usable
