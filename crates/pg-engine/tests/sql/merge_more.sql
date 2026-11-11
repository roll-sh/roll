CREATE TABLE mm_a (id int, x text);
CREATE TABLE mm_b (id int, y text, z int);
INSERT INTO mm_a VALUES (1, 'a1');
INSERT INTO mm_b VALUES (1, 'b1', 10), (2, 'b2', 20);
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id
  WHEN MATCHED THEN UPDATE SET x = mm_b.y WHEN NOT MATCHED THEN INSERT VALUES (mm_b.id, mm_b.y) RETURNING *;
WITH m AS (MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED THEN DELETE RETURNING merge_action() act, mm_a.*)
SELECT * FROM m ORDER BY id;
SELECT count(*) FROM mm_a;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED THEN UPDATE SET x = mm_b.y RETURNING merge_action() + 1;
SELECT pg_typeof(merge_action());
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED THEN DELETE RETURNING (SELECT merge_action());
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED AND merge_action() = 'x' THEN DELETE;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (z);
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (x);
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = y;
MERGE INTO mm_a USING mm_b ON mm_a.id = nope WHEN MATCHED THEN DELETE;
MERGE INTO mm_a USING (SELECT 1) s ON true WHEN MATCHED THEN UPDATE SET mm_a.x = 'q';
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED THEN UPDATE SET x = DEFAULT RETURNING ctid IS NOT NULL;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED THEN UPDATE SET x = 'zz' RETURNING mm_a.ctid IS NOT NULL, mm_a.tableoid::regclass, mm_b.*;
MERGE INTO mm_a AS q USING mm_b ON q.id = mm_b.id WHEN MATCHED THEN DELETE RETURNING mm_a.id;
MERGE INTO mm_a USING mm_b ON 1 WHEN MATCHED THEN DELETE;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED AND 1 THEN DELETE;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN MATCHED AND count(*) > 0 THEN DELETE;
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (sum(mm_b.z));
MERGE INTO mm_a USING mm_b ON mm_a.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (generate_series(1, 2));
SELECT a.id FROM mm_a q;
CREATE SEQUENCE mm_seq;
MERGE INTO mm_seq USING mm_b ON false WHEN NOT MATCHED THEN DO NOTHING;
DROP SEQUENCE mm_seq;

-- join source, lateral-free subquery, and an empty source
MERGE INTO mm_a t USING (mm_b JOIN (VALUES (1, 'j')) v(k, w) ON mm_b.id = v.k) ON t.id = mm_b.id
  WHEN NOT MATCHED THEN INSERT VALUES (mm_b.id, v.w || mm_b.y);
SELECT * FROM mm_a ORDER BY id;
MERGE INTO mm_a t USING (SELECT * FROM mm_b WHERE false) s ON t.id = s.id
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = 'orphan';
SELECT * FROM mm_a ORDER BY id;

-- views: INSTEAD OF triggers, non-updatable views
CREATE TABLE mm_base (id int PRIMARY KEY, v text);
CREATE VIEW mm_tv AS SELECT id, upper(v) AS v FROM mm_base;
MERGE INTO mm_tv USING mm_b ON mm_tv.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (mm_b.id, mm_b.y);
CREATE FUNCTION mm_tv_trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN INSERT INTO mm_base VALUES (NEW.id, lower(NEW.v)); RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN UPDATE mm_base SET v = lower(NEW.v) WHERE id = OLD.id; RETURN NEW;
  ELSE DELETE FROM mm_base WHERE id = OLD.id; RETURN OLD;
  END IF;
END $$;
CREATE TRIGGER mm_tv_i INSTEAD OF INSERT ON mm_tv FOR EACH ROW EXECUTE FUNCTION mm_tv_trg();
MERGE INTO mm_tv USING mm_b ON mm_tv.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (mm_b.id, mm_b.y);
SELECT * FROM mm_tv ORDER BY id;
MERGE INTO mm_tv USING mm_b ON mm_tv.id = mm_b.id WHEN MATCHED THEN UPDATE SET v = 'NEW';
CREATE TRIGGER mm_tv_u INSTEAD OF UPDATE ON mm_tv FOR EACH ROW EXECUTE FUNCTION mm_tv_trg();
CREATE TRIGGER mm_tv_d INSTEAD OF DELETE ON mm_tv FOR EACH ROW EXECUTE FUNCTION mm_tv_trg();
MERGE INTO mm_tv t USING (VALUES (1, 'X'), (2, NULL), (3, 'Z')) s(id, v) ON t.id = s.id
  WHEN MATCHED AND s.v IS NULL THEN DELETE
  WHEN MATCHED THEN UPDATE SET v = s.v
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)
  RETURNING merge_action(), t.*;
