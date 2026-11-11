-- PL/pgSQL OPEN starts the query; rows (and their side effects and errors) come at FETCH
CREATE FUNCTION lz_f(i int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'f(%)', i;
  RETURN 10 / (3 - i);
END $$;
CREATE FUNCTION lz_g() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT lz_f(i) AS v FROM generate_series(1, 5) i;
  r record;
BEGIN
  OPEN c;
  RAISE NOTICE 'opened';
  FETCH c INTO r;
  RAISE NOTICE 'got %', r.v;
  FETCH c INTO r;
  RAISE NOTICE 'got %', r.v;
  BEGIN
    FETCH c INTO r;
  EXCEPTION WHEN division_by_zero THEN
    RAISE NOTICE 'caught %', sqlerrm;
  END;
  BEGIN
    FETCH c INTO r;
    RAISE NOTICE 'got %', r.v;
  EXCEPTION WHEN others THEN
    RAISE NOTICE 'caught2 % %', sqlstate, sqlerrm;
  END;
END $$;
SELECT lz_g();
-- the cursor reads the data as of OPEN
CREATE TABLE lz_t (a int PRIMARY KEY, b text);
INSERT INTO lz_t VALUES (1, 'x'), (2, 'y'), (3, 'z');
CREATE FUNCTION lz_h() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  c refcursor;
  r record;
BEGIN
  OPEN c FOR SELECT a, b, (SELECT count(*) FROM lz_t) AS n FROM lz_t ORDER BY a;
  INSERT INTO lz_t VALUES (4, 'w');
  UPDATE lz_t SET b = 'changed' WHERE a = 2;
  DELETE FROM lz_t WHERE a = 3;
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
  INSERT INTO lz_t VALUES (5, 'v');
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
  FETCH c INTO r;
  RAISE NOTICE 'found=%', found;
  CLOSE c;
  OPEN c FOR SELECT a, b FROM lz_t WHERE a > 1 ORDER BY a;
  DELETE FROM lz_t WHERE a > 1;
  LOOP
    FETCH c INTO r;
    EXIT WHEN NOT found;
    RAISE NOTICE 'second %', r;
  END LOOP;
END $$;
SELECT lz_h();
SELECT * FROM lz_t ORDER BY a;
-- writes undone by an exception block between fetches
INSERT INTO lz_t VALUES (2, 'y'), (3, 'z');
CREATE FUNCTION lz_s() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT a, b FROM lz_t ORDER BY a;
  r record;
BEGIN
  OPEN c;
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
  BEGIN
    UPDATE lz_t SET b = 'gone' WHERE a = 2;
    DELETE FROM lz_t WHERE a = 3;
    RAISE EXCEPTION 'undo';
  EXCEPTION WHEN raise_exception THEN
    RAISE NOTICE 'undone';
  END;
  UPDATE lz_t SET b = 'later' WHERE a = 3;
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
  FETCH c INTO r;
  RAISE NOTICE 'r=%', r;
END $$;
SELECT lz_s();
SELECT * FROM lz_t ORDER BY a;
-- a FOR loop over a cursor doesn't see the rows its body changes
CREATE FUNCTION lz_k() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT a FROM lz_t ORDER BY a;
BEGIN
  FOR r IN c LOOP
    RAISE NOTICE 'loop %', r.a;
    UPDATE lz_t SET a = a * 10 WHERE a = r.a + 1;
    INSERT INTO lz_t VALUES (r.a + 100, 'new');
  END LOOP;
END $$;
SELECT lz_k();
SELECT a FROM lz_t ORDER BY a;
-- a cursor with arguments
CREATE FUNCTION lz_p(lim int) RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR (x int) FOR SELECT lz_f(i) FROM generate_series(0, x) i;
  v int;
BEGIN
  OPEN c(lim);
  lim := 0;
  LOOP
    FETCH c INTO v;
    EXIT WHEN NOT found;
    RETURN NEXT v;
  END LOOP;
  CLOSE c;
END $$;
SELECT * FROM lz_p(2);
SELECT * FROM lz_p(4);
-- a refcursor returned to SQL: FETCH runs it
CREATE FUNCTION lz_rc() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'lz_cur';
BEGIN
  OPEN c FOR SELECT lz_f(i) AS v, (SELECT count(*) FROM lz_t) AS n FROM generate_series(0, 4) i;
  RAISE NOTICE 'opened rc';
  RETURN c;
END $$;
BEGIN;
SELECT lz_rc();
INSERT INTO lz_t VALUES (1000, 'after');
FETCH 2 FROM lz_cur;
FETCH 1 FROM lz_cur;
SELECT name, statement FROM pg_cursors;
FETCH 1 FROM lz_cur;
ROLLBACK;
BEGIN;
SELECT lz_rc();
MOVE FORWARD 2 IN lz_cur;
CLOSE lz_cur;
COMMIT;
BEGIN;
SELECT lz_rc();
MOVE FORWARD ALL IN lz_cur;
ROLLBACK;
-- a cursor left open (not fetched) never runs its query
DO $$
DECLARE c CURSOR FOR SELECT lz_f(i) FROM generate_series(1, 9) i;
BEGIN
  OPEN c;
  RAISE NOTICE 'not fetched';
END $$;
