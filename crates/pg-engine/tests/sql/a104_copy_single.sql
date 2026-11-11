-- COPY FROM into a table with a BEFORE ROW trigger (CopyFrom's CIM_SINGLE) inserts each row as
-- it is read: an earlier row's unique violation, or its trigger's notice, comes before a later
-- record's format error (difftest seeds 5021, 5058). Without such a trigger the rows are
-- buffered, and the format error comes first.
CREATE TABLE a104_cs (id int PRIMARY KEY, a int, b text);
INSERT INTO a104_cs VALUES (3, 1, 'x');
COPY a104_cs (id, a, b) FROM STDIN;
3	1	y
1
\.
CREATE FUNCTION a104_csf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'row %', NEW.id; RETURN NEW; END $$;
CREATE TRIGGER a104_cst BEFORE INSERT ON a104_cs FOR EACH ROW EXECUTE FUNCTION a104_csf();
COPY a104_cs (id, a, b) FROM STDIN;
3	1	y
1
\.
COPY a104_cs (id, a, b) FROM STDIN;
4	1	y
5	x	z
\.
COPY a104_cs (id, a, b) FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose);
8	x	a
9	1	b
1
\.
SELECT * FROM a104_cs ORDER BY id;
DROP TABLE a104_cs;
DROP FUNCTION a104_csf();
