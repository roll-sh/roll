-- Error order (agent 118): which of two errors PostgreSQL raises first.
-- COPY FROM one row at a time: an unterminated CSV quoted field after the rows before it went in.
CREATE TABLE eo (id int PRIMARY KEY, b text);
CREATE FUNCTION eo_tr() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'row %', NEW.id; RETURN NEW; END $$;
CREATE TRIGGER eo_b BEFORE INSERT ON eo FOR EACH ROW EXECUTE FUNCTION eo_tr();
INSERT INTO eo VALUES (2, 'x');
COPY eo FROM STDIN (FORMAT csv);
1,a
3,"b
\.
COPY eo FROM STDIN (FORMAT csv);
1,a
2,b
3,"c
\.
SELECT * FROM eo ORDER BY id;
DROP TRIGGER eo_b ON eo;
COPY eo FROM STDIN (FORMAT csv);
5,a
6,"b
\.
SELECT * FROM eo ORDER BY id;
-- INSERT into a view column that can't be written, with fewer values than columns named.
CREATE VIEW eov AS SELECT id, b, id + 1 AS c FROM eo;
INSERT INTO eov (id, c) VALUES (7);
INSERT INTO eov (id, c, b) VALUES (7, 1);
INSERT INTO eov (id, c) VALUES (7, 1);
INSERT INTO eov (c, id) VALUES (7);
-- ALTER COLUMN TYPE: the index made anew over the new type before any row converts.
CREATE TABLE eog (id int, r int4range);
INSERT INTO eog VALUES (1, '[1,3)');
CREATE INDEX eog_r ON eog USING gist (r);
ALTER TABLE eog ALTER COLUMN r TYPE int USING lower(r) / 0;
ALTER TABLE eog ALTER COLUMN r TYPE int4multirange USING 'x';
ALTER TABLE eog ALTER COLUMN r TYPE text;
SELECT * FROM eog;
-- COPY: the relation and its column list before the options; COPY TO refuses a view first.
CREATE VIEW eocv AS SELECT * FROM eo;
COPY eo (id, nosuch) TO STDOUT (FORMAT csv, FORCE_NOT_NULL (b));
COPY eo (id, id) TO STDOUT (FORMAT csv, FORCE_NOT_NULL (b));
COPY eocv TO STDOUT (FORMAT csv, FORCE_NOT_NULL (b));
COPY eocv (id) TO STDOUT (FORMAT text, HEADER match);
COPY eo (nosuch) TO STDOUT (DELIMITER 'ab');
COPY eocv (nosuch) TO STDOUT;
COPY eocv TO STDOUT (FORMAT binary, HEADER);
COPY eo (nosuch) FROM STDIN (FORMAT binary, HEADER);
COPY nosuch TO STDOUT (FORMAT binary, HEADER);
COPY (SELECT 1) TO STDOUT (FORMAT binary, HEADER);
