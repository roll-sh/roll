-- ALTER COLUMN TYPE checks the conversion before what uses the column (a generated column,
-- a view): ATPrepAlterColumnType before ATExecAlterColumnType (difftest seeds 5020, 5042).
CREATE TABLE a104_at (id int PRIMARY KEY, a int, b text, g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) VIRTUAL);
INSERT INTO a104_at (id, a, b) VALUES (1, 2, 'x');
ALTER TABLE a104_at ALTER COLUMN b TYPE bigint;
ALTER TABLE a104_at ALTER COLUMN b TYPE varchar(3);
ALTER TABLE a104_at ALTER COLUMN a TYPE point;
ALTER TABLE a104_at ALTER COLUMN a TYPE bigint;
ALTER TABLE a104_at ALTER COLUMN g1 TYPE int USING length(g1::text);
ALTER TABLE a104_at ALTER COLUMN g1 TYPE bigint;
CREATE VIEW a104_atv AS SELECT id, b FROM a104_at;
ALTER TABLE a104_at ALTER COLUMN id TYPE date;
ALTER TABLE a104_at ALTER COLUMN id TYPE bigint;
SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'a104_at'::regclass AND attnum > 0 ORDER BY attnum;
DROP VIEW a104_atv;
DROP TABLE a104_at;
