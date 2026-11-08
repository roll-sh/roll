-- Input failures and default side effects, recorded on PostgreSQL 18.6.
-- INSERT SELECT follows table-column order. Multirow VALUES coerces the VALUES row first.
CREATE DOMAIN a182_pos AS int CHECK (VALUE > 0);
CREATE TABLE a182_d (x serial, v a182_pos);
INSERT INTO a182_d (v) SELECT g FROM generate_series(1, 3) g;
INSERT INTO a182_d (v) SELECT CASE WHEN g = 3 THEN -1 ELSE g END FROM generate_series(1, 5) g;
SELECT last_value, is_called FROM a182_d_x_seq;
INSERT INTO a182_d (v) VALUES (5), (-5);
SELECT last_value, is_called FROM a182_d_x_seq;
CREATE TABLE a182_e (x serial, v int, w int GENERATED ALWAYS AS IDENTITY);
COPY a182_e (v) FROM STDIN (ON_ERROR ignore);
1
2
z
4
\.
SELECT * FROM a182_e ORDER BY x;
SELECT last_value, is_called FROM a182_e_x_seq;
COPY a182_e (v) FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 5);
5
y
6
\.
SELECT * FROM a182_e ORDER BY x;
DROP TABLE a182_d, a182_e;
DROP DOMAIN a182_pos;
CREATE DOMAIN a182_pos AS int CHECK (VALUE > 0);
CREATE TABLE a182_d (v a182_pos, x serial);
INSERT INTO a182_d (v) SELECT CASE WHEN g = 3 THEN -1 ELSE g END FROM generate_series(1, 5) g;
SELECT last_value, is_called FROM a182_d_x_seq;
CREATE TABLE a182_f (x serial, v a182_pos);
INSERT INTO a182_f (v) SELECT CASE WHEN g = 3 THEN -1 ELSE g END FROM generate_series(1, 5) g;
SELECT last_value, is_called FROM a182_f_x_seq;
CREATE TABLE a182_g (x serial, v a182_pos);
CREATE TABLE a182_src (g int);
INSERT INTO a182_src SELECT generate_series(1, 5);
INSERT INTO a182_g (v) SELECT CASE WHEN g = 3 THEN -1 ELSE g END FROM a182_src;
SELECT last_value, is_called FROM a182_g_x_seq;
INSERT INTO a182_g (v) SELECT g::a182_pos FROM a182_src;
SELECT last_value, is_called FROM a182_g_x_seq;
DROP TABLE a182_d, a182_f, a182_g, a182_src;
DROP DOMAIN a182_pos;

-- An ignored conversion still consumes both omitted sequence defaults, including identity.
CREATE TABLE a182_progress (processed bigint, excluded bigint, skipped bigint);
CREATE FUNCTION a182_progress() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
  INSERT INTO a182_progress SELECT tuples_processed, tuples_excluded, tuples_skipped FROM pg_stat_progress_copy WHERE pid = pg_backend_pid();
  RETURN NULL;
END $$;
CREATE TABLE a182_i (a serial, b int, c int GENERATED ALWAYS AS IDENTITY);
CREATE TRIGGER a182_progress AFTER INSERT ON a182_i FOR EACH STATEMENT EXECUTE FUNCTION a182_progress();
COPY a182_i (b) FROM STDIN (ON_ERROR ignore) WHERE b > 0;
1
bad
-2
3
\.
SELECT * FROM a182_i ORDER BY a;
SELECT * FROM a182_progress;
TRUNCATE a182_progress;
SELECT last_value, is_called FROM a182_i_a_seq;
SELECT last_value, is_called FROM a182_i_c_seq;
-- A BEFORE trigger can observe the nextval calls on earlier ignored rows.
CREATE FUNCTION a182_before() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER a182_before BEFORE INSERT ON a182_i FOR EACH ROW EXECUTE FUNCTION a182_before();
COPY a182_i (b) FROM STDIN (ON_ERROR ignore);
bad
4
\.
SELECT * FROM a182_i ORDER BY a;
-- REJECT_LIMIT fails after computing the last skipped row's defaults too.
COPY a182_i (b) FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 1);
bad
bad
5
\.
SELECT last_value, is_called FROM a182_i_a_seq;
SELECT last_value, is_called FROM a182_i_c_seq;
DROP TABLE a182_i;
DROP FUNCTION a182_before();
-- Defaults provided by markers run before a later conversion fails, in COPY column order.
CREATE SEQUENCE a182_mark_seq;
CREATE TABLE a182_m (a int DEFAULT nextval('a182_mark_seq'), b int, c serial);
COPY a182_m (a, b) FROM STDIN (DEFAULT 'D', ON_ERROR ignore);
D	1
D	bad
D	2
\.
SELECT * FROM a182_m ORDER BY a;
SELECT last_value, is_called FROM a182_mark_seq;
SELECT last_value, is_called FROM a182_m_c_seq;
DROP TABLE a182_m;
DROP SEQUENCE a182_mark_seq;
-- View COPY's command tag counts accepted input even if INSTEAD OF returns NULL.
CREATE SEQUENCE a182_view_seq;
CREATE VIEW a182_v AS SELECT 1 AS a, 2 AS b;
ALTER VIEW a182_v ALTER COLUMN a SET DEFAULT nextval('a182_view_seq');
CREATE FUNCTION a182_instead() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER a182_instead INSTEAD OF INSERT ON a182_v FOR EACH ROW EXECUTE FUNCTION a182_instead();
TRUNCATE a182_progress;
CREATE TRIGGER a182_progress AFTER INSERT ON a182_v FOR EACH STATEMENT EXECUTE FUNCTION a182_progress();
COPY a182_v (b) FROM STDIN (ON_ERROR ignore);
1
bad
2
\.
SELECT last_value, is_called FROM a182_view_seq;
SELECT * FROM a182_progress;
DROP VIEW a182_v;
DROP FUNCTION a182_instead();
DROP SEQUENCE a182_view_seq;
DROP TABLE a182_progress;
DROP FUNCTION a182_progress();
-- Domain input errors consume omitted defaults in skipped rows too.
CREATE DOMAIN a182_pos AS int CHECK (VALUE > 0);
CREATE TABLE a182_domain (a serial, b a182_pos);
COPY a182_domain (b) FROM STDIN (ON_ERROR ignore);
1
-1
2
\.
SELECT * FROM a182_domain ORDER BY a;
SELECT last_value, is_called FROM a182_domain_a_seq;
COPY a182_domain (b) FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 1);
-1
-2
3
\.
SELECT last_value, is_called FROM a182_domain_a_seq;
DROP TABLE a182_domain;
DROP DOMAIN a182_pos;
