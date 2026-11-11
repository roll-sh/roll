-- A trigger's WHEN over the whole row (`OLD.* IS DISTINCT FROM NEW.*`) depends on no column:
-- dropping one doesn't need CASCADE, and the condition then compares what is left (difftest
-- seed 5081 #58).
CREATE TABLE a104_tw (id int, a int, b int, c text);
CREATE TABLE a104_twlog (tg text, v text);
CREATE FUNCTION a104_twf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO a104_twlog VALUES (TG_NAME, NEW::text); RETURN NEW; END $$;
CREATE TRIGGER a104_tw1 AFTER UPDATE ON a104_tw FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION a104_twf();
CREATE TRIGGER a104_tw2 AFTER UPDATE ON a104_tw FOR EACH ROW WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION a104_twf();
CREATE TRIGGER a104_tw3 AFTER UPDATE OF b ON a104_tw FOR EACH ROW EXECUTE FUNCTION a104_twf();
CREATE TRIGGER a104_tw4 BEFORE UPDATE ON a104_tw FOR EACH ROW WHEN (NEW.* IS NOT DISTINCT FROM OLD.* AND OLD.c IS NOT NULL) EXECUTE FUNCTION a104_twf();
SELECT tgname, objsubid, refobjsubid, deptype FROM pg_depend d JOIN pg_trigger t ON t.oid = d.objid WHERE d.classid = 'pg_trigger'::regclass AND t.tgrelid = 'a104_tw'::regclass AND d.refclassid = 'pg_class'::regclass ORDER BY 1, 2, 3, 4;
INSERT INTO a104_tw VALUES (1, 1, 1, 'x'), (2, 2, 2, NULL);
ALTER TABLE a104_tw DROP COLUMN a;
ALTER TABLE a104_tw DROP COLUMN c;
ALTER TABLE a104_tw DROP COLUMN b;
SELECT tgname, pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = 'a104_tw'::regclass ORDER BY 1;
UPDATE a104_tw SET b = b + 1 WHERE id = 1;
UPDATE a104_tw SET b = b WHERE id = 2;
SELECT * FROM a104_twlog ORDER BY 1, 2;
DROP TABLE a104_tw, a104_twlog;
DROP FUNCTION a104_twf();
