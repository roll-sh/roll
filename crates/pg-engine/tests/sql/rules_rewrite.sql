-- INSTEAD rules make a view writable
CREATE TABLE rr_t1 (a int, b int);
CREATE TABLE rr_t2 (a int, b int);
INSERT INTO rr_t2 VALUES (1, 21), (2, 22), (3, 23);
CREATE VIEW rr_v1 AS SELECT * FROM rr_t1;
CREATE RULE rr_v1_ins AS ON INSERT TO rr_v1 DO INSTEAD INSERT INTO rr_t1 VALUES (new.a, new.b);
CREATE RULE rr_v1_upd AS ON UPDATE TO rr_v1 DO INSTEAD UPDATE rr_t1 SET a = new.a, b = new.b WHERE a = old.a;
CREATE RULE rr_v1_del AS ON DELETE TO rr_v1 DO INSTEAD DELETE FROM rr_t1 WHERE a = old.a;
INSERT INTO rr_v1 VALUES (1, 11);
INSERT INTO rr_v1 SELECT * FROM rr_t2;
INSERT INTO rr_v1 (b, a) SELECT b, a + 10 FROM rr_t2;
SELECT * FROM rr_v1 ORDER BY a, b;
UPDATE rr_v1 SET b = 42 WHERE a = 2;
UPDATE rr_v1 SET b = rr_t2.b + 100 FROM rr_t2 WHERE rr_v1.a = rr_t2.a;
DELETE FROM rr_v1 WHERE b = 42;
SELECT * FROM rr_v1 ORDER BY a, b;
DELETE FROM rr_v1;
SELECT count(*) FROM rr_t1;
-- DO ALSO rules with conditions, logging old and new
CREATE TABLE rr_emp (ename text, salary numeric);
CREATE TABLE rr_log (ename text, action text, newsal numeric, oldsal numeric);
CREATE RULE rr_emp_ins AS ON INSERT TO rr_emp DO INSERT INTO rr_log VALUES (new.ename, 'hired', new.salary, 0);
CREATE RULE rr_emp_upd AS ON UPDATE TO rr_emp WHERE new.salary <> old.salary DO INSERT INTO rr_log VALUES (new.ename, 'honored', new.salary, old.salary);
CREATE RULE rr_emp_del AS ON DELETE TO rr_emp DO INSERT INTO rr_log VALUES (old.ename, 'fired', 0, old.salary);
INSERT INTO rr_emp VALUES ('w', 5000), ('g', 80000);
UPDATE rr_emp SET ename = 'x' WHERE ename = 'w';
UPDATE rr_emp SET salary = 6000 WHERE ename = 'x';
DELETE FROM rr_emp WHERE ename = 'g';
SELECT * FROM rr_log ORDER BY ename, action;
-- qualified INSTEAD rules leave the rest to the statement; rules fire by name
CREATE TABLE rr_a (a int, b text);
CREATE TABLE rr_b (a int, b text);
CREATE TABLE rr_c (a int, b text);
CREATE RULE rr_a_2 AS ON INSERT TO rr_a WHERE new.a >= 10 AND new.a < 20 DO INSTEAD INSERT INTO rr_b VALUES (new.a, new.b);
CREATE RULE rr_a_1 AS ON INSERT TO rr_a WHERE new.a >= 20 DO INSERT INTO rr_c VALUES (new.a, new.b);
CREATE RULE rr_a_3 AS ON INSERT TO rr_a WHERE a < 5 DO INSTEAD NOTHING;
INSERT INTO rr_a VALUES (1, 'gone'), (7, 'a'), (15, 'b'), (25, 'a and c');
SELECT 'a', * FROM rr_a UNION ALL SELECT 'b', * FROM rr_b UNION ALL SELECT 'c', * FROM rr_c ORDER BY 1, 2;
-- the original's RETURNING comes from the INSTEAD rule's
CREATE TABLE rr_ret (f1 int, f2 int, tag text);
CREATE VIEW rr_retv AS SELECT f1, f2 FROM rr_ret;
CREATE RULE rr_retv_ins AS ON INSERT TO rr_retv DO INSTEAD INSERT INTO rr_ret VALUES (new.f1, new.f2, 'via rule') RETURNING f1, f2;
INSERT INTO rr_retv VALUES (1, 2) RETURNING *, f1 + f2 AS sum;
UPDATE rr_retv SET f2 = 3 RETURNING *;
CREATE RULE rr_retv_upd AS ON UPDATE TO rr_retv WHERE new.f1 > 0 DO INSTEAD NOTHING;
UPDATE rr_retv SET f2 = 3 RETURNING *;
-- multiple actions, NOTHING, SELECT actions, multi-row VALUES with DEFAULT
CREATE TABLE rr_src (f1 int, f2 int DEFAULT 0);
CREATE TABLE rr_slog (f1 int, f2 int, tag text, id serial);
INSERT INTO rr_src VALUES (1, 2), (11, 12);
CREATE RULE r1 AS ON UPDATE TO rr_src DO ALSO INSERT INTO rr_slog VALUES (old.*, 'old', DEFAULT), (new.*, 'new', DEFAULT);
UPDATE rr_src SET f2 = f2 + 1;
SELECT * FROM rr_slog ORDER BY id;
CREATE RULE r2 AS ON UPDATE TO rr_src DO ALSO VALUES (old.*, 'old'), (new.*, 'new');
UPDATE rr_src SET f2 = f2 * 10;
CREATE RULE r3 AS ON DELETE TO rr_src DO INSTEAD (INSERT INTO rr_slog VALUES (old.f1, 0, 'deleted'); INSERT INTO rr_slog VALUES (old.f2, 0, 'deleted'));
DELETE FROM rr_src WHERE f1 = 1;
SELECT f1, f2, tag FROM rr_slog WHERE tag = 'deleted' ORDER BY f1;
SELECT f1, f2 FROM rr_src ORDER BY f1;
-- rule definitions
SELECT tablename, rulename, definition FROM pg_rules WHERE tablename LIKE 'rr_%' ORDER BY 1, 2;
SELECT rulename, pg_get_ruledef(oid, true) FROM pg_rewrite WHERE ev_class = 'rr_src'::regclass ORDER BY 1;
SELECT rulename, ev_type, ev_enabled, is_instead FROM pg_rewrite WHERE ev_class = 'rr_a'::regclass ORDER BY 1;
SELECT relhasrules FROM pg_class WHERE oid = 'rr_a'::regclass;
-- errors
CREATE RULE rr_bad AS ON INSERT TO rr_a DO INSTEAD INSERT INTO rr_b VALUES (a, b);
CREATE RULE rr_bad AS ON INSERT TO rr_a DO INSTEAD INSERT INTO rr_b VALUES (old.a, old.b);
CREATE RULE rr_bad AS ON DELETE TO rr_a DO INSTEAD INSERT INTO rr_b VALUES (new.a, new.b);
CREATE RULE rr_bad AS ON INSERT TO rr_a WHERE new.a > 0 DO INSTEAD NOTIFY rr_chan;
CREATE RULE rr_bad AS ON INSERT TO rr_a DO ALSO INSERT INTO rr_b VALUES (new.a, new.b) RETURNING *;
CREATE RULE rr_a_1 AS ON INSERT TO rr_a DO INSTEAD NOTHING;
CREATE RULE "_RETURN" AS ON INSERT TO rr_a DO INSTEAD NOTHING;
CREATE RULE "_RETURN" AS ON SELECT TO rr_a DO INSTEAD SELECT 1 AS a, 'x'::text AS b;
DROP RULE "_RETURN" ON rr_v1;
DROP RULE rr_nope ON rr_a;
DROP RULE IF EXISTS rr_nope ON rr_a;
ALTER RULE rr_a_1 ON rr_a RENAME TO rr_a_2;
ALTER RULE rr_a_1 ON rr_a RENAME TO "_RETURN";
ALTER RULE "_RETURN" ON rr_v1 RENAME TO abc;
ALTER RULE rr_a_1 ON rr_a RENAME TO rr_a_0;
INSERT INTO rr_a VALUES (1, 'x') ON CONFLICT DO NOTHING;
MERGE INTO rr_a t USING (VALUES (1)) v(a) ON t.a = v.a WHEN MATCHED THEN DELETE;
-- recursion
CREATE TABLE rr_loop (a int);
CREATE RULE rr_loop_r AS ON INSERT TO rr_loop DO INSTEAD INSERT INTO rr_loop VALUES (new.a + 1);
INSERT INTO rr_loop VALUES (1);
DROP RULE rr_loop_r ON rr_loop;
-- COPY and WITH can't be rewritten into several statements
CREATE TABLE rr_cp (id serial, t text);
CREATE RULE rr_cp_r AS ON INSERT TO rr_cp DO ALSO DELETE FROM rr_cp;
COPY (INSERT INTO rr_cp (t) VALUES ('x') RETURNING id) TO stdout;
WITH w AS (INSERT INTO rr_cp (t) VALUES ('x') RETURNING id) SELECT * FROM w;
CREATE OR REPLACE RULE rr_cp_r AS ON INSERT TO rr_cp DO INSTEAD NOTHING;
COPY (INSERT INTO rr_cp (t) VALUES ('x') RETURNING id) TO stdout;
WITH w AS (INSERT INTO rr_cp (t) VALUES ('x') RETURNING id) SELECT * FROM w;
CREATE OR REPLACE RULE rr_cp_r AS ON INSERT TO rr_cp DO INSTEAD NOTIFY rr_chan;
WITH w AS (INSERT INTO rr_cp (t) VALUES ('x')) SELECT 1;
-- disabling rules, session_replication_role
ALTER TABLE rr_a DISABLE RULE rr_a_2;
INSERT INTO rr_a VALUES (12, 'not redirected');
ALTER TABLE rr_a ENABLE REPLICA RULE rr_a_2;
INSERT INTO rr_a VALUES (13, 'not redirected either');
SET session_replication_role = replica;
INSERT INTO rr_a VALUES (14, 'redirected in replica');
RESET session_replication_role;
SELECT rulename, ev_enabled FROM pg_rewrite WHERE ev_class = 'rr_a'::regclass ORDER BY 1;
SELECT 'a', * FROM rr_a WHERE a BETWEEN 10 AND 19 UNION ALL SELECT 'b', * FROM rr_b ORDER BY 1, 2;
-- comments and dependencies
COMMENT ON RULE rr_a_2 ON rr_a IS 'redirect';
SELECT obj_description(oid, 'pg_rewrite') FROM pg_rewrite WHERE rulename = 'rr_a_2';
DROP TABLE rr_b;
DROP TABLE rr_b CASCADE;
SELECT rulename FROM pg_rewrite WHERE ev_class = 'rr_a'::regclass ORDER BY 1;
DROP TABLE rr_a;
SELECT count(*) FROM pg_rewrite WHERE rulename LIKE 'rr_a%';
