-- Adding a virtual generated column (or giving one a new expression) evaluates nothing: an
-- expression failing for some row fails only the queries that read it (difftest seed 5046).
CREATE TABLE a104_v (id int PRIMARY KEY, a bigint);
INSERT INTO a104_v VALUES (1, 9), (3, 0), (4, 5);
ALTER TABLE a104_v ADD COLUMN e1 numeric GENERATED ALWAYS AS (a / (id - 3));
SELECT * FROM a104_v ORDER BY id;
SELECT id, e1 FROM a104_v WHERE id <> 3 ORDER BY id;
SELECT id, a FROM a104_v ORDER BY id;
ALTER TABLE a104_v ADD COLUMN e2 int GENERATED ALWAYS AS (a / (id - 3)) STORED;
ALTER TABLE a104_v ADD COLUMN e3 text GENERATED ALWAYS AS (id::text || 'x') VIRTUAL;
ALTER TABLE a104_v ALTER COLUMN e3 SET EXPRESSION AS ((10 / (id - 4))::text);
SELECT id, e3 FROM a104_v WHERE id < 4 ORDER BY id;
SELECT id, e3 FROM a104_v ORDER BY id;
ALTER TABLE a104_v ALTER COLUMN e1 TYPE int;
SELECT id, e1 FROM a104_v WHERE id = 1;
ALTER TABLE a104_v DROP COLUMN e1, DROP COLUMN e3;
SELECT * FROM a104_v ORDER BY id;
-- A row trigger's WHEN over the whole row sees the added virtual column on both rows
-- (difftest seed 5130): an UPDATE changing nothing doesn't fire it.
CREATE TABLE a104_vlog (k int);
CREATE FUNCTION a104_vf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO a104_vlog VALUES (NEW.id); RETURN NEW; END $$;
CREATE TRIGGER a104_vt AFTER UPDATE ON a104_v FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION a104_vf();
ALTER TABLE a104_v ADD COLUMN e4 bigint GENERATED ALWAYS AS (a * 2) VIRTUAL;
ALTER TABLE a104_v ADD COLUMN e5 bigint GENERATED ALWAYS AS (a / (id - 3)) VIRTUAL;
UPDATE a104_v SET a = a + 0 WHERE id <> 3;
UPDATE a104_v SET a = a + 1 WHERE id = 1;
SELECT * FROM a104_vlog ORDER BY k;
DROP TABLE a104_vlog;
DROP FUNCTION a104_vf() CASCADE;
DROP TABLE a104_v;
