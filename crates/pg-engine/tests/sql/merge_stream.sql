-- MERGE runs one joined row at a time: WHEN conditions, the action and its row triggers
-- for a row all happen before the next row is fetched.
CREATE TABLE ms_t (a int PRIMARY KEY, b text);
CREATE TABLE ms_s (a int, b text);
INSERT INTO ms_t VALUES (1, 't1'), (2, 't2'), (3, 't3');
INSERT INTO ms_s VALUES (1, 's1'), (2, 's2'), (4, 's4'), (5, 's5');
CREATE FUNCTION ms_tf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % % % old=% new=%', tg_name, tg_when, tg_op, tg_level,
    CASE WHEN tg_op IN ('UPDATE', 'DELETE') AND tg_level = 'ROW' THEN old::text END,
    CASE WHEN tg_op IN ('UPDATE', 'INSERT') AND tg_level = 'ROW' THEN new::text END;
  IF tg_when = 'BEFORE' AND tg_op = 'DELETE' THEN RETURN old; END IF;
  RETURN new;
END $$;
CREATE FUNCTION ms_snoop(x anyelement) RETURNS bool LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'snoop %', x; RETURN true; END $$;
CREATE TRIGGER b_row BEFORE INSERT OR UPDATE OR DELETE ON ms_t FOR EACH ROW EXECUTE FUNCTION ms_tf();
CREATE TRIGGER a_row AFTER INSERT OR UPDATE OR DELETE ON ms_t FOR EACH ROW EXECUTE FUNCTION ms_tf();
CREATE TRIGGER b_st BEFORE INSERT OR UPDATE OR DELETE ON ms_t FOR EACH STATEMENT EXECUTE FUNCTION ms_tf();
CREATE TRIGGER a_st AFTER INSERT OR UPDATE OR DELETE ON ms_t FOR EACH STATEMENT EXECUTE FUNCTION ms_tf();
MERGE INTO ms_t t USING (SELECT * FROM ms_s ORDER BY a) s ON t.a = s.a
  WHEN MATCHED AND ms_snoop(t.b) AND s.a = 1 THEN UPDATE SET b = s.b
  WHEN MATCHED AND ms_snoop(s.b) THEN DELETE
  WHEN NOT MATCHED AND ms_snoop(s.a) THEN INSERT VALUES (s.a, s.b)
  RETURNING merge_action(), t.*;
SELECT * FROM ms_t ORDER BY a;
-- A BEFORE ROW trigger runs for the first source row before the second is joined; the
-- second then hits the same target row.
MERGE INTO ms_t t USING (VALUES (1, 'x'), (1, 'y')) s(a, b) ON t.a = s.a
  WHEN MATCHED THEN UPDATE SET b = s.b;
SELECT * FROM ms_t ORDER BY a;
DROP TRIGGER b_row ON ms_t;
DROP TRIGGER a_row ON ms_t;
DROP TRIGGER b_st ON ms_t;
DROP TRIGGER a_st ON ms_t;
-- A volatile source sees each action's effects only through the target's snapshot.
CREATE FUNCTION ms_count() RETURNS bigint LANGUAGE sql VOLATILE AS 'SELECT count(*) FROM ms_t';
MERGE INTO ms_t t USING (SELECT g AS a, ms_count() AS n FROM generate_series(6, 8) g) s ON t.a = s.a
  WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'n=' || s.n)
  RETURNING t.*;
-- The same MERGE text in a later transaction's DO block, after a rolled-back one added a
-- different trigger: the statement is planned against the catalog it runs with.
CREATE TABLE ms_pt (tid int, balance float, val text) PARTITION BY LIST (tid);
CREATE TABLE ms_p1 PARTITION OF ms_pt FOR VALUES IN (1, 4);
CREATE TABLE ms_p2 PARTITION OF ms_pt FOR VALUES IN (2, 5, 6);
CREATE TABLE ms_p4 PARTITION OF ms_pt DEFAULT;
CREATE TABLE ms_src (sid int, delta float);
INSERT INTO ms_src SELECT id, id * 10 FROM generate_series(1, 6) AS id;
INSERT INTO ms_pt SELECT id, id * 100, 'initial' FROM generate_series(1, 7, 2) AS id;
BEGIN;
CREATE FUNCTION ms_null() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$;
CREATE TRIGGER del_trig BEFORE DELETE ON ms_pt FOR EACH ROW EXECUTE PROCEDURE ms_null();
DO $$ DECLARE result integer; BEGIN
MERGE INTO ms_pt t USING ms_src s ON t.tid = s.sid
  WHEN MATCHED THEN UPDATE SET tid = tid + 1, balance = balance + delta, val = val || ' updated by merge'
  WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted by merge')
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' not matched by source';
GET DIAGNOSTICS result := ROW_COUNT; RAISE NOTICE 'ROW_COUNT = %', result; END; $$;
SELECT * FROM ms_pt ORDER BY tid, val;
ROLLBACK;
BEGIN;
CREATE FUNCTION ms_null() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$;
CREATE TRIGGER ins_trig BEFORE INSERT ON ms_pt FOR EACH ROW EXECUTE PROCEDURE ms_null();
DO $$ DECLARE result integer; BEGIN
MERGE INTO ms_pt t USING ms_src s ON t.tid = s.sid
  WHEN MATCHED THEN UPDATE SET tid = tid + 1, balance = balance + delta, val = val || ' updated by merge'
  WHEN NOT MATCHED THEN INSERT VALUES (sid, delta, 'inserted by merge')
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' not matched by source';
GET DIAGNOSTICS result := ROW_COUNT; RAISE NOTICE 'ROW_COUNT = %', result; END; $$;
SELECT * FROM ms_pt ORDER BY tid, val;
ROLLBACK;
