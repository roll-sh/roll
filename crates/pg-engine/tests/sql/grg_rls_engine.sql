-- Row-level security gaps closed in the regression sweep: policies over system columns,
-- polqual as a node tree, SQL functions re-checked when row_security changes.
DROP ROLE IF EXISTS grg_alice;
CREATE ROLE grg_alice;
-- a policy may read ctid; updates check it before the new row has a TID
CREATE TABLE grg_cc (a int, b text);
INSERT INTO grg_cc VALUES (1, 'Apple');
ALTER TABLE grg_cc ENABLE ROW LEVEL SECURITY;
ALTER TABLE grg_cc FORCE ROW LEVEL SECURITY;
CREATE POLICY p1 ON grg_cc AS PERMISSIVE USING (ctid IN ('(0,1)', '(0,2)', '(4294967295,0)'));
SELECT ctid, * FROM grg_cc;
BEGIN;
DECLARE grg_cur CURSOR FOR SELECT * FROM grg_cc;
FETCH FROM grg_cur;
UPDATE grg_cc SET b = 'Manzana' WHERE CURRENT OF grg_cur;
SELECT ctid, * FROM grg_cc;
ROLLBACK;
UPDATE grg_cc SET b = 'Pera';
SELECT ctid, * FROM grg_cc;
DELETE FROM grg_cc RETURNING ctid, *;
CREATE TABLE grg_cc2 (a int);
INSERT INTO grg_cc2 VALUES (1), (2), (3);
ALTER TABLE grg_cc2 ENABLE ROW LEVEL SECURITY;
ALTER TABLE grg_cc2 FORCE ROW LEVEL SECURITY;
CREATE POLICY p2 ON grg_cc2 USING (ctid <> '(0,2)');
SELECT * FROM grg_cc2 ORDER BY a;
UPDATE grg_cc2 SET a = a * 10 RETURNING a;
SELECT * FROM grg_cc2 ORDER BY a;
-- pg_policy keeps the expressions as node trees
CREATE TABLE grg_coll (c) AS VALUES ('bar'::text);
CREATE POLICY coll_p ON grg_coll USING (c < ('foo'::text COLLATE "C"));
CREATE POLICY coll_q ON grg_coll FOR INSERT WITH CHECK (c = current_user AND EXISTS (SELECT 1 FROM grg_coll x WHERE x.c = grg_coll.c));
SELECT (string_to_array(polqual, ':'))[7] AS inputcollid FROM pg_policy WHERE polrelid = 'grg_coll'::regclass AND polname = 'coll_p';
SELECT polname, polqual IS NOT NULL, polwithcheck IS NOT NULL, left(coalesce(polqual, polwithcheck), 9) FROM pg_policy WHERE polrelid = 'grg_coll'::regclass ORDER BY 1;
SELECT polname, pg_get_expr(polqual, polrelid), pg_get_expr(polwithcheck, polrelid) FROM pg_policy WHERE polrelid = 'grg_coll'::regclass ORDER BY 1;
SELECT policyname, qual, with_check FROM pg_policies WHERE tablename = 'grg_coll' ORDER BY 1;
-- a SQL function's statement is planned again once row_security changes
CREATE TABLE grg_rls_t (c text);
CREATE TABLE grg_test_t (c text);
INSERT INTO grg_rls_t VALUES ('a'), ('b'), ('c'), ('d');
INSERT INTO grg_test_t VALUES ('a'), ('b');
ALTER TABLE grg_rls_t ENABLE ROW LEVEL SECURITY;
GRANT SELECT ON grg_rls_t TO grg_alice;
GRANT SELECT ON grg_test_t TO grg_alice;
CREATE POLICY p1 ON grg_rls_t FOR SELECT TO grg_alice USING (c = current_setting('grg_test.blah'));
CREATE FUNCTION grg_rls_f(text) RETURNS text
BEGIN ATOMIC
 SELECT set_config('grg_test.blah', $1, true) || set_config('row_security', 'false', true) || string_agg(c, ',' ORDER BY c) FROM grg_rls_t;
END;
SET plan_cache_mode TO force_custom_plan;
SELECT grg_rls_f(c) FROM grg_test_t ORDER BY grg_rls_f;
SET ROLE grg_alice;
SELECT grg_rls_f(c) FROM grg_test_t ORDER BY grg_rls_f;
RESET ROLE;
SET plan_cache_mode TO force_generic_plan;
SELECT grg_rls_f(c) FROM grg_test_t ORDER BY grg_rls_f;
SET ROLE grg_alice;
SELECT grg_rls_f(c) FROM grg_test_t ORDER BY grg_rls_f;
RESET ROLE;
RESET plan_cache_mode;
DROP FUNCTION grg_rls_f(text);
DROP TABLE grg_cc, grg_cc2, grg_coll, grg_rls_t, grg_test_t;
DROP ROLE grg_alice;
