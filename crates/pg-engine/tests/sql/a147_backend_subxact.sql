-- pg_stat_get_backend_subxact returns one row of NULL fields for an invalid backend id.
SELECT * FROM pg_stat_get_backend_subxact(NULL);
SELECT pg_stat_get_backend_subxact(0);
SELECT * FROM pg_stat_get_backend_subxact(-1);
-- PID-addressed cumulative functions return a NULL WAL record or no I/O rows for a missing process.
SELECT * FROM pg_stat_get_backend_wal(-1);
SELECT count(*) FROM pg_stat_get_backend_io(-1);
