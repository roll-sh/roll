-- pg_control_* return one composite value (proretset false), so they are allowed where sets
-- aren't; PL/pgSQL's RETURN QUERY names no column when the result is a single OUT column.
SELECT pg_control_version, catalog_version_no FROM pg_control_system();
SELECT (pg_control_system()).pg_control_version;
SELECT count(*) FROM (VALUES (pg_control_system())) v (c);
SELECT (c).catalog_version_no FROM (VALUES (pg_control_system())) v (c);
SELECT CASE WHEN true THEN (pg_control_init()).database_block_size END;
SELECT database_block_size, max_identifier_length, max_index_columns, float8_pass_by_value FROM pg_control_init();
SELECT (pg_control_recovery()).min_recovery_end_timeline, (pg_control_recovery()).end_of_backup_record_required;
SELECT timeline_id, full_page_writes FROM pg_control_checkpoint();
SELECT pg_typeof(pg_control_system());
SELECT 1 WHERE (pg_control_init()).wal_block_size = 8192;
SELECT count(*) FROM pg_control_init() WHERE max_data_alignment = 8;
SELECT proname, proretset, prorows FROM pg_proc WHERE proname LIKE 'pg\_control\_%' ORDER BY 1;

CREATE FUNCTION a156_t1() RETURNS TABLE (x int) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 'a'::text; END $$;
SELECT * FROM a156_t1();
SELECT a156_t1();
SELECT x FROM a156_t1() r (x);
CREATE FUNCTION a156_t2() RETURNS TABLE (x int, y text) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1, 2; END $$;
SELECT * FROM a156_t2();
CREATE FUNCTION a156_t3(OUT x int) RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 'a'::text; END $$;
SELECT * FROM a156_t3();
CREATE FUNCTION a156_t4(OUT x int, OUT y text) RETURNS SETOF record LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1, 2; END $$;
SELECT * FROM a156_t4();
CREATE FUNCTION a156_t5() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT 1, 2; END $$;
SELECT * FROM a156_t5();
DROP FUNCTION a156_t1(), a156_t2(), a156_t3(), a156_t4(), a156_t5();
