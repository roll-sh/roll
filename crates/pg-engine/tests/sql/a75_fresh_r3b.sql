-- Round 3 difftest finds, batch 2 (agent 75).
-- `x [NOT] IN (subquery)` over no rows never evaluates x (ExecHashSubPlan's empty table,
-- ExecScanSubPlan's no row): a call failing in it doesn't fail the statement.
CREATE TABLE a75b2t (id int PRIMARY KEY, c int2);
CREATE TABLE a75b2u (id int PRIMARY KEY);
INSERT INTO a75b2u VALUES (19);
CREATE FUNCTION a75b2f(p int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF ('1'::jsonb - 'a') ?| ARRAY['a'] THEN RETURN p; END IF; RETURN 0; END $$;
SELECT g, a75b2f(g) NOT IN (SELECT c FROM a75b2t), a75b2f(g) IN (SELECT c FROM a75b2t), a75b2f(g) > ALL (SELECT c FROM a75b2t) FROM generate_series(1, 2) g;
DELETE FROM a75b2u x WHERE EXISTS (SELECT 1 FROM generate_series(1, 2) s1(g) WHERE a75b2f(s1.g) NOT IN (SELECT t4.c FROM a75b2t t4));
SELECT count(*) FROM a75b2u;
INSERT INTO a75b2t VALUES (1, 1);
SELECT a75b2f(1) NOT IN (SELECT c FROM a75b2t);
-- A subquery planned apart (grouped, aggregated) restricted by a constant-false qual is never
-- planned (relation_excluded_by_constraints): its failing constants don't fail the
-- statement.
CREATE VIEW a75b2v0 AS SELECT 1 AS c0 FROM (VALUES (0.001, 1)) s1 (v0, v1) WHERE (NULL -|- int4range(length('a%'), 1)) GROUP BY s1.v1;
CREATE VIEW a75b2v1 AS SELECT 1 AS c0 FROM a75b2v0 t1 WHERE false;
SELECT count(*) FROM a75b2v1;
SELECT (SELECT count(*) FILTER (WHERE NULL) FROM a75b2v1);
INSERT INTO a75b2u VALUES (12) ON CONFLICT (id) DO UPDATE SET id = (SELECT count(*) FROM a75b2v1);
SELECT * FROM a75b2u ORDER BY 1;
CREATE VIEW a75b2w0 AS SELECT t1.id AS c0 FROM a75b2t t1 WHERE isempty(int4range(('{"a": 2.50}'::jsonb ->> 'a')::int, NULL));
CREATE VIEW a75b2w1 AS SELECT max(1) AS c2 FROM (generate_series(1, 6) s1(g) LEFT JOIN a75b2w0 t2 ON true);
UPDATE a75b2t x SET c = (SELECT x.c FROM a75b2w1 t1 WHERE NULL) RETURNING *;
-- Over a join, a constant-false qual gates the join, whose relations are planned.
SELECT count(*) FROM a75b2v0, a75b2t WHERE false;
-- An immutable call over constants under a COLLATE folds (a CollateExpr over a Const is a
-- Const): a FULL JOIN on such a comparison is a FULL JOIN on a constant.
SELECT 1 FROM a75b2t a FULL JOIN a75b2u b ON 'a' = 'a' COLLATE "und-x-icu";
SELECT EXISTS (SELECT 1 FROM a75b2t a FULL JOIN a75b2u b ON 'a' = 'a' COLLATE "und-x-icu");
SELECT COALESCE(true, EXISTS (SELECT 1 FROM a75b2t a FULL JOIN a75b2u b ON 'a' < 'B' COLLATE "en-x-icu"));
SELECT 'a' < 'B' COLLATE "en-x-icu", 'a' < 'B' COLLATE "C", upper('é' COLLATE "C"), upper('é' COLLATE "en-x-icu");
EXPLAIN (COSTS OFF) SELECT * FROM a75b2t WHERE 'a' < 'B' COLLATE "en-x-icu";
DROP VIEW a75b2v1, a75b2v0, a75b2w1, a75b2w0;
DROP FUNCTION a75b2f;
DROP TABLE a75b2t, a75b2u;
