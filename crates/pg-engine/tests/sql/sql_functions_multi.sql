-- SQL functions and procedures with several statements
CREATE TABLE sm_log (id serial, msg text);
CREATE FUNCTION sm_add(m text) RETURNS bigint LANGUAGE sql AS $$
  INSERT INTO sm_log (msg) VALUES (m);
  INSERT INTO sm_log (msg) VALUES (m || '!');
  SELECT count(*) FROM sm_log;
$$;
SELECT sm_add('a');
SELECT sm_add('b');
SELECT id, msg FROM sm_log ORDER BY id;
CREATE FUNCTION sm_set(n int) RETURNS SETOF sm_log LANGUAGE sql AS $$
  DELETE FROM sm_log WHERE id > n;
  SELECT * FROM sm_log ORDER BY id;
$$;
SELECT * FROM sm_set(3);
CREATE FUNCTION sm_void() RETURNS void LANGUAGE sql AS $$
  UPDATE sm_log SET msg = upper(msg);
  DELETE FROM sm_log WHERE id = 1;
$$;
SELECT sm_void();
SELECT id, msg FROM sm_log ORDER BY id;
CREATE FUNCTION sm_atomic(x int) RETURNS int LANGUAGE sql
BEGIN ATOMIC
  INSERT INTO sm_log (msg) VALUES ('atomic ' || x);
  SELECT x * 2;
END;
SELECT sm_atomic(21);
SELECT msg FROM sm_log WHERE msg LIKE 'atomic%';
CREATE FUNCTION sm_empty_last() RETURNS int LANGUAGE sql AS $$
  SELECT 1;
  SELECT 2 WHERE false;
$$;
SELECT sm_empty_last() IS NULL;
CREATE FUNCTION sm_out(a int, OUT d int, OUT s text) LANGUAGE sql AS $$
  SELECT 0, 'ignored';
  SELECT a * 2, 'two';
$$;
SELECT * FROM sm_out(5);
CREATE PROCEDURE sm_proc(n int) LANGUAGE sql AS $$
  INSERT INTO sm_log (msg) VALUES ('proc ' || n);
  INSERT INTO sm_log (msg) VALUES ('proc ' || n + 1);
$$;
CALL sm_proc(10);
SELECT msg FROM sm_log WHERE msg LIKE 'proc%' ORDER BY id;
CREATE FUNCTION sm_strict(a int) RETURNS int LANGUAGE sql STRICT AS $$
  INSERT INTO sm_log (msg) VALUES ('strict');
  SELECT a;
$$;
SELECT sm_strict(NULL), sm_strict(4);
SELECT count(*) FROM sm_log WHERE msg = 'strict';
CREATE FUNCTION sm_bad() RETURNS int LANGUAGE sql AS $$
  SELECT 1;
  INSERT INTO sm_log (msg) VALUES ('x');
$$;
CREATE FUNCTION sm_commit() RETURNS int LANGUAGE sql AS $$
  SELECT 1;
  COMMIT;
$$;
SELECT prosrc FROM pg_proc WHERE proname = 'sm_add';
DROP FUNCTION sm_add(text), sm_set(int), sm_void(), sm_atomic(int), sm_empty_last(), sm_out(int), sm_strict(int);
DROP PROCEDURE sm_proc(int);
DROP TABLE sm_log;
