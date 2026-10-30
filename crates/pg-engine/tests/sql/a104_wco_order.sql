-- A view's WITH CHECK OPTION is checked after the row's unique keys (ExecInsert / ExecUpdate:
-- WCO_VIEW_CHECK after the index entries), so a duplicate key wins (difftest seed 5006 #54).
CREATE TABLE a104_wco (id int PRIMARY KEY, a int, CHECK (a > -100));
INSERT INTO a104_wco VALUES (1, 5), (2, 50);
CREATE VIEW a104_wcov AS SELECT id, a FROM a104_wco WHERE a < 10 WITH CHECK OPTION;
INSERT INTO a104_wcov VALUES (1, 20);
INSERT INTO a104_wcov VALUES (3, 20);
INSERT INTO a104_wcov VALUES (3, -200);
UPDATE a104_wcov SET id = 2, a = 30 WHERE id = 1;
UPDATE a104_wcov SET a = 30 WHERE id = 1;
INSERT INTO a104_wcov VALUES (1, 20) ON CONFLICT (id) DO UPDATE SET a = 40;
INSERT INTO a104_wcov VALUES (4, 1) RETURNING *;
SELECT * FROM a104_wco ORDER BY id;
DROP VIEW a104_wcov;
DROP TABLE a104_wco;
