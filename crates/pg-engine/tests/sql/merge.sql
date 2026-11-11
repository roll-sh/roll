-- basic upsert
CREATE TABLE mg_t (id int PRIMARY KEY, val text, n int DEFAULT 0);
CREATE TABLE mg_s (id int, val text);
INSERT INTO mg_t VALUES (1, 'one', 1), (2, 'two', 2), (3, 'three', 3);
INSERT INTO mg_s VALUES (2, 'TWO'), (3, NULL), (4, 'four'), (5, 'five');
MERGE INTO mg_t t USING mg_s s ON t.id = s.id
  WHEN MATCHED AND s.val IS NULL THEN DELETE
  WHEN MATCHED THEN UPDATE SET val = s.val, n = t.n + 10
  WHEN NOT MATCHED AND s.id = 5 THEN DO NOTHING
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val);
SELECT * FROM mg_t ORDER BY id;

-- RETURNING with merge_action()
MERGE INTO mg_t t USING (VALUES (1, 'uno'), (6, 'six'), (4, NULL)) AS s(id, val) ON t.id = s.id
  WHEN MATCHED AND s.val IS NULL THEN DELETE
  WHEN MATCHED THEN UPDATE SET val = s.val
  WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)
  RETURNING merge_action(), s.id, t.*;
SELECT * FROM mg_t ORDER BY id;

-- NOT MATCHED BY SOURCE
MERGE INTO mg_t t USING (SELECT 1 AS id UNION ALL SELECT 2) s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET n = 100
  WHEN NOT MATCHED BY SOURCE AND t.id > 5 THEN DELETE
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET n = -1
  RETURNING merge_action(), t.id, t.n;
SELECT * FROM mg_t ORDER BY id;

-- NOT MATCHED BY TARGET spelled out, DEFAULT VALUES, INSERT with DEFAULT
CREATE TABLE mg_d (id serial PRIMARY KEY, note text DEFAULT 'dflt');
MERGE INTO mg_d d USING (VALUES (10), (11)) s(x) ON false
  WHEN NOT MATCHED BY TARGET AND s.x = 10 THEN INSERT DEFAULT VALUES
  WHEN NOT MATCHED BY TARGET THEN INSERT (note) VALUES (DEFAULT);
SELECT * FROM mg_d ORDER BY id;

-- source is a table name without alias, target with schema
MERGE INTO public.mg_d USING mg_s ON mg_d.id = mg_s.id
  WHEN MATCHED THEN UPDATE SET note = mg_s.val;
SELECT * FROM mg_d ORDER BY id;

