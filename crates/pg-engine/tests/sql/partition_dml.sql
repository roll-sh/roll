CREATE TABLE m (k int, v text) PARTITION BY LIST (k);
CREATE TABLE m1 PARTITION OF m FOR VALUES IN (1, 2);
CREATE TABLE m2 PARTITION OF m FOR VALUES IN (3, 4);
CREATE UNIQUE INDEX m_k ON m (k);
INSERT INTO m VALUES (1, 'a'), (3, 'c');
INSERT INTO m VALUES (1, 'z') ON CONFLICT (k) DO UPDATE SET k = 3;
INSERT INTO m VALUES (1, 'z') ON CONFLICT (k) DO UPDATE SET v = m.v || excluded.v WHERE m.v = 'a' RETURNING tableoid::regclass, *;
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE '% % % on %', TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME; IF TG_LEVEL = 'ROW' THEN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; RETURN NEW; END IF; RETURN NULL; END $$;
CREATE TRIGGER tb BEFORE INSERT OR UPDATE OR DELETE ON m FOR EACH ROW EXECUTE FUNCTION trg();
CREATE TRIGGER ta AFTER INSERT OR UPDATE OR DELETE ON m FOR EACH ROW EXECUTE FUNCTION trg();
CREATE TRIGGER ts AFTER INSERT ON m FOR EACH STATEMENT EXECUTE FUNCTION trg();
SELECT tgname, tgrelid::regclass, tgparentid <> 0 AS child FROM pg_trigger WHERE tgrelid IN ('m'::regclass, 'm1'::regclass, 'm2'::regclass) ORDER BY 2, 1;
CREATE TABLE m3 PARTITION OF m FOR VALUES IN (5, 6);
SELECT tgname, tgrelid::regclass FROM pg_trigger WHERE tgrelid = 'm3'::regclass ORDER BY 1;
INSERT INTO m VALUES (2, 'b'), (5, 'e');
UPDATE m SET k = 4 WHERE k = 2;
DELETE FROM m WHERE k = 5;
DROP TRIGGER ta ON m1;
DROP TRIGGER ta ON m;
SELECT tgname, tgrelid::regclass FROM pg_trigger WHERE tgrelid IN ('m'::regclass, 'm1'::regclass, 'm3'::regclass) ORDER BY 2, 1;
MERGE INTO m USING (VALUES (1, 'q'), (6, 'f'), (3, 'r')) s(k, v) ON m.k = s.k
  WHEN MATCHED AND s.k = 3 THEN DELETE
  WHEN MATCHED THEN UPDATE SET v = s.v
  WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v);
SELECT tableoid::regclass, * FROM m ORDER BY k;
MERGE INTO m USING (VALUES (1)) s(k) ON m.k = s.k WHEN MATCHED THEN UPDATE SET k = 6;
SELECT tableoid::regclass, * FROM m ORDER BY k;
ALTER TABLE m1 ADD COLUMN extra int;
ALTER TABLE m ADD COLUMN extra int;
SELECT attname FROM pg_attribute WHERE attrelid = 'm2'::regclass AND attnum > 0 ORDER BY attnum;
ALTER TABLE m ALTER COLUMN v SET NOT NULL;
SELECT attnotnull FROM pg_attribute WHERE attrelid = 'm2'::regclass AND attname = 'v';
ALTER TABLE m1 ALTER COLUMN v DROP NOT NULL;
DROP TABLE m;
DROP FUNCTION trg();
