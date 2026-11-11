-- Found by the differential tester (tests/difftest.rs). A FULL JOIN's ON clauses must be merge-
-- or hash-joinable: a NULLIF, CASE, COALESCE over the sides or a sub-select is neither.
CREATE TABLE a (x int, b jsonb, t text);
CREATE TABLE c (z int, b jsonb, t text);
INSERT INTO a VALUES (1, '{"k":1}', 'a'), (2, NULL, 'b');
INSERT INTO c VALUES (1, '{"k":1}', 'a'), (3, '[]', NULL);
SELECT * FROM a FULL JOIN c ON NULLIF(c.b ? 'k', false);
SELECT * FROM a FULL JOIN c ON CASE WHEN a.x > 0 THEN true END;
SELECT * FROM a FULL JOIN c ON COALESCE(a.x = c.z, false);
SELECT * FROM a FULL JOIN c ON a.x = c.z AND NULLIF(c.b ? 'k', false);
SELECT * FROM a FULL JOIN c ON (SELECT a.x = c.z);
SELECT * FROM a FULL JOIN c ON NULLIF(true, false);
SELECT * FROM a FULL JOIN c ON (SELECT true);
SELECT * FROM a FULL JOIN c ON a.x = c.z AND (SELECT true);
SELECT * FROM a FULL JOIN c ON a.x = c.z AND random() < 2;
SELECT * FROM a FULL JOIN c ON a.t = c.t COLLATE "C";
SELECT count(*) FROM a FULL JOIN c ON (a.x = c.z) IS TRUE;
SELECT count(*) FROM a FULL JOIN c ON a.x::text = c.z::text;
SELECT count(*) FROM a FULL JOIN c ON a.x = c.z + 0;
SELECT count(*) FROM a FULL JOIN c ON a.x + c.z = 2;
SELECT count(*) FROM a FULL JOIN c ON a.b = c.b;
SELECT count(*) FROM a FULL JOIN c ON a.x = c.z OR a.x = 2;
SELECT array_remove('[0:1]={-2,-1}'::int4[], 5), array_remove('[0:1]={-2,-1}'::int4[], -2), array_remove('[0:1]={-2,-2}'::int4[], -2), array_replace('[3:4]={1,2}'::int4[], 1, 0);
CREATE TABLE ua (a int);
INSERT INTO ua VALUES (1);
SELECT 1 FROM (SELECT 1/0 AS c FROM ua UNION ALL SELECT 1) s;
SELECT count(*) FROM (SELECT a, 1/0 AS c FROM ua UNION ALL SELECT 1, 1) s;
SELECT a FROM (SELECT a, 1 AS c FROM ua UNION ALL SELECT 2, 3) s ORDER BY 1;
