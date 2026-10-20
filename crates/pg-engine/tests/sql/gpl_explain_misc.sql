-- EXPLAIN details: SETTINGS, a temporary schema's function qualified; a join through a
-- security-barrier view pulled up from a subquery.
CREATE TABLE gpl_cu (cid int PRIMARY KEY, name text NOT NULL);
CREATE TABLE gpl_cc (cid int REFERENCES gpl_cu (cid), cnum text);
CREATE TABLE gpl_us (cid int REFERENCES gpl_cu (cid), ymd date, usage int);
INSERT INTO gpl_cu VALUES (101, 'alice'), (102, 'bob'), (103, 'eve');
INSERT INTO gpl_cc VALUES (101, '1111'), (102, '5555'), (103, '9801');
INSERT INTO gpl_us VALUES (101, '2011-09-15', 120), (101, '2011-10-05', 90), (102, '2011-10-12', 120), (103, '2011-10-15', 480);
CREATE VIEW gpl_sec WITH (security_barrier) AS SELECT * FROM gpl_cu l NATURAL JOIN gpl_cc r WHERE l.name <> 'x';
SELECT * FROM (SELECT * FROM gpl_sec l NATURAL JOIN gpl_us r) v WHERE ymd >= '2011-10-01' AND ymd < '2011-11-01' ORDER BY 1, ymd;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF, SETTINGS) SELECT * FROM gpl_cu WHERE cid = 101;
RESET enable_seqscan;
EXPLAIN (SETTINGS, COSTS OFF, FORMAT JSON) SELECT 1;
CREATE FUNCTION pg_temp.gpl_f(int) RETURNS int LANGUAGE plpgsql AS 'begin return $1; end';
CREATE TEMP TABLE gpl_t (f1 int);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gpl_t WHERE pg_temp.gpl_f(f1) < 5;
