CREATE TABLE tr (id int PRIMARY KEY, v text, n int DEFAULT 0);
CREATE TABLE tr_log (seq serial, what text);
CREATE FUNCTION tr_log_fn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO tr_log (what) VALUES (format('%s %s %s %s on %s.%s nargs=%s args=%s', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME, TG_NARGS, TG_ARGV));
  IF TG_LEVEL = 'ROW' THEN
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER a_bs BEFORE INSERT OR UPDATE OR DELETE ON tr FOR EACH STATEMENT EXECUTE FUNCTION tr_log_fn('x', 'y');
CREATE TRIGGER b_br BEFORE INSERT OR UPDATE OR DELETE ON tr FOR EACH ROW EXECUTE FUNCTION tr_log_fn();
CREATE TRIGGER c_ar AFTER INSERT OR UPDATE OR DELETE ON tr FOR EACH ROW EXECUTE FUNCTION tr_log_fn(1);
CREATE TRIGGER d_as AFTER INSERT OR UPDATE OR DELETE ON tr FOR EACH STATEMENT EXECUTE FUNCTION tr_log_fn();
INSERT INTO tr VALUES (1, 'a'), (2, 'b');
UPDATE tr SET v = v || '!' WHERE id = 1;
DELETE FROM tr WHERE id = 2;
UPDATE tr SET v = 'none' WHERE id = 99;
SELECT what FROM tr_log ORDER BY seq;
TRUNCATE tr_log;
DROP TRIGGER a_bs ON tr;
DROP TRIGGER b_br ON tr;
DROP TRIGGER c_ar ON tr;
DROP TRIGGER d_as ON tr;
-- BEFORE ROW modifies and skips
CREATE FUNCTION tr_mod() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.v = 'skip' THEN RETURN NULL; END IF;
  NEW.v := upper(NEW.v);
  NEW.n := NEW.n + 1;
  RETURN NEW;
END $$;
CREATE TRIGGER tr_mod BEFORE INSERT OR UPDATE ON tr FOR EACH ROW EXECUTE FUNCTION tr_mod();
INSERT INTO tr VALUES (10, 'x'), (11, 'skip'), (12, 'y') RETURNING *;
UPDATE tr SET v = 'skip' WHERE id = 10;
UPDATE tr SET v = 'z' WHERE id = 12 RETURNING *;
SELECT * FROM tr ORDER BY id;
DROP TRIGGER tr_mod ON tr;
-- WHEN and UPDATE OF
CREATE FUNCTION tr_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% fired: old=% new=%', TG_NAME, OLD.v, NEW.v;
  RETURN NEW;
END $$;
CREATE TRIGGER tr_when BEFORE UPDATE ON tr FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION tr_note();
CREATE TRIGGER tr_of AFTER UPDATE OF n ON tr FOR EACH ROW EXECUTE FUNCTION tr_note();
UPDATE tr SET v = v WHERE id = 10;
UPDATE tr SET v = 'changed' WHERE id = 10;
UPDATE tr SET n = 5 WHERE id = 10;
CREATE TRIGGER tr_bad BEFORE INSERT ON tr FOR EACH ROW WHEN (OLD.v = 'x') EXECUTE FUNCTION tr_note();
CREATE TRIGGER tr_bad2 BEFORE INSERT ON tr FOR EACH STATEMENT WHEN (NEW.v = 'x') EXECUTE FUNCTION tr_note();
SELECT tgname, pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = 'tr'::regclass ORDER BY tgname;
SELECT tgname, tgtype, tgenabled, tgnargs FROM pg_trigger WHERE tgrelid = 'tr'::regclass ORDER BY tgname;
SELECT trigger_name, event_manipulation, action_timing, action_orientation, action_condition, action_statement
  FROM information_schema.triggers WHERE event_object_table = 'tr' ORDER BY trigger_name, event_manipulation;
-- disable / enable
ALTER TABLE tr DISABLE TRIGGER tr_when;
UPDATE tr SET v = 'quiet' WHERE id = 10;
ALTER TABLE tr ENABLE TRIGGER tr_when;
ALTER TABLE tr DISABLE TRIGGER ALL;
UPDATE tr SET v = 'quiet2', n = 1 WHERE id = 10;
ALTER TABLE tr ENABLE TRIGGER ALL;
SELECT tgname, tgenabled FROM pg_trigger WHERE tgrelid = 'tr'::regclass ORDER BY tgname;
ALTER TRIGGER tr_of ON tr RENAME TO tr_of2;
DROP TRIGGER tr_when ON tr;
DROP TRIGGER tr_of2 ON tr;
DROP TRIGGER IF EXISTS nope ON tr;
DROP TRIGGER nope ON tr;
-- errors in triggers
CREATE FUNCTION tr_fail() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'nope for %', NEW.id USING HINT = 'trigger said no';
END $$;
CREATE TRIGGER tr_fail BEFORE INSERT ON tr FOR EACH ROW EXECUTE FUNCTION tr_fail();
INSERT INTO tr VALUES (20, 'q');
DROP TRIGGER tr_fail ON tr;
CREATE FUNCTION not_trig() RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE TRIGGER t_nt BEFORE INSERT ON tr FOR EACH ROW EXECUTE FUNCTION not_trig();
SELECT tr_mod();
-- AFTER triggers see the final state; statement triggers fire once even with no rows
CREATE FUNCTION tr_count() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% %: % rows in tr', TG_WHEN, TG_OP, (SELECT count(*) FROM tr);
  RETURN NULL;
