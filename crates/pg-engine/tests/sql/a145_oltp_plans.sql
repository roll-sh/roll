-- Retained analyses, literal shapes and PL/pgSQL custom plans, recorded from PostgreSQL 18.6.
CREATE TABLE a145_t (id int PRIMARY KEY, v int NOT NULL DEFAULT 0, c int);
INSERT INTO a145_t SELECT g, 0, g % 3 FROM generate_series(1, 300) g;
ANALYZE a145_t;
CREATE FUNCTION a145_bump(k int, d int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int; n bigint; c int := 1;
BEGIN
  UPDATE a145_t SET v = v + d WHERE id = k;
  SELECT v INTO r FROM a145_t WHERE id = k;
  SELECT count(*) INTO n FROM a145_t WHERE a145_t.c = (k % 3);
  RETURN r + n::int;
END $$;
SELECT g, a145_bump(g, g) FROM generate_series(1, 12) g;
SET plan_cache_mode = force_custom_plan;
SELECT g, a145_bump(g, g) FROM generate_series(1, 12) g;
SELECT a145_bump(NULL, 1);
SELECT a145_bump(1, 2147483647);
SELECT a145_bump(1, 1);
RESET plan_cache_mode;
SELECT v FROM a145_t WHERE id = 1;
SELECT v FROM a145_t WHERE id = 22;
SELECT v FROM a145_t WHERE id = 300;
INSERT INTO a145_t SELECT g, g, 0 FROM generate_series(301, 10000) g;
SELECT v FROM a145_t WHERE id = 9999;
SELECT v FROM a145_t WHERE id = 7;
PREPARE a145_q(int) AS SELECT v FROM a145_t WHERE id = $1;
EXECUTE a145_q(7);
INSERT INTO a145_t SELECT g, g, 0 FROM generate_series(10001, 15000) g;
EXECUTE a145_q(14999);
ALTER TABLE a145_t ADD COLUMN added text;
EXECUTE a145_q(12);
DEALLOCATE a145_q;
DROP FUNCTION a145_bump(int, int);
DROP TABLE a145_t;
