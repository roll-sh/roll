# A migration's ALTER TABLE under lock_timeout next to a long transaction: the ALTER queues
# for its AccessExclusive lock, and the reads and writes arriving after it queue behind it;
# when it gives up (lock_timeout), they go ahead at once, before the long transaction ends.
# Then the same ALTER retried once the long transaction is gone goes through.

setup
{
  CREATE TABLE m_t (id int PRIMARY KEY, v int);
  INSERT INTO m_t SELECT g, g FROM generate_series(1, 5) g;
}

teardown
{
  DROP TABLE m_t;
}

session s1
step s1b        { BEGIN; }
step s1sel      { SELECT count(*) FROM m_t; }
step s1sleep    { SELECT pg_sleep(2); }
step s1c        { COMMIT; }

session s2
setup           { SET lock_timeout = '1s'; }
step s2alter    { ALTER TABLE m_t ADD COLUMN w int; }
step s2cols     { SELECT count(*) FROM pg_attribute WHERE attrelid = 'm_t'::regclass AND attnum > 0 AND NOT attisdropped; }

session s3
step s3sel      { SELECT count(*) FROM m_t WHERE id <= 5; }

session s4
step s4ins      { INSERT INTO m_t VALUES (100, 1); }
step s4upd      { UPDATE m_t SET v = v + 1 WHERE id = 1; }

session s5
step s5locks    {
  SELECT regexp_replace(a.application_name, '.*/', '') AS who, l.mode, l.granted
  FROM pg_locks l JOIN pg_stat_activity a USING (pid)
  WHERE l.locktype = 'relation' AND l.relation = 'm_t'::regclass
  ORDER BY 1, 2;
}

permutation s1b s1sel s2alter s3sel s4ins s5locks s1sleep s1c s2alter s2cols
permutation s1b s1sel s2alter s4upd s1sleep s3sel s1c s2alter s2cols
