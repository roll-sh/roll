-- A constant call under a COLLATE fails while planning, as eval_const_expressions folds it,
-- even over an empty table (difftest seed 3570 #67).
CREATE COLLATION a104_ci2 (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE a104_e (id int PRIMARY KEY, a text COLLATE a104_ci2);
SELECT starts_with('é' COLLATE a104_ci2, 'a') FROM a104_e;
SELECT 1 FROM a104_e WHERE starts_with('é' COLLATE a104_ci2, 'a') OR id = 1;
SELECT 1 FROM a104_e WHERE starts_with(CASE WHEN true THEN 'é' ELSE a104_e.a END, 'a');
SELECT 1 FROM a104_e WHERE starts_with(left(CASE WHEN true THEN 'é' ELSE a104_e.a END, 0), 'a');
SELECT 1 FROM a104_e WHERE EXISTS (SELECT 1 FROM a104_e x WHERE starts_with(left(CASE WHEN true THEN 'é' ELSE x.a END, 0), 'a') GROUP BY x.id);
SELECT 1 FROM a104_e WHERE starts_with('é' COLLATE "C", 'a');
SELECT 1 FROM a104_e WHERE strpos('é' COLLATE a104_ci2, 'a') = 1;
CREATE TABLE a104_m (id int PRIMARY KEY, c int);
MERGE INTO a104_m x USING (SELECT id FROM a104_e) s ON s.id = x.id WHEN MATCHED AND EXISTS (SELECT 1 FROM a104_e t WHERE starts_with(left(CASE WHEN true THEN 'é' ELSE t.a END, 0), 'a') GROUP BY t.id) THEN UPDATE SET c = 11 WHEN NOT MATCHED THEN INSERT VALUES (100, 1);
DROP TABLE a104_e, a104_m;
DROP COLLATION a104_ci2;
