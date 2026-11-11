-- Diesel's CI makes the databases with diesel_cli; PG_DATABASE_URL and PG_EXAMPLE_DATABASE_URL
-- name these two (diesel_cli's own tests make and drop theirs).
DROP DATABASE IF EXISTS diesel_test;
DROP DATABASE IF EXISTS diesel_example;
CREATE DATABASE diesel_test;
CREATE DATABASE diesel_example;
