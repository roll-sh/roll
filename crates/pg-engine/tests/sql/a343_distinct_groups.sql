-- A DISTINCT over computed columns estimates its groups as estimate_num_groups does: the
-- Vars the expressions read (each once), a boolean 2 values, a constant none; so a hashed
-- DISTINCT's table is sized as PostgreSQL's and its rows come out in its order.
CREATE TABLE a343_d (id int NOT NULL, a int NOT NULL);
INSERT INTO a343_d SELECT g, g * 7 % 1009 FROM generate_series(1, 2000) g;
ANALYZE a343_d;
EXPLAIN SELECT DISTINCT a % 100 FROM a343_d;
EXPLAIN SELECT DISTINCT a % 100, id % 7 FROM a343_d;
EXPLAIN SELECT DISTINCT a % 100, a % 7 FROM a343_d;
EXPLAIN SELECT DISTINCT id % 10 > 3 FROM a343_d;
EXPLAIN SELECT DISTINCT 1, a % 3 FROM a343_d;
SELECT DISTINCT a % 10, id % 3 FROM a343_d;
DROP TABLE a343_d;
