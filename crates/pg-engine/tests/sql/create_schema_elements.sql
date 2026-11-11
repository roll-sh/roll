--
-- Regression tests for schemas (namespaces)
--

-- set the whitespace-only search_path to test that the
-- GUC list syntax is preserved during a schema creation
SELECT pg_catalog.set_config('search_path', ' ', false);

CREATE SCHEMA gx_ns_schema_1
       CREATE UNIQUE INDEX abc_a_idx ON abc (a)

       CREATE VIEW abc_view AS
              SELECT a+1 AS a, b+1 AS b FROM abc

       CREATE TABLE abc (
              a serial,
              b int UNIQUE
       );

-- verify that the correct search_path restored on abort
SET search_path to public;
BEGIN;
SET search_path to public, gx_ns_schema_1;
CREATE SCHEMA gx_ns_schema_2
       CREATE VIEW abc_view AS SELECT c FROM abc;
COMMIT;
SHOW search_path;

-- verify that the correct search_path preserved
-- after creating the schema and on commit
BEGIN;
SET search_path to public, gx_ns_schema_1;
CREATE SCHEMA gx_ns_schema_2
       CREATE VIEW abc_view AS SELECT a FROM abc;
SHOW search_path;
COMMIT;
SHOW search_path;
DROP SCHEMA gx_ns_schema_2 CASCADE;

-- verify that the objects were created
