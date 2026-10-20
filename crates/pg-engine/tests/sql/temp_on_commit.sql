
CREATE TEMP TABLE temptest(col int) ON COMMIT DELETE ROWS;

CREATE INDEX ON temptest(bit_length(''));

BEGIN;
INSERT INTO temptest VALUES (1);
INSERT INTO temptest VALUES (2);

SELECT * FROM temptest;
COMMIT;

SELECT * FROM temptest;

DROP TABLE temptest;

BEGIN;
CREATE TEMP TABLE temptest(col) ON COMMIT DELETE ROWS AS SELECT 1;

SELECT * FROM temptest;
COMMIT;

SELECT * FROM temptest;

DROP TABLE temptest;


BEGIN;

CREATE TEMP TABLE temptest(col int) ON COMMIT DROP;

INSERT INTO temptest VALUES (1);
INSERT INTO temptest VALUES (2);

SELECT * FROM temptest;
COMMIT;

SELECT * FROM temptest;

BEGIN;
CREATE TEMP TABLE temptest(col) ON COMMIT DROP AS SELECT 1;

SELECT * FROM temptest;
COMMIT;

SELECT * FROM temptest;

BEGIN;
do $$
begin
  execute format($cmd$
    CREATE TEMP TABLE temptest (col text CHECK (col < %L)) ON COMMIT DROP
  $cmd$,
    (SELECT string_agg(g.i::text || ':' || random()::text, '|')
     FROM generate_series(1, 100) g(i)));
end$$;

SELECT * FROM temptest;
COMMIT;

SELECT * FROM temptest;


CREATE TABLE temptest(col int) ON COMMIT DELETE ROWS;
CREATE TABLE temptest(col) ON COMMIT DELETE ROWS AS SELECT 1;

BEGIN;
CREATE TEMP TABLE temptest1(col int PRIMARY KEY);
CREATE TEMP TABLE temptest2(col int REFERENCES temptest1)
  ON COMMIT DELETE ROWS;
INSERT INTO temptest1 VALUES (1);
INSERT INTO temptest2 VALUES (1);
COMMIT;
SELECT * FROM temptest1;
SELECT * FROM temptest2;
DROP TABLE temptest2; DROP TABLE temptest1;
WITH RECURSIVE t(n) AS (VALUES ('01'::varbit) UNION SELECT n || '10'::varbit FROM t WHERE n < '100'::varbit) SELECT n FROM t;
WITH RECURSIVE t(n) AS (VALUES ('01'::varbit) UNION ALL SELECT n || '10'::varbit FROM t WHERE length(n) < 7) SELECT n FROM t;
