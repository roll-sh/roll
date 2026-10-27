-- efcore.pg's CI: a superuser npgsql_tests (its tests create and drop their own databases,
-- extensions and types).
DROP ROLE IF EXISTS npgsql_tests;
CREATE USER npgsql_tests SUPERUSER PASSWORD 'npgsql_tests';
