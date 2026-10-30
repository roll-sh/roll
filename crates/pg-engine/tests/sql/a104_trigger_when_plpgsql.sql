-- A trigger's WHEN condition binds apart from the statement firing it: run from a PL/pgSQL
-- trigger function, whose OLD / NEW are variables, `OLD.a` isn't ambiguous (difftest seeds
-- 5009, 5018, 5019: INSERT through a view's INSTEAD OF trigger).
CREATE TABLE a104_w (id int PRIMARY KEY, a int, g int GENERATED ALWAYS AS (a * 2) STORED);
CREATE TABLE a104_wlog (tg text, k int, v text);
CREATE FUNCTION a104_wl() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO a104_wlog VALUES (TG_NAME, NEW.id, OLD.a || '>' || NEW.a || '/' || NEW.g); RETURN NEW; END $$;
CREATE TRIGGER a104_wu AFTER UPDATE ON a104_w FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION a104_wl();
CREATE TRIGGER a104_wg BEFORE UPDATE ON a104_w FOR EACH ROW WHEN (NEW.a > OLD.a AND NEW.id = OLD.id) EXECUTE FUNCTION a104_wl();
CREATE VIEW a104_wv AS SELECT x.id, x.a, 'w' AS w FROM a104_w x;
CREATE FUNCTION a104_wt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO a104_w (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a;
    RETURN NEW;
  END IF;
  UPDATE a104_w SET a = NEW.a WHERE id = OLD.id;
  RETURN NEW;
END $$;
CREATE TRIGGER a104_wt INSTEAD OF INSERT OR UPDATE ON a104_wv FOR EACH ROW EXECUTE FUNCTION a104_wt();
INSERT INTO a104_wv VALUES (1, 5), (2, 7) RETURNING *;
INSERT INTO a104_wv VALUES (1, 6) RETURNING *;
UPDATE a104_wv SET a = a - 1 WHERE id = 2 RETURNING *;
UPDATE a104_wv SET a = a + 10 RETURNING *;
SELECT * FROM a104_w ORDER BY id;
SELECT * FROM a104_wlog ORDER BY tg, k, v;
DROP VIEW a104_wv;
DROP TABLE a104_w, a104_wlog;
DROP FUNCTION a104_wl(), a104_wt();
