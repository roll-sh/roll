-- A rolled-back DROP keeps the sequences it would have dropped: a table's rows aren't replaced
-- by the next inserts, a serial column's values go on, and so do a dropped sequence's and a
-- materialized view's.
CREATE TABLE a182_r (a int);
INSERT INTO a182_r SELECT generate_series(1, 5);
BEGIN;
DROP TABLE a182_r;
ROLLBACK;
INSERT INTO a182_r VALUES (100);
SELECT ctid, a FROM a182_r ORDER BY a;
CREATE TABLE a182_s (id serial, a int);
INSERT INTO a182_s (a) SELECT generate_series(1, 3);
BEGIN;
SAVEPOINT p;
DROP TABLE a182_s;
ROLLBACK TO p;
COMMIT;
INSERT INTO a182_s (a) VALUES (100);
SELECT ctid, id, a FROM a182_s ORDER BY id;
CREATE SEQUENCE a182_q;
SELECT nextval('a182_q'), nextval('a182_q');
BEGIN;
DROP SEQUENCE a182_q;
ROLLBACK;
SELECT nextval('a182_q');
CREATE MATERIALIZED VIEW a182_mv AS SELECT generate_series(1, 3) AS a;
BEGIN;
DROP MATERIALIZED VIEW a182_mv;
ROLLBACK;
SELECT a FROM a182_mv ORDER BY a;
-- A DROP that commits forgets them: the names start over.
DROP TABLE a182_r, a182_s;
DROP SEQUENCE a182_q;
CREATE TABLE a182_r (a int);
INSERT INTO a182_r VALUES (1);
SELECT ctid, a FROM a182_r;
CREATE SEQUENCE a182_q;
SELECT nextval('a182_q');
DROP TABLE a182_r;
DROP SEQUENCE a182_q;
DROP MATERIALIZED VIEW a182_mv;
