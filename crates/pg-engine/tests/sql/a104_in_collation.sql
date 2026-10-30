-- An IN list's values compare under the collation the left side and all the values derive
-- together (transformAExprIn's array): an explicit COLLATE on any value counts for all
-- (difftest seed 5082 #94).
CREATE COLLATION a104_ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false);
CREATE TABLE a104_in (id int PRIMARY KEY, b text COLLATE a104_ai);
INSERT INTO a104_in VALUES (1, 'Z'), (3, 'ö'), (9, 'ss'), (11, 'E'), (12, 'é'), (13, 'a');
SELECT id, b FROM a104_in WHERE b IN ('E', 'A' COLLATE "de-x-icu") ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b IN ('E', 'A' COLLATE "de-x-icu") OR b > 'E' ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b NOT IN ('E', 'A' COLLATE "de-x-icu") ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b IN ('E' COLLATE "C", 'a') ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b IN ('E', 'a') ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b IN ('E', 'A' COLLATE "de-x-icu", b) ORDER BY b, id;
SELECT id, b FROM a104_in WHERE b IN (upper('e'), 'A' COLLATE "de-x-icu") ORDER BY b, id;
SELECT x.id, y.id FROM a104_in x JOIN a104_in y ON x.b IN (y.b, 'e' COLLATE "C") WHERE x.id < y.id ORDER BY 1, 2;
SELECT id FROM a104_in WHERE b IN ('E' COLLATE "C", 'a' COLLATE "POSIX");
DROP TABLE a104_in;
DROP COLLATION a104_ai;
