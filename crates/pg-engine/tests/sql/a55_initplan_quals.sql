-- order_qual_clauses: a sub-select reading no column of the scanned level is an InitPlan
-- whose output Param costs nothing per row, so its comparison goes before costlier quals;
-- a correlated one (a SubPlan run per row) goes last.
CREATE TABLE a55_c (id int, bal numeric, phone text);
INSERT INTO a55_c SELECT g, g % 100 - 20, lpad((g % 40)::text, 2, '1') || '-555' FROM generate_series(1, 500) g;
CREATE TABLE a55_o (cid int);
INSERT INTO a55_o SELECT g * 3 FROM generate_series(1, 100) g;
ANALYZE a55_c, a55_o;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a55_c
WHERE substring(phone FROM 1 FOR 2) IN ('13', '31', '23', '29', '30', '18', '17')
  AND bal > (SELECT avg(bal) FROM a55_c WHERE bal > 0);
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a55_c c
WHERE substring(phone FROM 1 FOR 2) IN ('13', '31', '23')
  AND EXISTS (SELECT 1 FROM a55_o WHERE cid > 250)
  AND bal > (SELECT max(o.cid) - 290 FROM a55_o o WHERE o.cid < c.id);
SELECT count(*) FROM a55_c
WHERE substring(phone FROM 1 FOR 2) IN ('13', '31', '23', '29', '30', '18', '17')
  AND bal > (SELECT avg(bal) FROM a55_c WHERE bal > 0);
SELECT count(*) FROM a55_c c
WHERE substring(phone FROM 1 FOR 2) IN ('13', '31', '23')
  AND EXISTS (SELECT 1 FROM a55_o WHERE cid > 250)
  AND bal > (SELECT max(o.cid) - 290 FROM a55_o o WHERE o.cid < c.id);
DROP TABLE a55_c, a55_o;