SELECT * FROM mm_base ORDER BY id;

-- row-level security
CREATE ROLE mm_user;
CREATE TABLE mm_r (id int PRIMARY KEY, owner text, v int);
GRANT ALL ON mm_r TO mm_user;
INSERT INTO mm_r VALUES (1, 'mm_user', 1), (2, 'other', 2), (3, 'mm_user', 3);
ALTER TABLE mm_r ENABLE ROW LEVEL SECURITY;
CREATE POLICY sel ON mm_r FOR SELECT USING (true);
CREATE POLICY upd ON mm_r FOR UPDATE USING (owner = current_user) WITH CHECK (v < 100);
CREATE POLICY del ON mm_r FOR DELETE USING (owner = current_user);
CREATE POLICY ins ON mm_r FOR INSERT WITH CHECK (owner = current_user);
SET ROLE mm_user;
MERGE INTO mm_r USING (VALUES (1)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN UPDATE SET v = 10;
MERGE INTO mm_r USING (VALUES (2)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN UPDATE SET v = 10;
MERGE INTO mm_r USING (VALUES (1)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN UPDATE SET v = 1000;
MERGE INTO mm_r USING (VALUES (2)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN DELETE;
MERGE INTO mm_r USING (VALUES (9)) s(id) ON mm_r.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (9, 'other', 1);
MERGE INTO mm_r USING (VALUES (9)) s(id) ON mm_r.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (9, 'mm_user', 1);
MERGE INTO mm_r USING (VALUES (3)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN DELETE;
RESET ROLE;
DROP POLICY sel ON mm_r;
CREATE POLICY sel2 ON mm_r FOR SELECT USING (id <> 1);
SET ROLE mm_user;
MERGE INTO mm_r USING (VALUES (1)) s(id) ON mm_r.id = s.id WHEN MATCHED THEN UPDATE SET v = 5 WHEN NOT MATCHED THEN DO NOTHING;
RESET ROLE;
SELECT * FROM mm_r ORDER BY id;

-- privileges
CREATE TABLE mm_p (id int, a int, b int);
INSERT INTO mm_p VALUES (1, 1, 1);
GRANT SELECT ON mm_b TO mm_user;
GRANT SELECT (id), UPDATE (a) ON mm_p TO mm_user;
SET ROLE mm_user;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED THEN UPDATE SET a = 5;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED THEN UPDATE SET b = 5;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED THEN DELETE;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN NOT MATCHED THEN INSERT VALUES (1, 2, 3);
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED AND mm_p.b = 1 THEN UPDATE SET a = 6;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED THEN DO NOTHING;
RESET ROLE;
SELECT * FROM mm_p;

-- read-only transaction
BEGIN READ ONLY;
MERGE INTO mm_p USING mm_b ON mm_p.id = mm_b.id WHEN MATCHED THEN DO NOTHING;
ROLLBACK;

-- foreign keys and deferred constraints
CREATE TABLE mm_parent (id int PRIMARY KEY);
CREATE TABLE mm_child (id int PRIMARY KEY, pid int REFERENCES mm_parent ON DELETE CASCADE);
INSERT INTO mm_parent VALUES (1), (2);
INSERT INTO mm_child VALUES (10, 1), (20, 2);
MERGE INTO mm_child c USING (VALUES (30, 3)) s(id, pid) ON c.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.pid);
MERGE INTO mm_parent p USING (VALUES (1)) s(id) ON p.id = s.id WHEN MATCHED THEN DELETE;
SELECT * FROM mm_child ORDER BY id;

-- SQL function with MERGE
CREATE FUNCTION mm_fn(i int) RETURNS SETOF text LANGUAGE sql AS $$
  MERGE INTO mm_parent p USING (VALUES (i)) s(id) ON p.id = s.id
    WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id)
    RETURNING merge_action()
$$;
SELECT mm_fn(2), mm_fn(5);
SELECT * FROM mm_parent ORDER BY id;

-- COPY (MERGE ... RETURNING) TO
COPY (MERGE INTO mm_parent p USING (VALUES (7)) s(id) ON p.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id) RETURNING p.id) TO STDOUT;
COPY (MERGE INTO mm_parent p USING (VALUES (7)) s(id) ON p.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id)) TO STDOUT;

DROP FUNCTION mm_fn(int);
DROP VIEW mm_tv;
DROP FUNCTION mm_tv_trg();
DROP TABLE mm_a, mm_b, mm_base, mm_r, mm_p, mm_child, mm_parent;
DROP ROLE mm_user;
