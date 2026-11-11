-- which views and columns are updatable, and why not
CREATE TABLE uv_base (a int PRIMARY KEY, b text DEFAULT 'def', c int);
INSERT INTO uv_base VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30);
CREATE VIEW uv_simple AS SELECT a, b, c * 2 AS c2 FROM uv_base;
CREATE VIEW uv_distinct AS SELECT DISTINCT a FROM uv_base;
CREATE VIEW uv_agg AS SELECT count(*) AS n FROM uv_base;
CREATE VIEW uv_union AS SELECT a FROM uv_base UNION SELECT c FROM uv_base;
CREATE VIEW uv_const AS SELECT 1 AS one;
CREATE VIEW uv_sys AS SELECT a, ctid FROM uv_base;
SELECT table_name, is_insertable_into FROM information_schema.tables WHERE table_name LIKE 'uv_%' ORDER BY 1;
SELECT table_name, is_updatable, is_insertable_into, is_trigger_updatable FROM information_schema.views WHERE table_name LIKE 'uv_%' ORDER BY 1;
SELECT table_name, column_name, is_updatable FROM information_schema.columns WHERE table_name LIKE 'uv_%' ORDER BY 1, ordinal_position;
SELECT c.relname, pg_relation_is_updatable(c.oid, false), pg_column_is_updatable(c.oid, 1::int2, false) FROM pg_class c WHERE c.relname LIKE 'uv_%' AND c.relkind = 'v' ORDER BY 1;
INSERT INTO uv_distinct VALUES (5);
UPDATE uv_agg SET n = 1;
DELETE FROM uv_union;
INSERT INTO uv_const VALUES (1);
UPDATE uv_simple SET c2 = 1;
INSERT INTO uv_simple (a, c2) VALUES (9, 9);
UPDATE uv_sys SET ctid = '(0,1)';
INSERT INTO uv_simple (a, a) VALUES (9, 9);
-- writes through a view, defaults, RETURNING
INSERT INTO uv_simple (a) VALUES (4) RETURNING *;
ALTER VIEW uv_simple ALTER COLUMN b SET DEFAULT 'view default';
INSERT INTO uv_simple (a) VALUES (5) RETURNING *;
ALTER VIEW uv_simple ALTER COLUMN b DROP DEFAULT;
UPDATE uv_simple SET b = upper(b) WHERE a > 3 RETURNING a, b, c2;
DELETE FROM uv_simple WHERE a = 5 RETURNING *;
INSERT INTO uv_simple (a, b) VALUES (1, 'conflict') ON CONFLICT (a) DO UPDATE SET b = excluded.b || '!' RETURNING *;
INSERT INTO uv_simple (a, b) VALUES (1, 'skip') ON CONFLICT (a) DO NOTHING RETURNING *;
MERGE INTO uv_simple t USING (VALUES (2, 'merged'), (7, 'new')) s(a, b) ON t.a = s.a
  WHEN MATCHED THEN UPDATE SET b = s.b WHEN NOT MATCHED THEN INSERT (a, b) VALUES (s.a, s.b);
SELECT * FROM uv_base ORDER BY a;
-- check options
CREATE VIEW uv_pos AS SELECT * FROM uv_base WHERE c > 0 WITH LOCAL CHECK OPTION;
CREATE VIEW uv_small AS SELECT * FROM uv_pos WHERE c < 100;
CREATE VIEW uv_small_local AS SELECT * FROM uv_pos WHERE c < 100 WITH LOCAL CHECK OPTION;
CREATE VIEW uv_small_casc AS SELECT * FROM uv_pos WHERE c < 100 WITH CASCADED CHECK OPTION;
INSERT INTO uv_pos VALUES (10, 'neg', -1);
INSERT INTO uv_small VALUES (11, 'big', 500);
INSERT INTO uv_small VALUES (12, 'neg', -5);
INSERT INTO uv_small_local VALUES (13, 'big', 500);
INSERT INTO uv_small_casc VALUES (14, 'neg', -5);
UPDATE uv_small_casc SET c = 1000 WHERE a = 1;
SELECT table_name, check_option FROM information_schema.views WHERE table_name LIKE 'uv_pos%' OR table_name LIKE 'uv_small%' ORDER BY 1;
ALTER VIEW uv_small SET (check_option = cascaded);
INSERT INTO uv_small VALUES (15, 'big', 500);
CREATE VIEW uv_bad AS SELECT DISTINCT a FROM uv_base WITH CHECK OPTION;
-- INSTEAD OF triggers
CREATE FUNCTION uv_trig() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF tg_op = 'INSERT' THEN
    INSERT INTO uv_base VALUES (new.a, 'trig ' || new.a, 1);
    RETURN new;
  ELSIF tg_op = 'UPDATE' THEN
    UPDATE uv_base SET a = new.a WHERE a = old.a;
    RETURN new;
  ELSE
    DELETE FROM uv_base WHERE a = old.a;
    RETURN old;
  END IF;
END $$;
CREATE TRIGGER uv_t INSTEAD OF INSERT OR UPDATE OR DELETE ON uv_distinct FOR EACH ROW EXECUTE FUNCTION uv_trig();
SELECT table_name, is_updatable, is_insertable_into, is_trigger_updatable, is_trigger_deletable, is_trigger_insertable_into FROM information_schema.views WHERE table_name = 'uv_distinct';
SELECT pg_relation_is_updatable('uv_distinct'::regclass, true);
INSERT INTO uv_distinct VALUES (20) RETURNING *;
UPDATE uv_distinct SET a = 21 WHERE a = 20 RETURNING *;
DELETE FROM uv_distinct WHERE a = 21 RETURNING *;
-- views over views with rules
CREATE TABLE uv_log (op text, a int);
CREATE VIEW uv_rv1 AS SELECT a, b FROM uv_base WHERE a < 100;
CREATE RULE uv_rv1_ins AS ON INSERT TO uv_rv1 DO INSTEAD INSERT INTO uv_log VALUES ('ins', new.a);
CREATE RULE uv_rv1_upd AS ON UPDATE TO uv_rv1 DO INSTEAD INSERT INTO uv_log VALUES ('upd', old.a);
CREATE VIEW uv_rv2 AS SELECT b AS bb, a AS aa FROM uv_rv1 WHERE a > 1;
INSERT INTO uv_rv2 (aa, bb) VALUES (50, 'x');
UPDATE uv_rv2 SET bb = 'y' WHERE aa < 4;
DELETE FROM uv_rv2 WHERE aa = 3;
SELECT * FROM uv_log ORDER BY 1, 2;
SELECT relname, pg_relation_is_updatable(oid, true) FROM pg_class WHERE relname IN ('uv_rv1', 'uv_rv2') ORDER BY 1;
-- cleanup
DROP TABLE uv_base CASCADE;
DROP TABLE uv_log;
DROP FUNCTION uv_trig();
