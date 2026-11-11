-- A PL/pgSQL body is scanned under the standard_conforming_strings of when it is compiled:
-- a DO block's each time it runs, a function's at its first call in the session. The same
-- body compiled before under another setting (another DO block, another function) doesn't
-- carry its scan over.
SET standard_conforming_strings = off;
DO $$ BEGIN RAISE NOTICE 'a\\b'; END $$;
SET standard_conforming_strings = on;
DO $$ BEGIN RAISE NOTICE 'a\\b'; END $$;
DO $$ BEGIN RAISE NOTICE 'c\\d %', 'e\\f'; END $$;
SET standard_conforming_strings = off;
DO $$ BEGIN RAISE NOTICE 'c\\d %', 'e\\f'; END $$;
-- Functions with one body: each compiled at its own first call.
SET standard_conforming_strings = on;
CREATE FUNCTION f1() RETURNS text LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'g\\h'; RETURN 'i\\j'; END $$;
CREATE FUNCTION f2() RETURNS text LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'g\\h'; RETURN 'i\\j'; END $$;
SELECT f1();
SET standard_conforming_strings = off;
SELECT f1();
SELECT f2();
SET standard_conforming_strings = on;
SELECT f2();
RESET standard_conforming_strings;