-- cardinality violation
MERGE INTO mg_t t USING (VALUES (1), (1)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET n = n + 1;
-- the same row matched twice by DO NOTHING is fine
MERGE INTO mg_t t USING (VALUES (1), (1)) s(id) ON t.id = s.id
  WHEN MATCHED THEN DO NOTHING;

-- constraints and errors
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET id = 1;
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN NOT MATCHED THEN UPDATE SET n = 1;
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN INSERT VALUES (1);
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET nope = 1;
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT VALUES (t.id);
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET n = s.id;
MERGE INTO mg_nope t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN DELETE;
MERGE INTO mg_t USING mg_t ON true WHEN MATCHED THEN DELETE;
MERGE INTO mg_t t USING mg_s t ON true WHEN MATCHED THEN DELETE;
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET n = count(*);
SELECT merge_action();
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET val = 'x'
  WHEN MATCHED THEN DELETE;
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT (id, id) VALUES (1, 2);
MERGE INTO mg_t t USING (VALUES (2)) s(id) ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT (id) VALUES (1, 2);

-- WITH, subquery source, parameter-free expressions
WITH src AS (SELECT id, val || '+' AS val FROM mg_s WHERE val IS NOT NULL)
MERGE INTO mg_t t USING src ON t.id = src.id
  WHEN MATCHED THEN UPDATE SET val = src.val
  WHEN NOT MATCHED THEN INSERT VALUES (src.id, src.val, 7)
  RETURNING merge_action() AS act, t.id, t.val, t.n;

-- triggers
CREATE TABLE mg_log (msg text);
CREATE FUNCTION mg_trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO mg_log VALUES (TG_WHEN || ' ' || TG_OP || ' ' || TG_LEVEL || coalesce(' ' || NEW.id, '') || coalesce(' old ' || OLD.id, ''));
  RETURN coalesce(NEW, OLD);
END $$;
CREATE TABLE mg_tr (id int PRIMARY KEY, v int);
INSERT INTO mg_tr VALUES (1, 1), (2, 2);
CREATE TRIGGER mg_b BEFORE INSERT OR UPDATE OR DELETE ON mg_tr FOR EACH ROW EXECUTE FUNCTION mg_trg();
CREATE TRIGGER mg_a AFTER INSERT OR UPDATE OR DELETE ON mg_tr FOR EACH ROW EXECUTE FUNCTION mg_trg();
CREATE TRIGGER mg_bs BEFORE INSERT OR UPDATE OR DELETE ON mg_tr FOR EACH STATEMENT EXECUTE FUNCTION mg_trg();
CREATE TRIGGER mg_as AFTER INSERT OR UPDATE OR DELETE ON mg_tr FOR EACH STATEMENT EXECUTE FUNCTION mg_trg();
MERGE INTO mg_tr t USING (VALUES (1), (3)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET v = 10
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, 30)
  WHEN NOT MATCHED BY SOURCE THEN DELETE;
SELECT * FROM mg_log;
SELECT * FROM mg_tr ORDER BY id;
DELETE FROM mg_log;
MERGE INTO mg_tr t USING (VALUES (1)) s(id) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET v = 11;
SELECT * FROM mg_log;

-- MERGE in PL/pgSQL with FOUND and ROW_COUNT
DO $$
DECLARE n int;
BEGIN
  MERGE INTO mg_tr t USING (VALUES (1), (2), (3)) s(id) ON t.id = s.id
    WHEN MATCHED THEN UPDATE SET v = v + 1;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'merged % found %', n, FOUND;
END $$;

-- updatable view target
CREATE TABLE mg_base (id int PRIMARY KEY, v text, hidden int DEFAULT 5);
CREATE VIEW mg_v AS SELECT id, v FROM mg_base WHERE id < 100;
MERGE INTO mg_v USING (VALUES (1, 'a'), (2, 'b')) s(id, v) ON mg_v.id = s.id
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v);
MERGE INTO mg_v USING (VALUES (1, 'A'), (3, 'c')) s(id, v) ON mg_v.id = s.id
  WHEN MATCHED THEN UPDATE SET v = s.v
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v);
SELECT * FROM mg_base ORDER BY id;

-- prepared statement with parameters
PREPARE mg_p(int, text) AS MERGE INTO mg_t t USING (VALUES ($1, $2)) s(id, val) ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET val = s.val WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val);
EXECUTE mg_p(1, 'p1');
EXECUTE mg_p(50, 'p50');
SELECT * FROM mg_t ORDER BY id;
DEALLOCATE mg_p;

-- EXPLAIN
EXPLAIN (COSTS OFF) MERGE INTO mg_t t USING mg_s s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET val = s.val;
EXPLAIN (COSTS OFF) MERGE INTO mg_t t USING mg_s s ON t.id = s.id
  WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id);
EXPLAIN (COSTS OFF) MERGE INTO mg_t t USING mg_s s ON t.id = s.id
  WHEN NOT MATCHED BY SOURCE THEN DELETE;

-- generated / identity columns
CREATE TABLE mg_g (id int GENERATED ALWAYS AS IDENTITY, k int UNIQUE, dbl int GENERATED ALWAYS AS (k * 2) STORED);
MERGE INTO mg_g g USING (VALUES (1), (2)) s(k) ON g.k = s.k WHEN NOT MATCHED THEN INSERT (k) VALUES (s.k);
MERGE INTO mg_g g USING (VALUES (2), (3)) s(k) ON g.k = s.k
  WHEN MATCHED THEN UPDATE SET k = g.k + 10 WHEN NOT MATCHED THEN INSERT (k) VALUES (s.k);
SELECT * FROM mg_g ORDER BY id;
MERGE INTO mg_g g USING (VALUES (5)) s(k) ON g.k = s.k WHEN NOT MATCHED THEN INSERT (id, k) VALUES (99, s.k);
MERGE INTO mg_g g USING (VALUES (5)) s(k) ON g.k = s.k WHEN NOT MATCHED THEN INSERT (id, k) OVERRIDING SYSTEM VALUE VALUES (99, s.k);
MERGE INTO mg_g g USING (VALUES (5)) s(k) ON g.k = s.k WHEN MATCHED THEN UPDATE SET dbl = 1;
SELECT * FROM mg_g ORDER BY id;

-- unique violation from MERGE INSERT
MERGE INTO mg_g g USING (VALUES (1)) s(k) ON false WHEN NOT MATCHED THEN INSERT (k) VALUES (s.k);

DROP VIEW mg_v;
DROP TABLE mg_t, mg_s, mg_d, mg_log, mg_tr, mg_base, mg_g;
DROP FUNCTION mg_trg();
