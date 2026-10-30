-- PREPARE's parameter types keep only the type, not its modifier (PrepareQuery):
-- `numeric(5,2)` takes 123.456 whole, `varchar(3)` 'abcd' (difftest seeds 5008, 5010, 5025 ...).
PREPARE a104_p1(numeric(5,2), varchar(3), char(2), interval(0), bit(4), timestamp(0)) AS
  SELECT $1, $2, $3, $4, $5::text, $6, pg_typeof($1)::text, pg_typeof($2)::text;
EXECUTE a104_p1(123.456, 'abcd', 'abc', '1.6 sec', B'10', '2025-01-01 10:00:00.75');
EXECUTE a104_p1('999.999', 'ab   ', ' ', 'P0.5D', '101', NULL);
SELECT name, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = 'a104_p1';
DEALLOCATE a104_p1;
CREATE TABLE a104_pt (n numeric(5,2), v varchar(3));
PREPARE a104_p2(numeric(10,4), text) AS INSERT INTO a104_pt VALUES ($1, $2) RETURNING *;
EXECUTE a104_p2(1.23456, 'abc');
EXECUTE a104_p2(1.23456, 'abcd');
PREPARE a104_p3(varchar(2)[]) AS SELECT $1, pg_typeof($1)::text;
EXECUTE a104_p3('{abcd,e}');
DEALLOCATE ALL;
DROP TABLE a104_pt;
