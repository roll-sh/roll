-- Npgsql's CI: user and database npgsql_tests (a superuser, as its tests create types,
-- extensions and roles).
DROP DATABASE IF EXISTS npgsql_tests;
DROP ROLE IF EXISTS npgsql_tests;
CREATE USER npgsql_tests SUPERUSER PASSWORD 'npgsql_tests';
CREATE DATABASE npgsql_tests OWNER npgsql_tests;