END $$;
CREATE TRIGGER tr_cnt_b BEFORE INSERT ON tr FOR EACH STATEMENT EXECUTE FUNCTION tr_count();
CREATE TRIGGER tr_cnt_a AFTER INSERT ON tr FOR EACH STATEMENT EXECUTE FUNCTION tr_count();
INSERT INTO tr VALUES (30, 'a'), (31, 'b');
INSERT INTO tr SELECT * FROM tr WHERE false;
DROP TRIGGER tr_cnt_b ON tr;
DROP TRIGGER tr_cnt_a ON tr;
-- TRUNCATE triggers
CREATE TRIGGER tr_trunc BEFORE TRUNCATE ON tr FOR EACH STATEMENT EXECUTE FUNCTION tr_count();
CREATE TRIGGER tr_trunc_row BEFORE TRUNCATE ON tr FOR EACH ROW EXECUTE FUNCTION tr_count();
TRUNCATE tr;
DROP TRIGGER tr_trunc ON tr;
-- triggers on INSERT ... ON CONFLICT
CREATE TRIGGER c_ar AFTER INSERT OR UPDATE ON tr FOR EACH ROW EXECUTE FUNCTION tr_log_fn();
INSERT INTO tr VALUES (40, 'a');
INSERT INTO tr VALUES (40, 'b') ON CONFLICT (id) DO UPDATE SET v = excluded.v;
INSERT INTO tr VALUES (40, 'c') ON CONFLICT DO NOTHING;
SELECT what FROM tr_log ORDER BY seq;
DROP TRIGGER c_ar ON tr;
-- recursion via nested triggers
CREATE TABLE tr_parent (id int PRIMARY KEY, total int DEFAULT 0);
CREATE TABLE tr_child (pid int REFERENCES tr_parent ON DELETE CASCADE, amt int);
CREATE FUNCTION tr_sum() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    UPDATE tr_parent SET total = total - OLD.amt WHERE id = OLD.pid;
    RETURN OLD;
  END IF;
  UPDATE tr_parent SET total = total + NEW.amt WHERE id = NEW.pid;
  RETURN NEW;
END $$;
CREATE TRIGGER tr_sum AFTER INSERT OR DELETE ON tr_child FOR EACH ROW EXECUTE FUNCTION tr_sum();
INSERT INTO tr_parent VALUES (1), (2);
INSERT INTO tr_child VALUES (1, 5), (1, 7), (2, 1);
SELECT * FROM tr_parent ORDER BY id;
DELETE FROM tr_child WHERE amt = 7;
SELECT * FROM tr_parent ORDER BY id;
-- FK cascades fire triggers on the referencing table
CREATE FUNCTION tr_del_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'deleting child % %', OLD.pid, OLD.amt;
  RETURN OLD;
END $$;
CREATE TRIGGER tr_del_note BEFORE DELETE ON tr_child FOR EACH ROW EXECUTE FUNCTION tr_del_note();
DELETE FROM tr_parent WHERE id = 1;
SELECT * FROM tr_child ORDER BY pid;
-- transition tables
CREATE FUNCTION tr_trans() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    FOR r IN SELECT * FROM old_rows ORDER BY 1 LOOP RAISE NOTICE 'old: %', r; END LOOP;
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    FOR r IN SELECT * FROM new_rows ORDER BY 1 LOOP RAISE NOTICE 'new: %', r; END LOOP;
  END IF;
  RETURN NULL;
END $$;
CREATE TABLE tt (a int, b text);
CREATE TRIGGER tt_ins AFTER INSERT ON tt REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION tr_trans();
CREATE TRIGGER tt_upd AFTER UPDATE ON tt REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION tr_trans();
CREATE TRIGGER tt_del AFTER DELETE ON tt REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION tr_trans();
INSERT INTO tt VALUES (1, 'a'), (2, 'b');
UPDATE tt SET b = b || b;
DELETE FROM tt WHERE a = 1;
CREATE TRIGGER tt_bad BEFORE INSERT ON tt REFERENCING NEW TABLE AS n FOR EACH STATEMENT EXECUTE FUNCTION tr_trans();
SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'tt_upd';
-- INSTEAD OF triggers on views
CREATE VIEW trv AS SELECT a, b FROM tt;
CREATE FUNCTION trv_ins() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO tt VALUES (NEW.a * 10, NEW.b);
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    UPDATE tt SET b = NEW.b WHERE a = OLD.a;
    RETURN NEW;
  ELSE
    DELETE FROM tt WHERE a = OLD.a;
    RETURN OLD;
  END IF;
END $$;
CREATE TRIGGER trv_io INSTEAD OF INSERT OR UPDATE OR DELETE ON trv FOR EACH ROW EXECUTE FUNCTION trv_ins();
DROP TRIGGER tt_ins ON tt;
DROP TRIGGER tt_upd ON tt;
DROP TRIGGER tt_del ON tt;
INSERT INTO trv VALUES (3, 'c') RETURNING *;
UPDATE trv SET b = 'upd' WHERE a = 30;
SELECT * FROM tt ORDER BY a;
DELETE FROM trv WHERE a = 2;
SELECT * FROM trv ORDER BY a;
CREATE TRIGGER trv_bad BEFORE INSERT ON trv FOR EACH ROW EXECUTE FUNCTION trv_ins();
CREATE TRIGGER tt_bad INSTEAD OF INSERT ON tt FOR EACH ROW EXECUTE FUNCTION trv_ins();
-- NEW.* and row values
CREATE FUNCTION tr_star() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'row: % / %', NEW, ROW(NEW.*);
  RETURN NEW;
END $$;
CREATE TRIGGER tr_star BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION tr_star();
INSERT INTO tt VALUES (50, 'star');
DROP TABLE tt CASCADE;
DROP TABLE tr, tr_log, tr_parent, tr_child;
DROP FUNCTION tr_log_fn(), tr_mod(), tr_note(), tr_fail(), not_trig(), tr_count(), tr_sum(), tr_del_note(), tr_trans(), trv_ins(), tr_star();
