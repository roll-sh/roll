-- pg_stat_progress_create_index, read by the build's own backend: an index expression
-- calling a function that looks at the view while the table is scanned.
CREATE FUNCTION a148_peek(int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
  IF $1 = 1 THEN
    RAISE NOTICE 'progress: %', (SELECT row(p.command, p.phase, p.relid = 'public.a148_pi'::regclass,
        p.index_relid <> 0, p.blocks_total > 0,
        p.blocks_done, p.tuples_total, p.tuples_done, p.lockers_total, p.partitions_total)
      FROM pg_stat_progress_create_index p WHERE p.pid = pg_backend_pid());
  END IF;
  RETURN $1;
END $$;
CREATE TABLE a148_pi (a int, b text);
INSERT INTO a148_pi SELECT g, 'v' || g FROM generate_series(1, 2000) g;
CREATE INDEX a148_pi_x ON a148_pi (a148_peek(a));
REINDEX INDEX a148_pi_x;
REINDEX TABLE a148_pi;
CREATE INDEX a148_pi_h ON a148_pi USING hash (a148_peek(a));
DROP INDEX a148_pi_h;
DROP INDEX a148_pi_x;
CREATE INDEX a148_pi_x ON a148_pi (a148_peek(a)) WHERE b <> 'v7';
DROP INDEX a148_pi_x;
ALTER TABLE a148_pi ADD CONSTRAINT a148_pi_x UNIQUE (a, b);
CREATE UNIQUE INDEX a148_pi_x2 ON a148_pi ((a148_peek(a)));
-- Nothing reported once the build is over.
SELECT count(*) FROM pg_stat_progress_create_index WHERE pid = pg_backend_pid();
-- CLUSTER's and VACUUM FULL's rebuilds report no CREATE INDEX progress.
CREATE INDEX a148_pi_x3 ON a148_pi (a148_peek(a));
CLUSTER a148_pi USING a148_pi_x;
VACUUM FULL a148_pi;
DROP TABLE a148_pi;
DROP FUNCTION a148_peek(int);
SELECT pg_indexam_progress_phasename(403, 1), pg_indexam_progress_phasename(403, 2),
  pg_indexam_progress_phasename(403, 5), pg_indexam_progress_phasename(403, 6) IS NULL,
  pg_indexam_progress_phasename(405, 1) IS NULL, pg_indexam_progress_phasename(783, 2) IS NULL;
