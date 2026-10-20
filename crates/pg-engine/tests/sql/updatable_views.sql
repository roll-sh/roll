CREATE TABLE ub (id int PRIMARY KEY, name text NOT NULL, qty int DEFAULT 7, note text DEFAULT 'n/a', CHECK (qty >= 0));
INSERT INTO ub VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 30);
-- simple views
CREATE VIEW uv AS SELECT id, name, qty FROM ub;
INSERT INTO uv VALUES (4, 'd', 4) RETURNING *;
INSERT INTO uv (name, id) VALUES ('e', 5) RETURNING id, qty, uv.*;
UPDATE uv SET qty = qty + 100 WHERE id IN (1, 2) RETURNING *;
DELETE FROM uv WHERE id = 5 RETURNING name;
SELECT * FROM ub ORDER BY id;
INSERT INTO uv VALUES (6, NULL, 1);
INSERT INTO uv VALUES (6, 'f', -1);
INSERT INTO uv VALUES (1, 'dup', 1);
INSERT INTO uv (note) VALUES ('x');
-- renamed, reordered and computed columns
CREATE VIEW uv2 (k, label, doubled) AS SELECT id, upper(name), qty * 2 FROM ub;
CREATE VIEW uv3 AS SELECT qty AS amount, id AS key, 'const' AS c, name FROM ub WHERE qty < 50;
INSERT INTO uv3 (key, name, amount) VALUES (10, 'ten', 10) RETURNING *;
INSERT INTO uv3 VALUES (11, 11, 'const', 'eleven');
INSERT INTO uv3 VALUES (11, 11, DEFAULT, 'eleven') RETURNING *;
UPDATE uv3 SET amount = amount + 1 RETURNING key, amount;
UPDATE uv3 SET c = 'x';
UPDATE uv2 SET label = 'x';
UPDATE uv2 SET k = k + 1000 WHERE label = 'TEN' RETURNING *;
DELETE FROM uv2 WHERE doubled > 100 RETURNING k;
SELECT * FROM ub ORDER BY id;
-- WHERE: invisible rows are untouched, inserts may vanish
UPDATE uv3 SET amount = 0 WHERE key = 3;
DELETE FROM uv3 WHERE key = 3;
INSERT INTO uv3 VALUES (99, 12, 'const', 'hidden');
SELECT * FROM uv3 ORDER BY key;
-- check options
CREATE VIEW uvc AS SELECT * FROM ub WHERE qty < 50 WITH CHECK OPTION;
INSERT INTO uvc VALUES (20, 't', 5);
INSERT INTO uvc VALUES (21, 't', 500);
UPDATE uvc SET qty = 1000 WHERE id = 20;
CREATE VIEW uvl AS SELECT * FROM uvc WHERE qty > 2 WITH LOCAL CHECK OPTION;
INSERT INTO uvl VALUES (22, 'u', 1);
INSERT INTO uvl VALUES (22, 'u', 100);
INSERT INTO uvl VALUES (22, 'u', 3);
CREATE VIEW uvn AS SELECT * FROM uv3 WHERE amount > 2;
CREATE VIEW uvcc AS SELECT * FROM uvn WITH CASCADED CHECK OPTION;
INSERT INTO uvcc (amount, key, name) VALUES (1, 23, 'v');
INSERT INTO uvcc (amount, key, name) VALUES (100, 23, 'v');
INSERT INTO uvcc (amount, key, name) VALUES (5, 23, 'v') RETURNING *;
UPDATE uvcc SET amount = 60 WHERE key = 23;
UPDATE uvcc SET amount = 6 WHERE key = 23 RETURNING key, amount;
SELECT table_name, check_option, is_updatable, is_insertable_into FROM information_schema.views WHERE table_name LIKE 'uv%' ORDER BY table_name;
-- nested updatable views
UPDATE uvn SET name = 'nested' WHERE key = 23 RETURNING *;
DELETE FROM uvn WHERE key = 23;
-- not automatically updatable
CREATE VIEW nv1 AS SELECT DISTINCT name FROM ub;
CREATE VIEW nv2 AS SELECT name, count(*) FROM ub GROUP BY name;
CREATE VIEW nv3 AS SELECT a.id FROM ub a JOIN ub b USING (id);
CREATE VIEW nv4 AS SELECT id FROM ub LIMIT 5;
CREATE VIEW nv5 AS SELECT id FROM ub UNION SELECT 1;
CREATE VIEW nv6 AS SELECT 1 AS one;
CREATE VIEW nv7 AS SELECT id, row_number() OVER () FROM ub;
CREATE VIEW nv8 AS SELECT id, generate_series(1, 2) FROM ub;
CREATE VIEW nv9 AS WITH w AS (SELECT * FROM ub) SELECT * FROM w;
CREATE VIEW nv10 AS SELECT * FROM (SELECT * FROM ub) s;
CREATE VIEW nv11 AS SELECT id * 2 AS x FROM ub;
INSERT INTO nv1 VALUES ('x');
INSERT INTO nv2 VALUES ('x', 1);
DELETE FROM nv3;
UPDATE nv4 SET id = 1;
DELETE FROM nv5;
DELETE FROM nv6;
DELETE FROM nv7;
DELETE FROM nv8;
DELETE FROM nv9;
DELETE FROM nv10;
INSERT INTO nv11 VALUES (1);
UPDATE nv11 SET x = 1;
DELETE FROM nv11 WHERE x = 6 RETURNING *;
SELECT table_name, is_updatable, is_insertable_into FROM information_schema.views WHERE table_name LIKE 'nv%' ORDER BY table_name;
SELECT table_name, column_name, is_updatable FROM information_schema.columns WHERE table_name IN ('uv2', 'uv3', 'nv11') ORDER BY table_name, ordinal_position;
SELECT pg_relation_is_updatable('uv3'::regclass, false), pg_relation_is_updatable('nv1'::regclass, false), pg_column_is_updatable('uv3'::regclass, 3::int2, false), pg_column_is_updatable('uv3'::regclass, 1::int2, false);
-- triggers on the base table fire
CREATE FUNCTION ub_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % on %', TG_WHEN, TG_OP, TG_TABLE_NAME;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER ub_row BEFORE INSERT OR UPDATE OR DELETE ON ub FOR EACH ROW EXECUTE FUNCTION ub_note();
CREATE TRIGGER ub_stmt AFTER INSERT OR UPDATE OR DELETE ON ub FOR EACH STATEMENT EXECUTE FUNCTION ub_note();
CREATE TRIGGER uv_stmt BEFORE INSERT ON uv FOR EACH STATEMENT EXECUTE FUNCTION ub_note();
INSERT INTO uv VALUES (30, 'trig', 1);
UPDATE uv SET qty = 2 WHERE id = 30;
DELETE FROM uv WHERE id = 30;
-- UPDATE ... FROM and DELETE ... USING through a view
CREATE TABLE ux (id int, bump int);
INSERT INTO ux VALUES (4, 5), (20, 6), (4, 100);
DROP TRIGGER ub_row ON ub;
DROP TRIGGER ub_stmt ON ub;
UPDATE uv SET qty = uv.qty + ux.bump FROM ux WHERE ux.id = uv.id RETURNING uv.id, uv.qty, ux.bump;
DELETE FROM uv USING ux WHERE ux.id = uv.id AND ux.bump = 6;
SELECT * FROM ub ORDER BY id;
DROP VIEW nv6;
DROP TABLE ub, ux CASCADE;
DROP FUNCTION ub_note();
