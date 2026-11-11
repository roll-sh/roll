-- MERGE ... INSERT VALUES (s.*): the source row's fields (transformExpressionList)
CREATE TABLE mvs_t (id int PRIMARY KEY, a int, b text);
CREATE TABLE mvs_s (id int, a int, b text);
INSERT INTO mvs_s VALUES (1, 10, 'x'), (2, 20, 'y');
INSERT INTO mvs_t VALUES (1, 0, 'old');
MERGE INTO mvs_t t USING mvs_s s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET a = s.a
  WHEN NOT MATCHED THEN INSERT VALUES (s.*);
SELECT * FROM mvs_t ORDER BY id;
MERGE INTO mvs_t t USING (SELECT id + 10 AS id, a FROM mvs_s) s ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT VALUES (s.*, 'z');
SELECT * FROM mvs_t ORDER BY id;
MERGE INTO mvs_t t USING mvs_s s ON t.id = s.id + 100
  WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.*);
MERGE INTO mvs_t t USING mvs_s s ON t.id = s.id + 100
  WHEN NOT MATCHED THEN INSERT VALUES (s.*, 1);
MERGE INTO mvs_t t USING (SELECT id + 200 AS id, a FROM mvs_s) s ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.*);
DROP TABLE mvs_t, mvs_s;
-- a rewrite of a user catalog table (ATRewriteTables)
CREATE TABLE mvs_cat (id int, x int) WITH (user_catalog_table = true);
ALTER TABLE mvs_cat ALTER COLUMN x TYPE text;
ALTER TABLE mvs_cat ADD COLUMN y int DEFAULT random();
ALTER TABLE mvs_cat ADD COLUMN z int DEFAULT 1;
ALTER TABLE mvs_cat SET (user_catalog_table = false);
ALTER TABLE mvs_cat ALTER COLUMN x TYPE text;
DROP TABLE mvs_cat;
